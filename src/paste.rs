use std::ffi::{c_char, c_void};
use std::ptr;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use std::time::Instant;

use color_eyre::eyre::{Result, eyre};
use objc2::rc::Retained;
#[cfg(test)]
use objc2::runtime::ProtocolObject;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
#[cfg(test)]
use objc2_app_kit::{NSPasteboardItem, NSPasteboardWriting};
#[cfg(test)]
use objc2_foundation::NSArray;
use objc2_foundation::NSString;

use crate::keyboard;
use crate::post_processing::Preferences as PostProcessing;
use crate::suppression::InputActivity;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PasteOutcome {
    Pasted,
    Deferred,
    CopiedToClipboard,
}

/// Settle delay before the pre-paste clipboard is restored; the transcript
/// is readable by same-login apps until then.
const PASTE_RESTORE_DELAY: Duration = Duration::from_millis(250);

/// Caps for the pre-paste clipboard snapshot. The pasteboard is user- and
/// app-controlled, so neither its shape nor its size is trusted: a hostile
/// item must not balloon Hex's memory or stall the paste.
const MAX_CLIPBOARD_ITEMS: usize = 16;
const MAX_FLAVOR_BYTES: usize = 8 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
const CAPTURE_DEADLINE: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PasteOptions {
    pub submit_after_paste: Option<u64>,
    pub post_processing: PostProcessing,
    pub preserve_name_case: bool,
}

pub struct Paster {
    clipboard: Retained<NSPasteboard>,
    activity: InputActivity,
    continuation: Option<Continuation>,
    clipboard_restore: Arc<Mutex<ClipboardRestore>>,
    prepared_clipboard: Option<PreparedClipboard>,
}

struct PreparedClipboard {
    change_count: isize,
    snapshot: ClipboardSnapshot,
    capture_ms: u128,
}

struct Continuation {
    revision: u64,
    target: Option<crate::context::ForegroundApplication>,
    inserted: String,
}

impl Continuation {
    fn applies_to(&self, revision: u64, target: &crate::context::ContextSnapshot) -> bool {
        self.revision == revision && self.target.is_some() && self.target == target.target
    }
}

#[derive(Default)]
struct ClipboardRestore {
    generation: u64,
    original: Option<ClipboardSnapshot>,
    last_change_count: Option<isize>,
}

impl ClipboardRestore {
    fn forget(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.original = None;
        self.last_change_count = None;
    }

    fn register(
        &mut self,
        previous: ClipboardSnapshot,
        previous_change_count: isize,
        inserted_change_count: isize,
    ) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        if self.last_change_count != Some(previous_change_count) {
            self.original = Some(previous);
        }
        self.last_change_count = Some(inserted_change_count);
        self.generation
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ClipboardSnapshot {
    items: Vec<Vec<ClipboardFlavor>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ClipboardFlavor {
    data_type: String,
    data: Vec<u8>,
    flags: u32,
}

type PasteboardRef = *const c_void;
type PasteboardItemId = *mut c_void;
type CfTypeRef = *const c_void;
type CfStringRef = *const c_void;
type CfArrayRef = *const c_void;
type CfDataRef = *const c_void;

const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const BAD_PASTEBOARD_FLAVOR: i32 = -25133;
const DUPLICATE_PASTEBOARD_FLAVOR: i32 = -25134;
const PASTE_SETTLE_DELAY: Duration = Duration::from_millis(100);
const SYSTEM_TRANSLATED_FLAVOR: u32 = 1 << 8;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn PasteboardCreate(name: CfStringRef, pasteboard: *mut PasteboardRef) -> i32;
    fn PasteboardSynchronize(pasteboard: PasteboardRef) -> u32;
    fn PasteboardClear(pasteboard: PasteboardRef) -> i32;
    fn PasteboardGetItemCount(pasteboard: PasteboardRef, count: *mut usize) -> i32;
    fn PasteboardGetItemIdentifier(
        pasteboard: PasteboardRef,
        index: isize,
        item: *mut PasteboardItemId,
    ) -> i32;
    fn PasteboardCopyItemFlavors(
        pasteboard: PasteboardRef,
        item: PasteboardItemId,
        flavors: *mut CfArrayRef,
    ) -> i32;
    fn PasteboardGetItemFlavorFlags(
        pasteboard: PasteboardRef,
        item: PasteboardItemId,
        flavor: CfStringRef,
        flags: *mut u32,
    ) -> i32;
    fn PasteboardCopyItemFlavorData(
        pasteboard: PasteboardRef,
        item: PasteboardItemId,
        flavor: CfStringRef,
        data: *mut CfDataRef,
    ) -> i32;
    fn PasteboardPutItemFlavor(
        pasteboard: PasteboardRef,
        item: PasteboardItemId,
        flavor: CfStringRef,
        data: CfDataRef,
        flags: u32,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: CfArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfArrayRef, index: isize) -> *const c_void;
    fn CFDataCreate(allocator: *const c_void, bytes: *const u8, length: isize) -> CfDataRef;
    fn CFDataGetBytePtr(data: CfDataRef) -> *const u8;
    fn CFDataGetLength(data: CfDataRef) -> isize;
    fn CFStringGetLength(value: CfStringRef) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        value: CfStringRef,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;
    fn CFStringCreateWithBytes(
        allocator: *const c_void,
        bytes: *const u8,
        length: isize,
        encoding: u32,
        is_external_representation: u8,
    ) -> CfStringRef;
    fn CFRelease(value: CfTypeRef);
}

struct PasteboardHandle(PasteboardRef);

impl Drop for PasteboardHandle {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

impl Paster {
    pub fn new(activity: InputActivity) -> Self {
        Self {
            clipboard: NSPasteboard::generalPasteboard(),
            activity,
            continuation: None,
            clipboard_restore: Arc::new(Mutex::new(ClipboardRestore::default())),
            prepared_clipboard: None,
        }
    }

