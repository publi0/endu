use std::ffi::{CString, c_char};

use objc2::MainThreadMarker;

use crate::i18n::t;

unsafe extern "C" {
    fn hex_show_permission_guide(permission: i32, instruction: *const c_char);
}

fn show(permission: i32, permission_name: &'static str) {
    if MainThreadMarker::new().is_some() {
        let text = tf!(
            "↑  Drag Endu into the {permission} list above",
            permission = t(permission_name)
        );
        let c_text = CString::new(text).ok();
        let ptr = c_text.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
        unsafe { hex_show_permission_guide(permission, ptr) };
    }
}

pub(crate) fn show_input_monitoring() {
    show(0, "Input Monitoring");
}

pub(crate) fn show_accessibility() {
    show(1, "Accessibility");
}
