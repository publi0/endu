use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use color_eyre::eyre::{Result, eyre};

type InputSourceRef = *const c_void;
type EventRef = *mut c_void;

const KEY_ACTION_DISPLAY: u16 = 3;
const NO_DEAD_KEYS: isize = 0;
const COMMAND_KEY_CODE: u16 = 55;
const COMMAND_FLAG: u64 = 1 << 20;
const SHIFT_KEY_CODE: u16 = 56;
const SHIFT_FLAG: u64 = 1 << 17;
const RETURN_KEY_CODE: u16 = 36;
/// Right Command through Fn: keys a real keyboard reports as flags-changed
/// events, never as key down or up.
const MODIFIER_KEY_CODES: std::ops::RangeInclusive<u16> = 54..=63;
const FLAGS_CHANGED_EVENT: u32 = 12;
/// Pause between a posted shortcut's events. Remote desktop and VM clients
/// (Microsoft's Windows App) track modifiers from their own events and can
/// read a burst posted in one instant as the bare key, typing "v". Shortcuts
/// are posted only from the output worker, never the capture or shortcut
/// threads, so the pause never delays capture.
const SHORTCUT_EVENT_GAP: Duration = Duration::from_millis(15);
/// Quartz keeps at most 20 UTF-16 units of a keyboard event's Unicode string.
const MAX_UNICODE_UNITS: usize = 20;
const EVENT_SOURCE_USER_DATA: u32 = 42;
pub const SYNTHETIC_EVENT_MARKER: i64 = 0x0056_4f49_4345;
static KEY_CODES: LayoutSnapshot = LayoutSnapshot::new();
static LAYOUT_ACCESS: Mutex<()> = Mutex::new(());
#[cfg(test)]
pub(crate) static INPUT_SOURCE_QUERIES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> InputSourceRef;
    fn TISCopyCurrentASCIICapableKeyboardLayoutInputSource() -> InputSourceRef;
    static kTISPropertyUnicodeKeyLayoutData: *const c_void;
    fn TISGetInputSourceProperty(
        input_source: InputSourceRef,
        property_key: *const c_void,
    ) -> *const c_void;
    fn UCKeyTranslate(
        layout: *const u8,
        key_code: u16,
        key_action: u16,
        modifier_state: u32,
        keyboard_type: u32,
        options: isize,
        dead_key_state: *mut u32,
        max_length: isize,
        actual_length: *mut isize,
        output: *mut u16,
    ) -> i32;
    fn LMGetKbdType() -> u8;
    static kTISNotifySelectedKeyboardInputSourceChanged: *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
    fn CFRelease(value: *const c_void);
    fn CFNotificationCenterGetDistributedCenter() -> *const c_void;
    fn CFNotificationCenterAddObserver(
        center: *const c_void,
        observer: *const c_void,
        callback: unsafe extern "C" fn(
            *const c_void,
            *mut c_void,
            *const c_void,
            *const c_void,
            *const c_void,
        ),
        name: *const c_void,
        object: *const c_void,
        suspension_behavior: isize,
    );
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateKeyboardEvent(source: *mut c_void, key: u16, down: bool) -> EventRef;
    fn CGEventSetFlags(event: EventRef, flags: u64);
    fn CGEventSetType(event: EventRef, event_type: u32);
    fn CGEventKeyboardSetUnicodeString(event: EventRef, length: usize, text: *const u16);
    fn CGEventSetIntegerValueField(event: EventRef, field: u32, value: i64);
    fn CGEventPostToPid(pid: i32, event: EventRef);
}

/// The published layout codes. The main thread replaces them whole when the
/// selected input source changes; any thread reads the current ones without
/// touching TIS, and sees either the old codes or the new, never a mix.
struct LayoutSnapshot(RwLock<Option<Arc<HashMap<char, u16>>>>);

impl LayoutSnapshot {
    const fn new() -> Self {
        Self(RwLock::new(None))
    }

