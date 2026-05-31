//! Build-time integration for the vendored SEGGER emUSB-Device stack.
//!
//! Two jobs:
//!   1. Locate + verify the (non-redistributable, locally provided) SEGGER files
//!      and tell the linker to statically link the prebuilt emUSB-Device archive.
//!   2. Generate Rust FFI bindings from the SEGGER headers with `bindgen`, into
//!      `$OUT_DIR/segger_bindings.rs` (pulled in by `src/segger/sys.rs`).
//!
//! The SEGGER files are NOT committed (their license forbids redistribution), so
//! this module is also the first place a fresh checkout fails -- the error text
//! doubles as setup documentation. See `docs/SEGGER_SETUP.md`.

use std::env;
use std::path::{Path, PathBuf};

/// emUSB-Device version this firmware is developed/locked against (the
/// "SEGGER emPower, Embedded Studio" eval bundle dated 2023-06-26).
const LOCKED_BUNDLE: &str = "SEGGER emPower, Embedded Studio (2023-06-26)";

/// The prebuilt archive we link. Release build, Cortex-M4 / VFPv4-D16 / hard-float
/// (matches `thumbv7em-none-eabihf`). `lib<NAME>.a` lives in `<USB-D>/Lib`.
const LIB_NAME: &str = "USBD_v7m_t_vfpv4_le_r";

pub fn build() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // `SEGGER_USBD_DIR` overrides the default vendor location (used by the private
    // submodule / `cargo xtask setup-segger` / a bring-your-own copy).
    let usbd_dir = env::var_os("SEGGER_USBD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("vendor/segger/USB-D"));
    // Shared SEGGER headers (SEGGER.h / Global.h) live one level up in SEGGER/Inc.
    let segger_inc = env::var_os("SEGGER_INC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest_dir.join("vendor/segger/SEGGER/Inc"));

    let inc = usbd_dir.join("Inc");
    let conf = usbd_dir.join("Config");
    let lib = usbd_dir.join("Lib");
    let lib_file = lib.join(format!("lib{LIB_NAME}.a"));

    // ---- presence check (error text == setup docs) -------------------------------
    let mut missing: Vec<PathBuf> = Vec::new();
    for p in [&inc.join("USB.h"), &inc.join("USB_MSD.h"), &lib_file, &segger_inc.join("SEGGER.h")]
    {
        if !p.exists() {
            missing.push(p.clone());
        }
    }
    if !missing.is_empty() {
        fail_missing(&usbd_dir, &missing);
    }

    // ---- link the prebuilt emUSB-Device archive ----------------------------------
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static={LIB_NAME}");

    // ---- generate FFI bindings ---------------------------------------------------
    generate_bindings(&manifest_dir, &inc, &conf, &segger_inc, &out_dir);

    // Re-run when the vendored files or the wrapper/shim change.
    println!("cargo:rerun-if-env-changed=SEGGER_USBD_DIR");
    println!("cargo:rerun-if-env-changed=SEGGER_INC_DIR");
    println!("cargo:rerun-if-changed=build/segger_wrapper.h");
    println!("cargo:rerun-if-changed=build/shim/string.h");
    println!("cargo:rerun-if-changed={}", lib_file.display());
    println!("cargo:rerun-if-changed={}", inc.join("USB.h").display());
    println!("cargo:rerun-if-changed={}", inc.join("USB_MSD.h").display());
}

fn generate_bindings(
    manifest_dir: &Path,
    inc: &Path,
    conf: &Path,
    segger_inc: &Path,
    out_dir: &Path,
) {
    // bindgen needs libclang; nudge clang-sys toward an installed LLVM if the user
    // has not set LIBCLANG_PATH explicitly. Harmless if the path does not exist.
    if env::var_os("LIBCLANG_PATH").is_none() {
        for cand in ["/usr/lib/llvm-19/lib", "/usr/lib/llvm-18/lib", "/usr/lib"] {
            if Path::new(cand).exists() {
                // SAFETY: single-threaded build script, before bindgen runs.
                unsafe { env::set_var("LIBCLANG_PATH", cand) };
                break;
            }
        }
    }

    let shim = manifest_dir.join("build/shim");
    let wrapper = manifest_dir.join("build/segger_wrapper.h");

    let bindings = bindgen::Builder::default()
        .header(wrapper.to_str().unwrap())
        // Parse for the real target so pointer/int sizes and enum width match the
        // prebuilt archive (Cortex-M4, ILP32, -fshort-enums like the SES build).
        .clang_arg("-target")
        .clang_arg("thumbv7em-none-eabihf")
        .clang_arg("-fshort-enums")
        .clang_arg(format!("-I{}", inc.display()))
        .clang_arg(format!("-I{}", conf.display()))
        .clang_arg(format!("-I{}", segger_inc.display()))
        // Shim dir LAST so it only satisfies libc headers clang can't provide
        // (e.g. <string.h>) without shadowing clang's own freestanding headers.
        .clang_arg(format!("-I{}", shim.display()))
        .use_core()
        .ctypes_prefix("::core::ffi")
        // Only the emUSB-Device surface; the shim's libc decls are dropped here.
        .allowlist_item("USB.*")
        .allowlist_item("USBD.*")
        .allowlist_item("SEGGER_.*")
        // Zero-initialise INIT/INST data the way the C samples do via memset(0).
        .derive_default(true)
        // Layout tests use core::mem and add noise; the link + runtime are the
        // real proof, and a mismatch would corrupt silently regardless.
        .layout_tests(false)
        .generate()
        .unwrap_or_else(|e| {
            panic!(
                "bindgen failed to parse the SEGGER headers ({e}).\n\
                 This usually means libclang is missing -- install it (e.g.\n\
                 `apt install libclang-dev`) or set LIBCLANG_PATH. See docs/SEGGER_SETUP.md."
            )
        });

    bindings
        .write_to_file(out_dir.join("segger_bindings.rs"))
        .expect("write segger_bindings.rs");
}

#[cold]
fn fail_missing(usbd_dir: &Path, missing: &[PathBuf]) -> ! {
    let mut list = String::new();
    for p in missing {
        list.push_str(&format!("    {}\n", p.display()));
    }
    panic!(
        "\n\
\n\
================================================================================\n\
 SEGGER emUSB-Device files not found.\n\
\n\
 This firmware links SEGGER's emUSB-Device library. Its license (SFL) forbids us\n\
 from redistributing it, so it is NOT bundled in this repository. You must\n\
 provide it locally, once.\n\
\n\
 Missing:\n\
{list}\
\n\
 Provide {LOCKED_BUNDLE} one of two ways:\n\
\n\
   A) Contributors with repo access (private submodule):\n\
        git submodule update --init vendor/segger\n\
\n\
   B) Bring your own SEGGER eval copy (free for non-commercial use):\n\
        cargo xtask setup-segger --zip <path-to-eval.zip>\n\
      ...or copy the files into vendor/segger/ by hand (see docs/SEGGER_SETUP.md).\n\
\n\
 Expected layout (override the root with SEGGER_USBD_DIR):\n\
   {usbd}/Inc/USB.h, USB_MSD.h, ...\n\
   {usbd}/Config/USB_Conf.h\n\
   {usbd}/Lib/lib{LIB_NAME}.a\n\
   vendor/segger/SEGGER/Inc/SEGGER.h, Global.h\n\
\n\
 Full guide: docs/SEGGER_SETUP.md\n\
================================================================================\n",
        usbd = usbd_dir.display(),
    )
}
