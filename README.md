# `stm32-template`

> A template for building applications for STM32 microcontrollers

## Dependencies

To build embedded programs using this template you'll need:

- The `cargo generate` subcommand. [Installation
  instructions](https://github.com/cargo-generate/cargo-generate#installation).
``` console
$ cargo install cargo-generate
```

- Flash and run/debug tools:
``` console
$ cargo install probe-rs --features cli
```

- `rust-std` components (pre-compiled `core` crate) for the ARM Cortex-M
  targets. Run:
  
``` console
$ rustup target add thumbv6m-none-eabi thumbv7m-none-eabi thumbv7em-none-eabi thumbv7em-none-eabihf
```

## Building: SEGGER emUSB-Device dependency

This firmware's USB stack is **SEGGER emUSB-Device**, a proprietary library used
under [SEGGER's Friendly License](https://www.segger.com/purchase/licensing/license-sfl/)
(free for non-commercial / hobby use). That licence **forbids redistribution**,
so the SEGGER headers and prebuilt library are **not** committed here — and a
plain `cargo build` will fail until you provide them locally, once.

Two ways to do that:

``` console
# Contributors with access to the private submodule:
$ git submodule update --init vendor/segger

# Or bring your own SEGGER eval copy (free for non-commercial use):
$ cargo xtask setup-segger --zip /path/to/SeggerEval_...zip
```

Then `cargo build --release` as usual. You will also need `libclang` (for
`bindgen`) and the `thumbv7em-none-eabihf` target. Full instructions, the exact
eval bundle, and troubleshooting are in
**[`docs/SEGGER_SETUP.md`](docs/SEGGER_SETUP.md)**.

## Storage image: maximum compatibility & archival robustness

This firmware presents a **read-only USB mass-storage device** whose contents are
an embedded, ZX0-compressed FAT image built at compile time from `assets/` (plus
an optional snapshot of the source that produced it). Because the device is meant
for **archival** use, its guiding principle is to mount cleanly on the **widest
possible range of hosts, now and in the future** — so the emitted image is held
to strict spec-compliance rather than "works on the machine I tested":

- **Real MBR partition table**, not the bare "superfloppy" layout (a filesystem
  written at LBA 0 with no partition table). Some hosts — notably Android —
  refuse to mount a superfloppy. The single partition is 1 MiB-aligned (starts at
  LBA 2048), the alignment every modern partitioner uses.
- **FAT16 with 512-byte clusters**, chosen deliberately over the FAT12 that small
  volumes would otherwise default to, as FAT16 is the most broadly mounted format
  for media this size.
- **Spec deviations are fixed, not tolerated.** For example, the `fatfs` crate
  emits invalid long-name entries in front of each subdirectory's `.`/`..`
  entries; the build rewrites them into the canonical bare 8.3 form.
- **Independent verification.** The build validates the finished image with
  [`fsck.fat`](https://github.com/dosfstools/dosfstools) (a *different* FAT
  implementation than the one that wrote it) and asserts the MBR layout byte for
  byte. If `fsck.fat` isn't installed it is skipped with a warning; set
  `SMS_SKIP_IMAGE_CHECK=1` to skip it deliberately.

All of this lives in [`build/fat_image.rs`](build/fat_image.rs). When changing how
the image is produced, **preserve this strictness** — keep the independent check
passing and avoid anything that trades compatibility for convenience.

## Instantiate the template.

1. Run and enter project name
``` console
$ cargo generate --git https://github.com/burrbull/stm32-template/
 Project Name: app
```

2. Specify **chip product name** and answer on several other guide questions.

3. Your program is ready to compile:
``` console
$ cargo build --release
```

## Flash and run/debug

You can flash your firmware using one of those tools:

- `cargo flash --release` — just flash
- `cargo run --release` — flash and run using `probe-rs run` runner or `probe-run` runner (deprecated) which you can set in `.cargo/config.toml`
- `cargo embed --release` — multifunctional tool for flash and debug

You also can debug your firmware on device from VS Code with [probe-rs](https://probe.rs/docs/tools/vscode/) extention or with `probe-rs gdb` command.
You will need SVD specification for your chip for this. You can load patched SVD files [here](https://stm32-rs.github.io/stm32-rs/).

## Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.

## Code of Conduct

Contribution to this crate is organized under the terms of the [Rust Code of
Conduct][CoC], the maintainer of this crate, promises
to intervene to uphold that code of conduct.

[CoC]: https://www.rust-lang.org/policies/code-of-conduct