    fn get(&self) -> Option<Arc<HashMap<char, u16>>> {
        self.0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Publishes `codes`; returns whether they differ from the current ones.
    fn publish(&self, codes: HashMap<char, u16>) -> bool {
        let mut current = self.0.write().unwrap_or_else(|error| error.into_inner());
        if current.as_deref() == Some(&codes) {
            return false;
        }
        *current = Some(Arc::new(codes));
        true
    }
}

pub fn key_code_for(character: char) -> Result<u16> {
    let cached = || {
        KEY_CODES.get().map(|codes| {
            codes
                .get(&character.to_ascii_lowercase())
                .copied()
                .ok_or_else(|| eyre!("current keyboard layout has no key for {character:?}"))
        })
    };
    if let Some(result) = cached() {
        return result;
    }
    // Headless callers retain live-layout resolution. GUI callers must prewarm
    // on the main thread; even a cache miss then returns without entering TIS.
    let _access = LAYOUT_ACCESS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(result) = cached() {
        return result;
    }
    let source = LayoutSource::current()?;
    let layout = source.layout_bytes()?;
    let result = (0..128).find(|&key_code| {
        translate(layout, key_code)
            .is_some_and(|translated| translated.eq_ignore_ascii_case(&character.to_string()))
    });
    drop(source);
    if let Some(key_code) = result {
        return Ok(key_code);
    }
    // Non-Latin layouts produce no ASCII letters with an empty modifier state;
    // resolve those against the ASCII-capable layout like AppKit does.
    if let Some(key_code) = ascii_capable_key_code(character) {
        return Ok(key_code);
    }
    Err(eyre!(
        "current keyboard layout has no key for {character:?}"
    ))
}

/// GUI startup must build this snapshot on the main thread before starting
/// workers; [`refresh_layout`] rebuilds it there when the selected input
/// source changes. Lookups, including misses, never re-enter TIS/TSM from a
/// worker thread.
pub fn initialize_layout() -> Result<()> {
    if KEY_CODES.get().is_some() {
        return Ok(());
    }
    let _access = LAYOUT_ACCESS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if KEY_CODES.get().is_none() {
        KEY_CODES.publish(read_layout()?);
    }
    Ok(())
}

/// Rebuilds the snapshot from the now-selected input source, on the main
/// thread, and publishes it whole. Returns whether any key moved. Saved
/// shortcuts are physical key codes, so they need no re-resolution; only
/// character lookups such as the posted Command-V change.
pub fn refresh_layout(_mtm: objc2::MainThreadMarker) -> Result<bool> {
    let _access = LAYOUT_ACCESS
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    Ok(KEY_CODES.publish(read_layout()?))
}

/// Observes input-source changes for the app's life, from GUI startup on the
/// main thread; later calls do nothing. Each change rebuilds the snapshot on
/// the main thread, so a switch made after launch (Dvorak to U.S., say) never
/// leaves Command-V on a stale key. Call it before [`initialize_layout`] so a
/// switch during startup still rebuilds. The observer delivers immediately,
/// including while Endu is in the background, and never polls.
pub fn observe_layout_changes(_mtm: objc2::MainThreadMarker) {
    static OBSERVING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if OBSERVING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    // SAFETY: The callback ignores its observer pointer, so a null observer
    // is valid; the registration lives for the process and is never removed.
    // 4 = DeliverImmediately, so delivery does not wait for activation.
    unsafe {
        CFNotificationCenterAddObserver(
            CFNotificationCenterGetDistributedCenter(),
            std::ptr::null(),
            input_source_changed,
            kTISNotifySelectedKeyboardInputSourceChanged,
            std::ptr::null(),
            4,
        );
    }
}

unsafe extern "C" fn input_source_changed(
    _center: *const c_void,
    _observer: *mut c_void,
    _name: *const c_void,
    _object: *const c_void,
    _user_info: *const c_void,
) {
    // The distributed center delivers on the registering (main) thread's run
    // loop; hop there explicitly should it ever arrive elsewhere, because TIS
    // must only be read on the main thread.
    if let Some(mtm) = objc2::MainThreadMarker::new() {
        refresh_after_input_source_change(mtm);
        return;
    }
    let block = block2::RcBlock::new(|| {
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            refresh_after_input_source_change(mtm);
        }
    });
    // SAFETY: The block captures nothing and is therefore sendable.
    unsafe { objc2_foundation::NSOperationQueue::mainQueue().addOperationWithBlock(&block) };
}

