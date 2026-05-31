//! Logging hooks the stack calls (`USB_X_Log` / `USB_X_Warn`) plus a minimal
//! `SEGGER_vsnprintf` so the symbol resolves. The release library has logging
//! largely compiled out, so these are rarely (if ever) exercised; we forward any
//! message text to `defmt` and keep formatting trivial.

use core::ffi::{c_char, c_int, c_void};

/// Forward a NUL-terminated C string to `defmt` at the given level. Best-effort:
/// non-UTF-8 bytes are shown as a placeholder.
fn forward(prefix: &str, s: *const c_char) {
    if s.is_null() {
        return;
    }
    // SAFETY: the stack passes a valid NUL-terminated string.
    let bytes = unsafe { core::ffi::CStr::from_ptr(s) }.to_bytes();
    match core::str::from_utf8(bytes) {
        Ok(text) => defmt::info!("{}: {}", prefix, text),
        Err(_) => defmt::info!("{}: <{} non-utf8 bytes>", prefix, bytes.len()),
    }
}

#[no_mangle]
pub extern "C" fn USB_X_Log(s: *const c_char) {
    forward("emUSB", s);
}

#[no_mangle]
pub extern "C" fn USB_X_Warn(s: *const c_char) {
    forward("emUSB WARN", s);
}

/// Minimal stand-in for SEGGER's `vsnprintf`. The release stack references the
/// symbol but, with logging compiled out, does not rely on real formatting. We
/// just NUL-terminate the buffer and report zero bytes written.
///
/// # Safety
/// `buffer`/`buffer_size` describe a writable region; `_format`/`_args` are a
/// printf-style format and its varargs, which we intentionally ignore.
#[no_mangle]
pub unsafe extern "C" fn SEGGER_vsnprintf(
    buffer: *mut c_char,
    buffer_size: c_int,
    _format: *const c_char,
    _args: *mut c_void,
) -> c_int {
    if !buffer.is_null() && buffer_size > 0 {
        unsafe { *buffer = 0 };
    }
    0
}