    pub fn prepare(&mut self) {
        let change_count = self.clipboard.changeCount();
        if self
            .prepared_clipboard
            .as_ref()
            .is_some_and(|prepared| prepared.change_count == change_count)
        {
            return;
        }
        self.prepared_clipboard = None;
        let _restore = self
            .clipboard_restore
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let started = Instant::now();
        match capture_clipboard(&self.clipboard) {
            Ok(snapshot) => {
                self.prepared_clipboard = Some(PreparedClipboard {
                    change_count,
                    snapshot,
                    capture_ms: started.elapsed().as_millis(),
                });
            }
            Err(error) => tracing::debug!(%error, "eager clipboard capture failed"),
        }
    }

    /// Commit only after clipboard preparation, immediately before the first write.
    /// A rejected commit leaves both the clipboard and continuation unchanged.
    pub fn paste(
        &mut self,
        text: &str,
        target: &crate::context::ContextSnapshot,
        options: PasteOptions,
        commit: impl Fn() -> bool,
    ) -> Result<PasteOutcome> {
        let enabled = crate::app_settings::copy_on_paste_failure();
        let outcome = self.paste_attempt(text, target, options, &commit);
        fallback_after_attempt(outcome, enabled, &commit, || self.copy_fallback(text))
    }

    fn copy_fallback(&mut self, text: &str) -> Result<()> {
        let mut restore = self
            .clipboard_restore
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // This opt-in copy replaces the clipboard. Preserve it if possible so
        // a failed write can still be rolled back. No key event is posted.
        let previous_count = self.clipboard.changeCount();
        let previous = capture_clipboard(&self.clipboard).ok();
        if self.clipboard.changeCount() != previous_count {
            return Err(eyre!("clipboard changed while preparing the fallback copy"));
        }
        if let Err(error) = write_clipboard_text(&self.clipboard, text) {
            if let Some(previous) = previous
                && restore_clipboard(&self.clipboard, &previous).is_ok()
                && restore.last_change_count == Some(previous_count)
            {
                restore.last_change_count = Some(self.clipboard.changeCount());
            }
            return Err(error);
        }
        // A restore scheduled by the failed paste must never erase this copy.
        restore.forget();
        self.prepared_clipboard = None;
        self.continuation = None;
        Ok(())
    }