fn refresh_after_input_source_change(mtm: objc2::MainThreadMarker) {
    match refresh_layout(mtm) {
        Ok(changed) => tracing::info!(changed, "rebuilt keyboard layout after input source change"),
        Err(error) => tracing::warn!(%error, "could not rebuild keyboard layout"),
    }
}

fn read_layout() -> Result<HashMap<char, u16>> {
    let source = LayoutSource::current()?;
    let layout = source.layout_bytes()?;
    let codes = collect_key_codes((0..128).filter_map(|key_code| {
        let translated = translate(layout, key_code)?;
        let character = translated.chars().next()?.to_ascii_lowercase();
        Some((character, key_code))
    }));
    drop(source);
    // Non-Latin layouts (Russian, Hebrew, …) produce no ASCII letters with an
    // empty modifier state, so shortcuts like Cmd+V would miss. Appkit resolves
    // those against the ASCII-capable layout; mirror that here by filling the
    // missing letters from the ASCII-capable input source.
    let mut codes = codes;
    if ('a'..='z').any(|character| !codes.contains_key(&character))
        && let Some(ascii_codes) = ascii_capable_key_codes()
    {
        fill_missing_ascii_letters(&mut codes, ascii_codes);
    }
    Ok(codes)
}

fn ascii_capable_key_codes() -> Option<HashMap<char, u16>> {
    // Called only while LAYOUT_ACCESS is held: on the main thread for the GUI
    // snapshot, or by a headless caller without one.
    let source = LayoutSource::ascii_capable()?;
    let layout = source.layout_bytes().ok()?;
    Some(collect_key_codes((0..128).filter_map(|key_code| {
        let translated = translate(layout, key_code)?;
        let character = translated.chars().next()?.to_ascii_lowercase();
        character
            .is_ascii_alphabetic()
            .then_some((character, key_code))
    })))
}

fn fill_missing_ascii_letters(codes: &mut HashMap<char, u16>, fallback: HashMap<char, u16>) {
    for (character, key_code) in fallback {
        if character.is_ascii_alphabetic() {
            codes.entry(character).or_insert(key_code);
        }
    }
}

/// Resolves a character against the ASCII-capable layout for non-Latin
/// active layouts, mirroring how AppKit resolves shortcuts such as Cmd+V.
fn ascii_capable_key_code(character: char) -> Option<u16> {
    if !character.is_ascii_alphabetic() {
        return None;
    }
    ascii_capable_key_codes()?
        .get(&character.to_ascii_lowercase())
        .copied()
}

fn collect_key_codes(entries: impl IntoIterator<Item = (char, u16)>) -> HashMap<char, u16> {
    entries
        .into_iter()
        .fold(HashMap::new(), |mut codes, (character, key_code)| {
            codes.entry(character).or_insert(key_code);
            codes
        })
}

/// Posts Command plus the key that types `character` in the current layout.
pub fn post_command_to_pid(character: char, pid: i32) -> Result<()> {
    if pid <= 0 {
        return Err(eyre!("paste target is unavailable"));
    }
    post_key_code(
        key_code_for(character)?,
        &[(COMMAND_FLAG, COMMAND_KEY_CODE)],
        1,
        pid,
    )
}

pub fn post_return_to_pid(pid: i32) -> Result<()> {
    if pid <= 0 {
        return Err(eyre!("submit target is unavailable"));
    }
    post_key_code(RETURN_KEY_CODE, &[], 1, pid)
}

