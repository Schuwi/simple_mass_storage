//! Raw FFI bindings to SEGGER emUSB-Device, generated at build time by `bindgen`
//! from the vendored headers (see `build/segger.rs`). Never edited by hand and
//! never committed -- the output lives in `$OUT_DIR/segger_bindings.rs` and is
//! produced from the user's own licensed copy of the SEGGER headers.
//!
//! Everything here is `unsafe` C ABI; the rest of `crate::segger` wraps it in a
//! safe facade. Refer to `Doc/UM09001_emUSBD.pdf` for the API contracts.
#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    dead_code,
    clippy::all
)]

include!(concat!(env!("OUT_DIR"), "/segger_bindings.rs"));
