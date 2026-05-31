# Providing the SEGGER emUSB-Device files

This firmware's USB stack is **SEGGER emUSB-Device**, a proprietary library. Its
licence — [SEGGER's Friendly License (SFL)](https://www.segger.com/purchase/licensing/license-sfl/),
free for non-commercial / hobby use — **forbids redistribution**. So the SEGGER
headers and the prebuilt library are **not** committed to this (MIT/Apache,
public) repository, and a plain `git clone` cannot build until you provide them
locally, once.

> **Heads-up:** because of the licence, this project cannot truly "just compile"
> from a clean clone the way a pure-Rust project can. The one-time setup below is
> the unavoidable cost of building on SEGGER's stack. The build script fails with
> these same instructions if the files are missing.

There are two ways to get the files.

---

## Option A — contributors with repo access (private submodule)

If you have access to the private companion repository that holds a pinned copy
of the SEGGER files, it is wired in as a git submodule at `vendor/segger`:

```console
$ git submodule update --init vendor/segger
```

(Maintainer setup, once: create a **private** repo containing the
`vendor/segger/` tree — `USB-D/Inc`, `USB-D/Config`, `USB-D/Lib`, `SEGGER/Inc` —
and add it as a submodule:
`git submodule add <private-url> vendor/segger`. Keeping it private avoids
redistributing SEGGER's files publicly while giving you and CI a one-command,
version-pinned checkout. CI authenticates with a read-only deploy key.)

---

## Option B — bring your own SEGGER eval copy (anyone)

### 1. Download the eval bundle

Open the SEGGER emPower eval download page:

  <https://www.segger.com/downloads/empower/>

This firmware is developed and **locked against** this bundle:

| Field      | Value                                                         |
|------------|---------------------------------------------------------------|
| Title      | **SEGGER emPower, Embedded Studio**                           |
| Date       | 2023-06-26                                                    |
| Size       | 303,931 KB                                                    |
| File       | `SeggerEval_K66FN2M0_SEGGER_emPower_CortexM_SES_230626.zip`   |
| ZIP MD5    | `f0cc414564ea198195c44e5f8c09e409` (published by SEGGER)      |
| emUSB-Device | 3.60.0                                                       |

The download page is a "Software" table; the row we want is *SEGGER emPower,
Embedded Studio* (its expandable description lists `emUSB-Device` among the
included middleware and prints the MD5 above). Accept the Terms of Use to start
the download.

> The page changes over time (for example it may also list a newer *emPower
> Zynq* bundle — a different chip family — or eventually only a newer emPower
> revision). The table above describes the exact bundle this firmware was
> verified against; `vendor/segger.lock` is the source of truth. A newer bundle
> is not forbidden — see *Updating* below.

### 2. Extract the needed files

The helper pulls just the files this firmware links out of the ZIP into
`vendor/segger/` and verifies them:

```console
$ cargo xtask setup-segger --zip /path/to/SeggerEval_K66FN2M0_...zip
```

You can also point it via `SEGGER_EVAL_ZIP=/path/to.zip cargo xtask setup-segger`,
or run `cargo xtask setup-segger` with no arguments to be prompted for the path.

> `cargo xtask` builds a small host-side tool. On non-Linux hosts, change the
> target triple in the `xtask` alias in `.cargo/config.toml` (e.g.
> `aarch64-apple-darwin`).

### 3. Build

```console
$ cargo build --release
```

---

## What gets vendored

Everything lands under `vendor/segger/` (git-ignored). The consumed set is:

```
vendor/segger/
  USB-D/Inc/*.h                              # emUSB-Device API headers
  USB-D/Config/USB_Conf.h                    # stack configuration
  USB-D/Lib/libUSBD_v7m_t_vfpv4_le_r.a       # prebuilt release library
  SEGGER/Inc/SEGGER.h, Global.h              # shared SEGGER types
```

`build.rs` links `libUSBD_v7m_t_vfpv4_le_r.a` (Cortex-M4 / VFPv4-D16 /
hard-float — an exact match for `thumbv7em-none-eabihf`) and runs `bindgen`
against the headers into `$OUT_DIR` (the generated bindings are produced from
*your* licensed headers and are never committed).

You can place the files anywhere and point the build at them with
`SEGGER_USBD_DIR` (the `USB-D` dir) and `SEGGER_INC_DIR` (the `SEGGER/Inc` dir).

---

## Reproducibility and the lockfile

`vendor/segger.lock` (committed) records the SHA-256 of every consumed file plus
the bundle identity and emUSB-Device version. It pins *which bytes* a build used,
without shipping them. On every build, `build.rs` re-verifies the vendored files
against it and **fails loudly on any mismatch**.

Keep your own durable backup of the eval ZIP — SEGGER does not guarantee a stable
per-version download URL.

## Build prerequisites

- `libclang` (for `bindgen`), e.g. `apt install libclang-dev`, or set
  `LIBCLANG_PATH`.
- The `thumbv7em-none-eabihf` Rust target: `rustup target add thumbv7em-none-eabihf`.

## Updating to a newer SEGGER version

If only a newer bundle is available (or you deliberately upgrade):

1. `cargo xtask setup-segger --zip <newer.zip>` — extracts and reports which
   files differ from the lock (the ZIP MD5 check will note the bundle changed).
2. Build and test on hardware.
3. If all is well, re-pin: `cargo xtask relock-segger`, then commit the updated
   `vendor/segger.lock`.

## Troubleshooting

- **`SEGGER emUSB-Device files not found`** — run Option A or B above.
- **`Vendored SEGGER files do not match vendor/segger.lock`** — you have a
  different version than is pinned; re-provision with `setup-segger`, or
  `relock-segger` if the change is intentional and verified.
- **`bindgen failed ... libclang`** — install `libclang-dev` / set
  `LIBCLANG_PATH`.
- **Link errors / ABI** — ensure you copied the `_r` (release) `.a` for
  `v7m_t_vfpv4_le` and are building for `thumbv7em-none-eabihf`.