/// Types `text` into `pid` as Unicode key events, never touching the
/// clipboard. Line breaks are Shift-Return so chat boxes insert a newline
/// instead of sending. Every event carries the synthetic marker, so Endu's
/// own event tap ignores them.
pub fn type_text_to_pid(text: &str, pid: i32) -> Result<()> {
    if pid <= 0 {
        return Err(eyre!("typing target is unavailable"));
    }
    let mut events = Vec::new();
    for stroke in typing_strokes(text) {
        match stroke {
            Stroke::Text(units) => {
                for down in [true, false] {
                    let event = KeyboardEvent::new((0, down, 0))?;
                    event.set_unicode(&units);
                    events.push(event);
                }
            }
            Stroke::LineBreak => {
                for spec in
                    shortcut_event_specs(RETURN_KEY_CODE, &[(SHIFT_FLAG, SHIFT_KEY_CODE)], 1)
                {
                    events.push(KeyboardEvent::new(spec)?);
                }
            }
        }
    }
    for event in events {
        event.post(pid);
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Stroke {
    Text(Vec<u16>),
    LineBreak,
}

/// Splits text into key events of at most `MAX_UNICODE_UNITS` UTF-16 units,
/// the most one event carries, without splitting a surrogate pair. `\r\n`,
/// `\n`, and `\r` each become one line break.
fn typing_strokes(text: &str) -> Vec<Stroke> {
    let mut strokes = Vec::new();
    let mut units = Vec::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if matches!(character, '\r' | '\n') {
            if character == '\r' {
                characters.next_if_eq(&'\n');
            }
            if !units.is_empty() {
                strokes.push(Stroke::Text(std::mem::take(&mut units)));
            }
            strokes.push(Stroke::LineBreak);
            continue;
        }
        let mut encoded = [0; 2];
        let encoded = character.encode_utf16(&mut encoded);
        if units.len() + encoded.len() > MAX_UNICODE_UNITS {
            strokes.push(Stroke::Text(std::mem::take(&mut units)));
        }
        units.extend_from_slice(encoded);
    }
    if !units.is_empty() {
        strokes.push(Stroke::Text(units));
    }
    strokes
}

fn post_key_code(key_code: u16, modifiers: &[(u64, u16)], count: u8, pid: i32) -> Result<()> {
    let specs = shortcut_event_specs(key_code, modifiers, count);
    let events = specs.map(KeyboardEvent::new).collect::<Result<Vec<_>>>()?;
    post_spaced(&events, |event| event.post(pid), std::thread::sleep);
    Ok(())
}

/// Posts `events` in order with `SHORTCUT_EVENT_GAP` between neighbours.
fn post_spaced<E>(events: &[E], mut post: impl FnMut(&E), mut wait: impl FnMut(Duration)) {
    for (index, event) in events.iter().enumerate() {
        if index > 0 {
            wait(SHORTCUT_EVENT_GAP);
        }
        post(event);
    }
}

fn shortcut_event_specs(
    key_code: u16,
    modifiers: &[(u64, u16)],
    count: u8,
) -> impl Iterator<Item = (u16, bool, u64)> {
    let flags = modifiers.iter().fold(0, |flags, (flag, _)| flags | flag);
    let mut active_flags = 0;
    let mut remaining_flags = flags;
    modifiers
        .iter()
        .map(move |(flag, key)| {
            active_flags |= flag;
            (*key, true, active_flags)
        })
        .chain((0..count).flat_map(move |_| [(key_code, true, flags), (key_code, false, flags)]))
        .chain(modifiers.iter().rev().map(move |(flag, key)| {
            remaining_flags &= !flag;
            (*key, false, remaining_flags)
        }))
}

struct KeyboardEvent(EventRef);

impl KeyboardEvent {
    fn new((key, down, flags): (u16, bool, u64)) -> Result<Self> {
        // SAFETY: Null selects the default event source. The owned event is released in Drop.
        let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null_mut(), key, down) };
        if event.is_null() {
            return Err(eyre!("could not create keyboard event"));
        }
        unsafe {
            // A real keyboard reports a modifier key as flags-changed, never
            // key down or up; clients that track modifiers from those events
            // (remote desktops, VMs) otherwise miss the held Command.
            if MODIFIER_KEY_CODES.contains(&key) {
                CGEventSetType(event, FLAGS_CHANGED_EVENT);
            }
            CGEventSetFlags(event, flags);
            CGEventSetIntegerValueField(event, EVENT_SOURCE_USER_DATA, SYNTHETIC_EVENT_MARKER);
        }
        Ok(Self(event))
    }

    fn set_unicode(&self, units: &[u16]) {
        // SAFETY: The event is valid and `units` outlives the call, which copies it.
        unsafe { CGEventKeyboardSetUnicodeString(self.0, units.len(), units.as_ptr()) };
    }

    fn post(&self, pid: i32) {
        // SAFETY: This owns a valid event and the caller verified the target.
        // Routing to that process prevents a focus switch from sending Cmd+V
        // into another application after the final foreground check.
        unsafe { CGEventPostToPid(pid, self.0) };
    }
}

