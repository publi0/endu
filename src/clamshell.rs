//! A MacBook's lid state, for microphone selection.
//!
//! With the lid closed the internal microphone is switched off in hardware,
//! yet CoreAudio still lists it, often as the default input, so capturing
//! from it records silence. A microphone in the headphone jack stays on.
//!
//! The state is read from `AppleClamshellState` on IOKit's power root domain
//! whenever an input is resolved, never on the capture or shortcut threads.
//! An IOKit interest notification advances [`generation`] when the lid opens
//! or closes, so the audio owner and the Microphone pane resolve again
//! without polling. Macs without a lid read open.

use std::ffi::{c_char, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use objc2_core_foundation::CFString;

/// The message reported to the user when nothing but the internal
/// microphone is listed and the lid is closed. Compared by the Microphone
/// pane, which translates it at display.
pub const LID_CLOSED_ERROR: &str = "the built-in microphone is off while the lid is closed";

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Advances each time the lid opens or closes.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Whether a built-in CoreAudio input is the internal microphone the lid
/// switches off, from its UID and selected input data source. A microphone
/// in the headphone jack is built in too, but stays on with the lid closed:
/// Apple silicon lists it as its own device (`BuiltInHeadphoneInputDevice`),
/// older Macs as the built-in input's external-microphone or line-in source.
pub fn is_lid_switched_microphone(uid: &str, data_source: Option<u32>) -> bool {
    const EXTERNAL_MICROPHONE: u32 = u32::from_be_bytes(*b"emic");
    const LINE_IN: u32 = u32::from_be_bytes(*b"line");
    if uid.contains("HeadphoneInput") {
        return false;
    }
    !matches!(data_source, Some(EXTERNAL_MICROPHONE | LINE_IN))
}

const MAIN_PORT_DEFAULT: u32 = 0;
/// `kIOPMMessageClamshellStateChange`:
/// `iokit_family_msg(sub_iokit_powermanagement, 0x100)`.
const CLAMSHELL_STATE_CHANGE: u32 = 0xE003_4100;

type InterestCallback = unsafe extern "C" fn(*mut c_void, u32, u32, *mut c_void);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IORegistryEntryCreateCFProperty(
        entry: u32,
        key: *const c_void,
        allocator: *const c_void,
        options: u32,
    ) -> *const c_void;
    fn IONotificationPortCreate(main_port: u32) -> *mut c_void;
    fn IONotificationPortSetDispatchQueue(port: *mut c_void, queue: *mut c_void);
    fn IOServiceAddInterestNotification(
        port: *mut c_void,
        service: u32,
        interest_type: *const c_char,
        callback: InterestCallback,
        refcon: *mut c_void,
        notification: *mut u32,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFBooleanTrue: *const c_void;
    fn CFRelease(value: *const c_void);
}

unsafe extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
}

fn root_domain() -> Option<u32> {
    static ROOT: OnceLock<Option<u32>> = OnceLock::new();
    *ROOT.get_or_init(|| {
        // SAFETY: the matching dictionary is consumed by the lookup, and the
        // returned service is kept for the life of the process.
        let service = unsafe {
            let matching = IOServiceMatching(c"IOPMrootDomain".as_ptr());
            if matching.is_null() {
                return None;
            }
            IOServiceGetMatchingService(MAIN_PORT_DEFAULT, matching)
        };
        (service != 0).then_some(service)
    })
}

/// Whether the lid is closed right now. Reads the IORegistry, so callers
/// stay off the capture and shortcut threads.
pub fn closed() -> bool {
    let Some(root) = root_domain() else {
        return false;
    };
    let key = CFString::from_str("AppleClamshellState");
    let key: *const CFString = &*key;
    // SAFETY: `root` is a live service and `key` a live CFString; the copied
    // property is released after the comparison.
    unsafe {
        let value = IORegistryEntryCreateCFProperty(root, key.cast(), std::ptr::null(), 0);
        if value.is_null() {
            return false;
        }
        let closed = value == kCFBooleanTrue;
        CFRelease(value);
        closed
    }
}

/// Starts observing the lid once per process. The notification arrives on a
/// dispatch queue and only advances [`generation`].
pub fn watch() {
    static WATCHING: OnceLock<()> = OnceLock::new();
    unsafe extern "C" fn on_message(
        _refcon: *mut c_void,
        _service: u32,
        message: u32,
        _argument: *mut c_void,
    ) {
        if message == CLAMSHELL_STATE_CHANGE {
            GENERATION.fetch_add(1, Ordering::AcqRel);
        }
    }
    if WATCHING.set(()).is_err() {
        return;
    }
    let Some(root) = root_domain() else {
        return;
    };
    // SAFETY: the port and notification live for the process, and the
    // callback only touches an atomic.
    let status = unsafe {
        let port = IONotificationPortCreate(MAIN_PORT_DEFAULT);
        if port.is_null() {
            -1
        } else {
            IONotificationPortSetDispatchQueue(port, dispatch_get_global_queue(0, 0));
            let mut notification = 0;
            IOServiceAddInterestNotification(
                port,
                root,
                c"IOGeneralInterest".as_ptr(),
                on_message,
                std::ptr::null_mut(),
                &mut notification,
            )
        }
    };
    if status != 0 {
        tracing::warn!(status, "could not watch the lid for microphone changes");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_internal_microphone_is_switched_off_by_the_lid() {
        let source = |code: &[u8; 4]| Some(u32::from_be_bytes(*code));
        assert!(is_lid_switched_microphone("BuiltInMicrophoneDevice", None));
        assert!(is_lid_switched_microphone(
            "AppleHDAEngineInput:1B,0,1,0:1",
            source(b"imic")
        ));
        assert!(!is_lid_switched_microphone(
            "BuiltInHeadphoneInputDevice",
            None
        ));
        assert!(!is_lid_switched_microphone(
            "AppleHDAEngineInput:1B,0,1,0:1",
            source(b"emic")
        ));
        assert!(!is_lid_switched_microphone(
            "AppleHDAEngineInput:1B,0,1,0:1",
            source(b"line")
        ));
    }
}