    fn paste_attempt(
        &mut self,
        text: &str,
        target: &crate::context::ContextSnapshot,
        options: PasteOptions,
        commit: impl Fn() -> bool,
    ) -> Result<PasteOutcome> {
        let submit_after_paste = options.submit_after_paste;
        let revision = self.activity.revision();
        let text = self
            .continuation
            .as_ref()
            .filter(|continuation| continuation.applies_to(revision, target))
            .map_or_else(
                || text.to_string(),
                |continuation| {
                    join_with_case_policy(
                        &continuation.inserted,
                        text,
                        options.post_processing.controls_initial_case()
                            || options.preserve_name_case,
                    )
                },
            );
        let generation = commit_targeted_paste(
            || {
                let restore = self
                    .clipboard_restore
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let previous_change_count = self.clipboard.changeCount();
                let prepared = self.prepared_clipboard.take();
                let previous = if let Some(prepared) = prepared
                    && prepared.change_count == previous_change_count
                {
                    tracing::debug!(
                        clipboard_capture_ms = prepared.capture_ms,
                        "used eagerly captured clipboard"
                    );
                    prepared.snapshot
                } else {
                    let started = Instant::now();
                    let snapshot = capture_clipboard(&self.clipboard)?;
                    tracing::debug!(
                        clipboard_capture_ms = started.elapsed().as_millis(),
                        "captured clipboard at paste time"
                    );
                    snapshot
                };
                if self.clipboard.changeCount() != previous_change_count {
                    return Err(eyre!("clipboard changed while Endu was preserving it"));
                }
                Ok((restore, previous, previous_change_count))
            },
            &commit,
            || {
                if submit_after_paste
                    .is_some_and(|expected| expected != self.activity.interaction_revision())
                {
                    return None;
                }
                target
                    .target
                    .as_ref()
                    .and_then(|target| target.current_process_id())
            },
            |(mut restore, previous, previous_change_count), pid| {
                if let Err(error) = write_clipboard_text(&self.clipboard, &text) {
                    if let Err(restore_error) = restore_clipboard(&self.clipboard, &previous) {
                        tracing::error!(%restore_error, "could not recover the clipboard after a failed write");
                    }
                    return Err(error);
                }
                let inserted_change_count = self.clipboard.changeCount();
                Ok((
                    restore.register(previous, previous_change_count, inserted_change_count),
                    pid,
                ))
            },
        )?;

        let Some((generation, pid)) = generation else {
            return Ok(PasteOutcome::Deferred);
        };
        let clipboard_restore = self.clipboard_restore.clone();
        let submitted = complete_paste_with_submit(
            || keyboard::post_command_to_pid('v', pid),
            move || {
                thread::spawn(move || {
                    // The transcript sits on the shared pasteboard until the
                    // restore runs, so keep that window as short as the target
                    // app's paste handling allows.
                    thread::sleep(PASTE_RESTORE_DELAY);
                    let mut restore = clipboard_restore
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if restore.generation != generation {
                        return;
                    }
                    let clipboard = NSPasteboard::generalPasteboard();
                    if restore.last_change_count != Some(clipboard.changeCount()) {
                        restore.original = None;
                        restore.last_change_count = None;
                        return;
                    }
                    let Some(previous) = restore.original.take() else {
                        return;
                    };
                    let result = restore_clipboard(&clipboard, &previous);
                    restore.last_change_count = None;
                    if let Err(error) = result {
                        tracing::warn!(%error, "could not restore clipboard after paste");
                    }
                });
            },
            thread::sleep,
            || {
                submit_if_current(
                    submit_after_paste,
                    &commit,
                    || self.activity.interaction_revision(),
                    || {
                        target
                            .target
                            .as_ref()
                            .and_then(|target| target.current_process_id())
                            == Some(pid)
                    },
                    || keyboard::post_return_to_pid(pid),
                )
            },
        )?;
        self.continuation = (!submitted).then_some(Continuation {
            revision,
            target: target.target.clone(),
            inserted: text,
        });
        Ok(PasteOutcome::Pasted)
    }
}

fn fallback_after_attempt(
    outcome: Result<PasteOutcome>,
    enabled: bool,
    commit: &dyn Fn() -> bool,
    copy: impl FnOnce() -> Result<()>,
) -> Result<PasteOutcome> {
    if !enabled
        || matches!(
            outcome,
            Ok(PasteOutcome::Pasted | PasteOutcome::CopiedToClipboard)
        )
    {
        return outcome;
    }
    // Clipboard preparation can fail before the original output commit. Check
    // again here so cancellation/shutdown can still win before the fallback.
    if !commit() {
        return Err(eyre!("paste was cancelled"));
    }
    if let Err(error) = &outcome {
        tracing::warn!(%error, "automatic paste failed; copying the transcript instead");
    }
    copy().map_err(|error| {
        error.wrap_err("could not copy the transcript after automatic paste failed")
    })?;
    Ok(PasteOutcome::CopiedToClipboard)
}

pub(crate) fn commit_prepared_paste<P, T>(
    prepare: impl FnOnce() -> Result<P>,
    commit: impl FnOnce() -> bool,
    paste: impl FnOnce(P) -> Result<T>,
) -> Result<T> {
    let prepared = prepare()?;
    // Waiting for restoration and materializing promised clipboard data remain cancellable.
    // Once committed, even a failed first write may have changed the clipboard.
    if !commit() {
        return Err(eyre!("paste was cancelled"));
    }
    paste(prepared)
}

fn commit_targeted_paste<P, T>(
    prepare: impl FnOnce() -> Result<P>,
    commit: impl FnOnce() -> bool,
    target: impl FnOnce() -> Option<i32>,
    paste: impl FnOnce(P, i32) -> Result<T>,
) -> Result<Option<T>> {
    commit_prepared_paste(prepare, commit, |prepared| {
        target().map(|pid| paste(prepared, pid)).transpose()
    })
}

#[cfg(test)]
fn complete_paste(
    post_paste: impl FnOnce() -> Result<()>,
    schedule_restore: impl FnOnce(),
    wait: impl FnOnce(Duration),
) -> Result<()> {
    complete_paste_with_submit(post_paste, schedule_restore, wait, || false).map(|_| ())
}

fn complete_paste_with_submit(
    post_paste: impl FnOnce() -> Result<()>,
    schedule_restore: impl FnOnce(),
    wait: impl FnOnce(Duration),
    submit: impl FnOnce() -> bool,
) -> Result<bool> {
    let posted = post_paste();
    schedule_restore();
    posted?;
    // Keep the clipboard stable for the target's 100 ms consumption window.
    // This is not an acknowledgment; longer OS or application stalls can still lose a paste.
    wait(PASTE_SETTLE_DELAY);
    Ok(submit())
}

fn submit_if_current(
    intent: Option<u64>,
    output_allowed: impl FnOnce() -> bool,
    revision: impl FnOnce() -> u64,
    destination_is_current: impl FnOnce() -> bool,
    post: impl FnOnce() -> Result<()>,
) -> bool {
    let Some(expected) = intent else {
        return false;
    };
    if !output_allowed() || revision() != expected || !destination_is_current() {
        tracing::info!(
            "Return skipped after paste because output stopped, the destination changed, or user input intervened"
        );
        return false;
    }
    match post() {
        Ok(()) => true,
        Err(error) => {
            // Text was inserted, so retain normal paste/History success even if
            // Return could not be posted. Never repeat this through Paste Last.
            tracing::warn!(%error, "text pasted but Return could not be posted");
            false
        }
    }
}

fn capture_clipboard(clipboard: &NSPasteboard) -> Result<ClipboardSnapshot> {
    let capture_started = Instant::now();
    let capture_deadline = capture_started + CAPTURE_DEADLINE;
    let mut snapshot_bytes = 0usize;
    let pasteboard = create_pasteboard(&clipboard.name())?;
    unsafe { PasteboardSynchronize(pasteboard.0) };
    let mut item_count = 0;
    check_status(
        unsafe { PasteboardGetItemCount(pasteboard.0, &mut item_count) },
        "count clipboard items",
    )?;
    if item_count > MAX_CLIPBOARD_ITEMS {
        return Err(eyre!(
            "clipboard holds {item_count} items; refusing to snapshot more than {MAX_CLIPBOARD_ITEMS}"
        ));
    }
    let mut items = Vec::with_capacity(item_count);
    for index in 1..=item_count {
        let mut item = ptr::null_mut();
        check_status(
            unsafe { PasteboardGetItemIdentifier(pasteboard.0, index as isize, &mut item) },
            "identify a clipboard item",
        )?;
        let mut flavors = ptr::null();
        check_status(
            unsafe { PasteboardCopyItemFlavors(pasteboard.0, item, &mut flavors) },
            "list clipboard formats",
        )?;
        if flavors.is_null() {
            return Err(eyre!("could not list clipboard formats"));
        }
        let flavor_count = unsafe { CFArrayGetCount(flavors) };
        let has_authoritative_flavor = (0..flavor_count).any(|flavor_index| {
            let flavor = unsafe { CFArrayGetValueAtIndex(flavors, flavor_index) };
            if flavor.is_null() {
                return false;
            }
            let mut flags = 0;
            (unsafe { PasteboardGetItemFlavorFlags(pasteboard.0, item, flavor, &mut flags) }) == 0
                && flags & SYSTEM_TRANSLATED_FLAVOR == 0
        });
        let result = (0..flavor_count)
            .map(|flavor_index| {
                let flavor = unsafe { CFArrayGetValueAtIndex(flavors, flavor_index) };
                if flavor.is_null() {
                    return Err(eyre!("clipboard format was unavailable"));
                }
                let data_type = cf_string(flavor)?;
                let mut flags = 0;
                check_status(
                    unsafe { PasteboardGetItemFlavorFlags(pasteboard.0, item, flavor, &mut flags) },
                    "inspect clipboard format",
                )?;
                if !should_preserve_flavor(flags, has_authoritative_flavor) {
                    tracing::debug!(%data_type, "skipping synthesized clipboard format");
                    return Ok(None);
                }
                let mut data = ptr::null();
                let flavor_started = Instant::now();
                let status =
                    unsafe { PasteboardCopyItemFlavorData(pasteboard.0, item, flavor, &mut data) };
                let flavor_ms = flavor_started.elapsed().as_millis();
                if flavor_ms >= 100 {
                    tracing::warn!(%data_type, flavor_ms, "clipboard format was slow to materialize");
                }
                if status == BAD_PASTEBOARD_FLAVOR {
                    tracing::debug!(%data_type, "skipping unavailable clipboard format");
                    return Ok(None);
                }
                check_status(status, "preserve clipboard format")?;
                if data.is_null() {
                    return Err(eyre!("clipboard format data was unavailable"));
                }
                let bytes = cf_data(data)?;
                unsafe { CFRelease(data) };
                snapshot_bytes += bytes.len();
                if bytes.len() > MAX_FLAVOR_BYTES || snapshot_bytes > MAX_SNAPSHOT_BYTES {
                    tracing::warn!(
                        %data_type,
                        bytes = bytes.len(),
                        "clipboard format exceeded the preservation cap"
                    );
                    return Ok(None);
                }
                if Instant::now() >= capture_deadline {
                    return Err(eyre!("clipboard snapshot exceeded its deadline"));
                }
                Ok(Some(ClipboardFlavor {
                    data_type,
                    data: bytes,
                    flags: flags & 0x0f,
                }))
            })
            .collect::<Result<Vec<_>>>();
        unsafe { CFRelease(flavors) };
        items.push(result?.into_iter().flatten().collect());
    }
    let capture_ms = capture_started.elapsed().as_millis();
    if capture_ms >= 100 {
        tracing::warn!(capture_ms, item_count, "clipboard snapshot was slow");
    }
    Ok(ClipboardSnapshot { items })
}

fn should_preserve_flavor(flags: u32, has_authoritative_flavor: bool) -> bool {
    !has_authoritative_flavor || flags & SYSTEM_TRANSLATED_FLAVOR == 0
}

fn write_clipboard_text(clipboard: &NSPasteboard, text: &str) -> Result<()> {
    clipboard.clearContents();
    let text = NSString::from_str(text);
    let data_type = unsafe { NSPasteboardTypeString };
    clipboard
        .setString_forType(&text, data_type)
        .then_some(())
        .ok_or_else(|| eyre!("could not write the transcript to the clipboard"))
}

fn restore_clipboard(clipboard: &NSPasteboard, snapshot: &ClipboardSnapshot) -> Result<()> {
    let pasteboard = create_pasteboard(&clipboard.name())?;
    unsafe { PasteboardSynchronize(pasteboard.0) };
    check_status(
        unsafe { PasteboardClear(pasteboard.0) },
        "clear the clipboard",
    )?;
    for (item_index, item) in snapshot.items.iter().enumerate() {
        let item_id = (item_index + 1) as PasteboardItemId;
        for flavor in item {
            let data_type = cf_string_create(&flavor.data_type)?;
            let data = unsafe {
                CFDataCreate(
                    ptr::null(),
                    flavor.data.as_ptr(),
                    flavor.data.len() as isize,
                )
            };
            if data.is_null() {
                unsafe { CFRelease(data_type) };
                return Err(eyre!("could not encode a clipboard format"));
            }
            let status = unsafe {
                PasteboardPutItemFlavor(pasteboard.0, item_id, data_type, data, flavor.flags)
            };
            unsafe {
                CFRelease(data);
                CFRelease(data_type);
            }
            if status == DUPLICATE_PASTEBOARD_FLAVOR {
                tracing::debug!(data_type = %flavor.data_type, "skipping synthesized clipboard format");
                continue;
            }
            check_status(status, "restore clipboard format")?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn write_items(clipboard: &NSPasteboard, items: Vec<Retained<NSPasteboardItem>>) -> Result<()> {
    let objects = items
        .iter()
        .map(|item| ProtocolObject::from_ref(&**item))
        .collect::<Vec<&ProtocolObject<dyn NSPasteboardWriting>>>();
    let objects = NSArray::from_slice(&objects);
    clipboard
        .writeObjects(&objects)
        .then_some(())
        .ok_or_else(|| eyre!("could not restore the clipboard"))
}

fn create_pasteboard(name: &NSString) -> Result<PasteboardHandle> {
    let mut pasteboard = ptr::null();
    check_status(
        unsafe { PasteboardCreate((name as *const NSString).cast(), &mut pasteboard) },
        "open the clipboard",
    )?;
    (!pasteboard.is_null())
        .then_some(PasteboardHandle(pasteboard))
        .ok_or_else(|| eyre!("could not open the clipboard"))
}

fn check_status(status: i32, action: &str) -> Result<()> {
    (status == 0)
        .then_some(())
        .ok_or_else(|| eyre!("could not {action} (status {status})"))
}

fn cf_string_create(value: &str) -> Result<CfStringRef> {
    let value = unsafe {
        CFStringCreateWithBytes(
            ptr::null(),
            value.as_ptr(),
            value.len() as isize,
            CF_STRING_ENCODING_UTF8,
            0,
        )
    };
    (!value.is_null())
        .then_some(value)
        .ok_or_else(|| eyre!("could not encode a clipboard format name"))
}

fn cf_string(value: CfStringRef) -> Result<String> {
    let length = unsafe { CFStringGetLength(value) };
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, CF_STRING_ENCODING_UTF8) }
        .saturating_add(1);
    let mut bytes = vec![0; capacity as usize];
    if !unsafe {
        CFStringGetCString(
            value,
            bytes.as_mut_ptr().cast(),
            capacity,
            CF_STRING_ENCODING_UTF8,
        )
    } {
        return Err(eyre!("could not decode a clipboard format name"));
    }
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8(bytes[..length].to_vec()).map_err(Into::into)
}

fn cf_data(value: CfDataRef) -> Result<Vec<u8>> {
    let length = unsafe { CFDataGetLength(value) };
    if length < 0 {
        return Err(eyre!("clipboard format had an invalid length"));
    }
    if length == 0 {
        return Ok(Vec::new());
    }
    let bytes = unsafe { CFDataGetBytePtr(value) };
    if bytes.is_null() {
        return Err(eyre!("clipboard format data was unavailable"));
    }
    Ok(unsafe { std::slice::from_raw_parts(bytes, length as usize) }.to_vec())
}

#[cfg(test)]
fn join(previous: &str, next: &str) -> String {
    join_with_preferences(previous, next, PostProcessing::default())
}

#[cfg(test)]
fn join_with_preferences(previous: &str, next: &str, preferences: PostProcessing) -> String {
    join_with_case_policy(previous, next, preferences.controls_initial_case())
}

fn join_with_case_policy(previous: &str, next: &str, preserve_case: bool) -> String {
    let sentence_start = ends_sentence(previous);
    let mut next = if preserve_case {
        next.to_owned()
    } else {
        set_initial_case(next, sentence_start)
    };
    let needs_space = previous
        .chars()
        .next_back()
        .is_some_and(|character| !character.is_whitespace() && !is_opening(character))
        && next
            .chars()
            .next()
            .is_some_and(|character| !character.is_whitespace() && !is_closing(character));
    if needs_space {
        next.insert(0, ' ');
    }
    next
}

fn ends_sentence(text: &str) -> bool {
    text.trim_end()
        .trim_end_matches(['\'', '"', ')', ']', '}'])
        .ends_with(['.', '?', '!'])
}

fn is_opening(character: char) -> bool {
    matches!(character, '(' | '[' | '{')
}

fn is_closing(character: char) -> bool {
    matches!(
        character,
        ',' | '.' | '?' | '!' | ';' | ':' | ')' | ']' | '}'
    )
}

fn set_initial_case(text: &str, uppercase: bool) -> String {
    let Some((index, character)) = text
        .char_indices()
        .find(|(_, character)| character.is_alphabetic())
    else {
        return text.to_string();
    };
    if uppercase && character.is_lowercase() {
        return replace_character(text, index, character.to_uppercase());
    }
    if !uppercase && character.is_uppercase() && sentence_initial_word(text, index) {
        return replace_character(text, index, character.to_lowercase());
    }
    text.to_string()
}

fn sentence_initial_word(text: &str, start: usize) -> bool {
    let word = text[start..]
        .split(|character: char| !character.is_alphabetic() && character != '\'')
        .next()
        .unwrap_or_default();
    matches!(
        word.to_ascii_lowercase().as_str(),
        "a" | "an"
            | "and"
            | "as"
            | "because"
            | "but"
            | "for"
            | "if"
            | "nor"
            | "or"
            | "so"
            | "the"
            | "then"
            | "though"
            | "to"
            | "when"
            | "while"
            | "yet"
    )
}

fn replace_character(text: &str, index: usize, replacement: impl Iterator<Item = char>) -> String {
    let character_length = text[index..].chars().next().unwrap().len_utf8();
    let mut output = String::with_capacity(text.len());
    output.push_str(&text[..index]);
    output.extend(replacement);
    output.push_str(&text[index + character_length..]);
    output
}

#[cfg(test)]
mod tests {
    #[test]
    fn canonical_name_case_survives_continuation() {
        assert_eq!(
            join_with_case_policy("Next.", "nimbus-files", true),
            " nimbus-files"
        );
        assert_eq!(
            join_with_case_policy("Use", "OpenRouter", true),
            " OpenRouter"
        );
        assert_eq!(
            join_with_case_policy("Next.", "nimbus-files", false),
            " Nimbus-files"
        );
    }
    #[test]
    fn fallback_copies_only_detected_failures_when_enabled_and_committed() {
        use std::cell::Cell;
        for enabled in [false, true] {
            for cancelled in [false, true] {
                for error in [false, true] {
                    let copied = Cell::new(false);
                    let outcome = if error {
                        Err(eyre!("paste failure"))
                    } else {
                        Ok(PasteOutcome::Deferred)
                    };
                    let result = fallback_after_attempt(outcome, enabled, &|| !cancelled, || {
                        copied.set(true);
                        Ok(())
                    });
                    assert_eq!(copied.get(), enabled && !cancelled);
                    if copied.get() {
                        assert_eq!(result.unwrap(), PasteOutcome::CopiedToClipboard);
                    }
                }
            }
        }
        assert_eq!(
            fallback_after_attempt(
                Ok(PasteOutcome::Pasted),
                true,
                &|| panic!("already committed"),
                || panic!("successful paste must restore the clipboard normally")
            )
            .unwrap(),
            PasteOutcome::Pasted
        );
        assert!(
            fallback_after_attempt(Err(eyre!("paste failure")), true, &|| true, || Err(eyre!(
                "clipboard unavailable"
            )))
            .is_err()
        );
    }

    #[test]
    fn fallback_copy_invalidates_pending_clipboard_restoration() {
        let mut restore = ClipboardRestore::default();
        let original = ClipboardSnapshot { items: Vec::new() };
        let scheduled_generation = restore.register(original, 1, 2);
        restore.forget();
        assert_ne!(restore.generation, scheduled_generation);
        assert!(restore.original.is_none());
        assert!(restore.last_change_count.is_none());
    }

    use std::cell::{Cell, RefCell};

    use super::*;
    use objc2_foundation::NSData;

    static PASTEBOARD_TEST: Mutex<()> = Mutex::new(());

    #[test]
    fn return_follows_a_successful_paste_and_its_consumption_window() {
        let events = std::cell::RefCell::new(Vec::new());
        let sent = complete_paste_with_submit(
            || {
                events.borrow_mut().push("paste");
                Ok(())
            },
            || events.borrow_mut().push("restore scheduled"),
            |delay| {
                assert_eq!(delay, PASTE_SETTLE_DELAY);
                events.borrow_mut().push("wait");
            },
            || {
                submit_if_current(
                    Some(7),
                    || true,
                    || 7,
                    || true,
                    || {
                        events.borrow_mut().push("return");
                        Ok(())
                    },
                )
            },
        )
        .unwrap();
        assert!(sent);
        assert_eq!(
            *events.borrow(),
            ["paste", "restore scheduled", "wait", "return"]
        );
        assert!(
            complete_paste_with_submit(
                || Err(eyre!("paste failed")),
                || {},
                |_| panic!("no wait"),
                || panic!("no Return")
            )
            .is_err()
        );
    }

    #[test]
    fn return_is_not_sent_for_manual_paste_new_input_or_a_changed_destination() {
        for (intent, revision, same_target) in
            [(None, 7, true), (Some(7), 8, true), (Some(7), 7, false)]
        {
            assert!(!submit_if_current(
                intent,
                || true,
                || revision,
                || same_target,
                || panic!("must not send")
            ));
        }
        assert!(!submit_if_current(
            Some(7),
            || true,
            || 7,
            || true,
            || Err(eyre!("posting failed"))
        ));
    }

    #[test]
    fn shutdown_during_paste_settling_prevents_the_later_return() {
        let allowed = Cell::new(true);
        let submitted = complete_paste_with_submit(
            || Ok(()),
            || {},
            |_| allowed.set(false),
            || {
                submit_if_current(
                    Some(7),
                    || allowed.get(),
                    || 7,
                    || true,
                    || panic!("shutdown must not submit the pasted text"),
                )
            },
        )
        .unwrap();
        assert!(!submitted);
    }

    #[test]
    fn continuation_requires_the_same_application_and_input_revision() {
        use crate::context::{ContextSnapshot, ForegroundApplication};
        let target = ContextSnapshot {
            application: Some("Fixture A".into()),
            target: Some(ForegroundApplication::Test(1)),
        };
        let continuation = Continuation {
            revision: 7,
            target: target.target.clone(),
            inserted: "Previous dictation".into(),
        };
        assert!(continuation.applies_to(7, &target));
        assert!(!continuation.applies_to(8, &target));
        assert!(!continuation.applies_to(
            7,
            &ContextSnapshot {
                target: Some(ForegroundApplication::Test(2)),
                ..target.clone()
            }
        ));
        assert!(!continuation.applies_to(7, &ContextSnapshot::default()));
    }

    #[test]
    fn a_focus_change_during_clipboard_preparation_prevents_writes() {
        let current = Cell::new(10);
        let output = commit_targeted_paste(
            || {
                current.set(20);
                Ok("original clipboard")
            },
            || true,
            || (current.get() == 10).then_some(10),
            |_, _| -> Result<()> { panic!("changed target must not receive text") },
        )
        .unwrap();
        assert_eq!(output, None);
    }

    #[test]
    fn the_verified_process_is_kept_for_delivery_even_if_focus_moves_again() {
        let current = Cell::new(10);
        let routed = commit_targeted_paste(
            || Ok(()),
            || true,
            || {
                let verified = current.get();
                current.set(20);
                Some(verified)
            },
            |(), pid| Ok(pid),
        )
        .unwrap();
        assert_eq!(routed, Some(10));
        assert_eq!(current.get(), 20);
        assert!(
            commit_targeted_paste(
                || Ok(()),
                || false,
                || panic!("cancelled output must not inspect or deliver to a target"),
                |(), _| Ok(())
            )
            .is_err()
        );
    }

    #[test]
    fn rejected_commit_does_not_write_paste_or_restore() {
        let steps = RefCell::new(Vec::new());
        let result = commit_prepared_paste(
            || {
                steps.borrow_mut().push("prepare");
                Ok(())
            },
            || {
                steps.borrow_mut().push("commit");
                false
            },
            |()| {
                steps.borrow_mut().push("write");
                complete_paste(
                    || {
                        steps.borrow_mut().push("post");
                        Ok(())
                    },
                    || steps.borrow_mut().push("restore"),
                    |_| steps.borrow_mut().push("settle"),
                )
            },
        );

        assert_eq!(result.unwrap_err().to_string(), "paste was cancelled");
        assert_eq!(steps.into_inner(), ["prepare", "commit"]);
    }

    #[test]
    fn failed_preparation_does_not_commit_or_write() {
        let result = commit_prepared_paste(
            || Err::<(), _>(eyre!("snapshot failed")),
            || panic!("failed preparation must remain cancellable"),
            |()| -> Result<()> { panic!("failed preparation must not mutate the clipboard") },
        );

        assert_eq!(result.unwrap_err().to_string(), "snapshot failed");
    }

    #[test]
    fn sequential_pastes_settle_before_overwrite() {
        let clipboard = Cell::new("");
        let pending = Cell::new(false);
        let consumed = RefCell::new(Vec::new());
        let steps = RefCell::new(Vec::new());
        let paste = |text| {
            clipboard.set(text);
            complete_paste(
                || {
                    pending.set(true);
                    steps.borrow_mut().push("post");
                    Ok(())
                },
                || steps.borrow_mut().push("restore"),
                |delay| {
                    assert_eq!(delay, PASTE_SETTLE_DELAY);
                    assert!(pending.replace(false));
                    consumed.borrow_mut().push(clipboard.get());
                    steps.borrow_mut().push("settle");
                },
            )
        };

        paste("first").unwrap();
        assert_eq!(*consumed.borrow(), ["first"]);
        paste("second").unwrap();
        assert_eq!(*consumed.borrow(), ["first", "second"]);

        assert_eq!(
            steps.into_inner(),
            ["post", "restore", "settle", "post", "restore", "settle"]
        );
    }

    #[test]
    fn restoration_is_scheduled_after_a_stalled_post_completes() {
        let elapsed = Cell::new(Duration::ZERO);
        let restore_scheduled_at = Cell::new(None);

        complete_paste(
            || {
                elapsed.set(Duration::from_secs(1));
                assert_eq!(restore_scheduled_at.get(), None);
                Ok(())
            },
            || restore_scheduled_at.set(Some(elapsed.get())),
            |delay| elapsed.set(elapsed.get() + delay),
        )
        .unwrap();

        assert_eq!(restore_scheduled_at.get(), Some(Duration::from_secs(1)));
        assert_eq!(elapsed.get(), Duration::from_secs(1) + PASTE_SETTLE_DELAY);
    }

    #[test]
    fn failed_paste_still_schedules_restore_without_settling() {
        let steps = RefCell::new(Vec::new());
        let result = complete_paste(
            || {
                steps.borrow_mut().push("post");
                Err(eyre!("post failed"))
            },
            || steps.borrow_mut().push("restore"),
            |_| steps.borrow_mut().push("settle"),
        );

        assert_eq!(result.unwrap_err().to_string(), "post failed");
        assert_eq!(steps.into_inner(), ["post", "restore"]);
    }

    #[test]
    fn continuation_never_undoes_explicit_lowercase_preferences() {
        for preferences in [
            PostProcessing {
                lowercase: true,
                ..PostProcessing::default()
            },
            PostProcessing {
                lowercase_initial: true,
                ..PostProcessing::default()
            },
        ] {
            assert_eq!(
                join_with_preferences("Previous sentence.", "olá João", preferences),
                " olá João"
            );
        }
        assert_eq!(join("Previous sentence.", "olá João"), " Olá João");
    }

    #[test]
    fn joins_contiguous_dictation_with_sentence_aware_spacing() {
        assert_eq!(
            join("Because if I don't do that,", "And if you see it."),
            " and if you see it."
        );
        assert_eq!(
            join("And if you see it.", "well, that works."),
            " Well, that works."
        );
        assert_eq!(join("Already spaced. ", "Next sentence."), "Next sentence.");
        assert_eq!(join("Hello", ", world."), ", world.");
    }

    #[test]
    fn preserves_likely_proper_nouns_mid_sentence() {
        assert_eq!(join("Open", "GitHub next."), " GitHub next.");
        assert_eq!(join("Message", "Slack now."), " Slack now.");
    }

    #[test]
    fn rapid_pastes_keep_the_original_clipboard_for_the_latest_restore() {
        let mut restore = ClipboardRestore::default();
        let original = ClipboardSnapshot {
            items: vec![vec![ClipboardFlavor {
                data_type: "public.png".into(),
                data: vec![1, 2, 3],
                flags: 0,
            }]],
        };
        let first_insert = ClipboardSnapshot {
            items: vec![vec![ClipboardFlavor {
                data_type: "public.utf8-plain-text".into(),
                data: b"first".to_vec(),
                flags: 0,
            }]],
        };

        let first = restore.register(original.clone(), 10, 12);
        let second = restore.register(first_insert, 12, 14);

        assert_ne!(first, second);
        assert_eq!(restore.original, Some(original));
        assert_eq!(restore.last_change_count, Some(14));
    }

    #[test]
    fn clipboard_snapshot_preserves_multiple_items_and_formats() {
        let _test = PASTEBOARD_TEST.lock().unwrap();
        let clipboard = NSPasteboard::pasteboardWithUniqueName();
        let expected = [
            vec![
                ("com.hex.fixture.text", b"text".to_vec()),
                ("com.hex.fixture.rich", b"rich text".to_vec()),
            ],
            vec![("com.hex.fixture.image", vec![1, 2, 3, 4])],
        ];
        let items = expected
            .iter()
            .map(|types| {
                let item = NSPasteboardItem::new();
                for (data_type, bytes) in types {
                    assert!(item.setData_forType(
                        &NSData::with_bytes(bytes),
                        &NSString::from_str(data_type)
                    ));
                }
                item
            })
            .collect();
        write_items(&clipboard, items).unwrap();
        let snapshot = capture_clipboard(&clipboard).unwrap();

        clipboard.clearContents();
        restore_clipboard(&clipboard, &snapshot).unwrap();

        assert_eq!(capture_clipboard(&clipboard).unwrap(), snapshot);
    }

    #[test]
    fn eager_clipboard_capture_refreshes_a_stale_snapshot() {
        let _test = PASTEBOARD_TEST.lock().unwrap();
        let clipboard = NSPasteboard::pasteboardWithUniqueName();
        let item = NSPasteboardItem::new();
        assert!(item.setData_forType(
            &NSData::with_bytes(b"first"),
            &NSString::from_str("com.hex.fixture.first")
        ));
        clipboard.clearContents();
        write_items(&clipboard, vec![item]).unwrap();
        let mut paster = Paster {
            clipboard: clipboard.clone(),
            activity: InputActivity::default(),
            continuation: None,
            clipboard_restore: Arc::new(Mutex::new(ClipboardRestore::default())),
            prepared_clipboard: None,
        };
        paster.prepare();
        let first_change_count = paster.prepared_clipboard.as_ref().unwrap().change_count;

        let item = NSPasteboardItem::new();
        assert!(item.setData_forType(
            &NSData::with_bytes(b"second"),
            &NSString::from_str("com.hex.fixture.second")
        ));
        clipboard.clearContents();
        write_items(&clipboard, vec![item]).unwrap();
        paster.prepare();
        let prepared = paster.prepared_clipboard.as_ref().unwrap();

        assert_ne!(prepared.change_count, first_change_count);
        assert_eq!(prepared.snapshot, capture_clipboard(&clipboard).unwrap());
    }

    #[test]
    fn skips_regenerable_system_translations() {
        assert!(!should_preserve_flavor(SYSTEM_TRANSLATED_FLAVOR, true));
        assert!(should_preserve_flavor(0, true));
        assert!(should_preserve_flavor(SYSTEM_TRANSLATED_FLAVOR, false));
    }

    /// A valid 1×1 transparent PNG.
    const ONE_PIXEL_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn image_snapshot_keeps_its_authoritative_format() {
        let _test = PASTEBOARD_TEST.lock().unwrap();
        let clipboard = NSPasteboard::pasteboardWithUniqueName();
        let item = NSPasteboardItem::new();
        assert!(item.setData_forType(
            &NSData::with_bytes(ONE_PIXEL_PNG),
            &NSString::from_str("public.png")
        ));
        clipboard.clearContents();
        write_items(&clipboard, vec![item]).unwrap();

        let snapshot = capture_clipboard(&clipboard).unwrap();
        assert_eq!(
            snapshot
                .items
                .iter()
                .flatten()
                .map(|flavor| flavor.data_type.as_str())
                .collect::<Vec<_>>(),
            vec!["public.png"]
        );
        clipboard.clearContents();
        restore_clipboard(&clipboard, &snapshot).unwrap();
        assert_eq!(capture_clipboard(&clipboard).unwrap(), snapshot);
    }
}