impl Drop for KeyboardEvent {
    fn drop(&mut self) {
        // SAFETY: This is the final owner of the retained CoreGraphics event.
        unsafe { CFRelease(self.0.cast_const()) };
    }
}

/// A retained TIS keyboard-layout input source, released on drop.
struct LayoutSource(InputSourceRef);

impl LayoutSource {
    /// The active layout, falling back to the ASCII-capable layout.
    fn current() -> Result<Self> {
        #[cfg(test)]
        INPUT_SOURCE_QUERIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // SAFETY: TIS copy functions return retained immutable input-source objects.
        let source = unsafe { TISCopyCurrentKeyboardLayoutInputSource() };
        if !source.is_null() {
            return Ok(Self(source));
        }
        Self::ascii_capable().ok_or_else(|| eyre!("no keyboard layout input source is available"))
    }

    fn ascii_capable() -> Option<Self> {
        // SAFETY: TIS copy functions return retained immutable input-source objects.
        let source = unsafe { TISCopyCurrentASCIICapableKeyboardLayoutInputSource() };
        (!source.is_null()).then_some(Self(source))
    }

    /// The UCKeyboardLayout bytes, valid while this source is alive.
    fn layout_bytes(&self) -> Result<*const u8> {
        // SAFETY: The retained source remains alive for every property read.
        let layout_data =
            unsafe { TISGetInputSourceProperty(self.0, kTISPropertyUnicodeKeyLayoutData) };
        if layout_data.is_null() {
            return Err(eyre!("active keyboard layout has no Unicode mapping"));
        }
        // SAFETY: The property is CFData containing a UCKeyboardLayout for this source.
        Ok(unsafe { CFDataGetBytePtr(layout_data) })
    }
}

impl Drop for LayoutSource {
    fn drop(&mut self) {
        // SAFETY: TIS copy functions return a retained source that we own.
        unsafe { CFRelease(self.0) };
    }
}

