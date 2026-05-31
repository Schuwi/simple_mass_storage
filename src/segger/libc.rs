//! The handful of C string functions the prebuilt emUSB-Device archive needs that
//! Rust's `compiler_builtins` does not provide (it supplies `mem*` but not these).
//! Straightforward, standard-conforming implementations.

use core::ffi::{c_char, c_int};

/// `strcmp`: lexicographic compare of two NUL-terminated strings.
///
/// # Safety
/// `a` and `b` must point to valid NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn strcmp(a: *const c_char, b: *const c_char) -> c_int {
    let (mut a, mut b) = (a, b);
    loop {
        let (ca, cb) = unsafe { (*a, *b) };
        if ca != cb || ca == 0 {
            return ca as c_int - cb as c_int;
        }
        a = unsafe { a.add(1) };
        b = unsafe { b.add(1) };
    }
}

/// `strncpy`: copy up to `n` bytes of `src` into `dst`, NUL-padding the remainder.
///
/// # Safety
/// `dst` must be writable for `n` bytes; `src` must be a valid NUL-terminated
/// string (or readable up to the copied length).
#[no_mangle]
pub unsafe extern "C" fn strncpy(dst: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
    let mut i = 0;
    let mut hit_nul = false;
    while i < n {
        let c = if hit_nul { 0 } else { unsafe { *src.add(i) } };
        if c == 0 {
            hit_nul = true;
        }
        unsafe { *dst.add(i) = c };
        i += 1;
    }
    dst
}

/// `strrchr`: pointer to the last occurrence of `c` in `s` (NUL included), or
/// null if not found.
///
/// # Safety
/// `s` must point to a valid NUL-terminated C string.
#[no_mangle]
pub unsafe extern "C" fn strrchr(s: *const c_char, c: c_int) -> *mut c_char {
    let needle = c as c_char;
    let mut p = s;
    let mut last: *const c_char = core::ptr::null();
    loop {
        let ch = unsafe { *p };
        if ch == needle {
            last = p;
        }
        if ch == 0 {
            return last as *mut c_char;
        }
        p = unsafe { p.add(1) };
    }
}