fn translate(layout: *const u8, key_code: u16) -> Option<String> {
    let mut dead_key_state = 0;
    let mut output = [0_u16; 4];
    let mut length = 0;
    // SAFETY: `layout` points into retained TIS layout data and output buffers are valid.
    let status = unsafe {
        UCKeyTranslate(
            layout,
            key_code,
            KEY_ACTION_DISPLAY,
            0,
            LMGetKbdType().into(),
            NO_DEAD_KEYS,
            &mut dead_key_state,
            output.len() as isize,
            &mut length,
            output.as_mut_ptr(),
        )
    };
    (status == 0 && length > 0)
        .then(|| String::from_utf16(&output[..length as usize]).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_latin_snapshot_gets_shortcut_letters_without_replacing_active_keys() {
        let mut codes = collect_key_codes([('ж', 41), ('v', 12)]);
        let fallback = collect_key_codes([('v', 9), ('c', 8), ('c', 88), ('1', 18)]);
        fill_missing_ascii_letters(&mut codes, fallback);
        assert_eq!(codes.get(&'ж'), Some(&41));
        assert_eq!(codes.get(&'v'), Some(&12));
        assert_eq!(codes.get(&'c'), Some(&8));
        assert!(!codes.contains_key(&'1'));

        let mut non_latin = collect_key_codes([('מ', 9)]);
        fill_missing_ascii_letters(&mut non_latin, collect_key_codes([('v', 9)]));
        assert_eq!(non_latin.get(&'v'), Some(&9));
        assert_eq!(non_latin.get(&'מ'), Some(&9));
    }

    fn dvorak() -> HashMap<char, u16> {
        collect_key_codes([('v', 47), ('.', 9)])
    }

    fn us() -> HashMap<char, u16> {
        collect_key_codes([('v', 9), ('.', 47)])
    }

    /// Launched on Dvorak, then switched to U.S.: Command-V must post the U.S.
    /// V key once the main thread publishes the new layout.
    #[test]
    fn a_published_layout_replaces_the_one_before() {
        let snapshot = LayoutSnapshot::new();
        assert!(snapshot.get().is_none());
        assert!(snapshot.publish(dvorak()));
        assert_eq!(snapshot.get().unwrap().get(&'v'), Some(&47));
        assert!(
            !snapshot.publish(dvorak()),
            "the same layout changes nothing"
        );
        assert!(snapshot.publish(us()));
        assert_eq!(snapshot.get().unwrap().get(&'v'), Some(&9));
        assert!(snapshot.publish(dvorak()));
        assert_eq!(snapshot.get().unwrap().get(&'v'), Some(&47));
    }

    /// Workers read while the main thread publishes: each read is the old
    /// layout or the new one, never a mix of the two.
    #[test]
    fn readers_see_a_whole_layout_while_it_is_replaced() {
        let snapshot = Arc::new(LayoutSnapshot::new());
        snapshot.publish(dvorak());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let readers = (0..4)
            .map(|_| {
                let snapshot = snapshot.clone();
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let (dvorak, us) = (dvorak(), us());
                    let mut reads = 0_u64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) || reads == 0 {
                        let codes = snapshot.get().expect("published before reading");
                        assert!(*codes == dvorak || *codes == us, "torn layout: {codes:?}");
                        reads += 1;
                    }
                })
            })
            .collect::<Vec<_>>();
        for round in 0..2_000 {
            snapshot.publish(if round % 2 == 0 { us() } else { dvorak() });
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for reader in readers {
            reader.join().unwrap();
        }
    }

    /// Remote desktop clients read Command from flags-changed events, as a
    /// real keyboard sends them; the shortcut's own key stays key down/up.
    /// The events are created, never posted.
    #[test]
    fn posted_modifiers_are_flags_changed_events() {
        #[link(name = "ApplicationServices", kind = "framework")]
        unsafe extern "C" {
            fn CGEventGetType(event: EventRef) -> u32;
            fn CGEventGetIntegerValueField(event: EventRef, field: u32) -> i64;
        }
        const KEY_DOWN: u32 = 10;
        const KEY_UP: u32 = 11;
        let events = shortcut_event_specs(9, &[(COMMAND_FLAG, COMMAND_KEY_CODE)], 1)
            .map(|spec| KeyboardEvent::new(spec).unwrap())
            .collect::<Vec<_>>();
        // SAFETY: each event is valid until `events` drops.
        let types = events
            .iter()
            .map(|event| unsafe { CGEventGetType(event.0) })
            .collect::<Vec<_>>();
        assert_eq!(
            types,
            [FLAGS_CHANGED_EVENT, KEY_DOWN, KEY_UP, FLAGS_CHANGED_EVENT]
        );
        for event in &events {
            // SAFETY: as above.
            let marker = unsafe { CGEventGetIntegerValueField(event.0, EVENT_SOURCE_USER_DATA) };
            assert_eq!(marker, SYNTHETIC_EVENT_MARKER, "Endu's tap must ignore it");
        }
    }

    #[test]
    fn shortcut_events_are_spaced_without_a_leading_or_trailing_pause() {
        let steps = std::cell::RefCell::new(Vec::new());
        post_spaced(
            &[1, 2, 3, 4],
            |event| steps.borrow_mut().push(format!("post {event}")),
            |gap| steps.borrow_mut().push(format!("wait {}", gap.as_millis())),
        );
        assert_eq!(
            steps.into_inner(),
            [
                "post 1", "wait 15", "post 2", "wait 15", "post 3", "wait 15", "post 4"
            ]
        );
        let mut waits = 0;
        post_spaced(&[1], |_| {}, |_| waits += 1);
        assert_eq!(waits, 0);
    }

    #[test]
    fn typing_splits_text_into_twenty_unit_events_without_splitting_surrogate_pairs() {
        let text = |text: &str| Stroke::Text(text.encode_utf16().collect());
        assert_eq!(typing_strokes(""), []);
        assert_eq!(typing_strokes("hello"), [text("hello")]);
        assert_eq!(
            typing_strokes("abcdefghijklmnopqrstuvwxyz"),
            [text("abcdefghijklmnopqrst"), text("uvwxyz")]
        );
        // 19 units, then a surrogate pair that would make 21, moves whole.
        assert_eq!(
            typing_strokes("abcdefghijklmnopqrs😀z"),
            [text("abcdefghijklmnopqrs"), text("😀z")]
        );
        assert_eq!(
            typing_strokes("😀😀😀😀😀😀😀😀😀😀😀"),
            [text("😀😀😀😀😀😀😀😀😀😀"), text("😀")]
        );
        for stroke in typing_strokes(&"é漢😀a".repeat(40)) {
            let Stroke::Text(units) = stroke else {
                panic!("unexpected line break");
            };
            assert!(units.len() <= MAX_UNICODE_UNITS);
            assert!(String::from_utf16(&units).is_ok());
        }
    }

    #[test]
    fn typing_sends_each_line_break_as_one_shift_return() {
        let text = |text: &str| Stroke::Text(text.encode_utf16().collect());
        assert_eq!(
            typing_strokes("one\ntwo\r\nthree\rfour\n\n"),
            [
                text("one"),
                Stroke::LineBreak,
                text("two"),
                Stroke::LineBreak,
                text("three"),
                Stroke::LineBreak,
                text("four"),
                Stroke::LineBreak,
                Stroke::LineBreak,
            ]
        );
        assert_eq!(
            shortcut_event_specs(RETURN_KEY_CODE, &[(SHIFT_FLAG, SHIFT_KEY_CODE)], 1)
                .collect::<Vec<_>>(),
            [
                (SHIFT_KEY_CODE, true, SHIFT_FLAG),
                (RETURN_KEY_CODE, true, SHIFT_FLAG),
                (RETURN_KEY_CODE, false, SHIFT_FLAG),
                (SHIFT_KEY_CODE, false, 0),
            ]
        );
    }

    #[test]
    fn typing_refuses_an_unverified_target_before_creating_events() {
        assert!(type_text_to_pid("hi", 0).is_err());
        assert!(type_text_to_pid("hi", -1).is_err());
    }

    #[test]
    fn layout_cache_prefers_the_first_key_that_produces_a_character() {
        let codes = collect_key_codes([('2', 19), ('2', 84)]);

        assert_eq!(codes.get(&'2'), Some(&19));
    }

    #[test]
    fn shortcut_modifier_events_track_physical_flag_transitions() {
        let events =
            shortcut_event_specs(9, &[(COMMAND_FLAG, COMMAND_KEY_CODE)], 1).collect::<Vec<_>>();

        assert_eq!(
            events,
            vec![
                (COMMAND_KEY_CODE, true, COMMAND_FLAG),
                (9, true, COMMAND_FLAG),
                (9, false, COMMAND_FLAG),
                (COMMAND_KEY_CODE, false, 0),
            ]
        );
    }
}
