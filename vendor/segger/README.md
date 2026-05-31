# `vendor/segger/` — provide SEGGER emUSB-Device here

This directory is intentionally (almost) empty. The firmware links **SEGGER
emUSB-Device**, a proprietary library whose licence (SFL) forbids
redistribution, so its files are **not** committed — only this placeholder is.

Populate it once (everything else here is git-ignored):

```console
# Option A: contributors with access to the private submodule
git submodule update --init vendor/segger

# Option B: bring your own SEGGER eval copy
cargo xtask setup-segger --zip /path/to/SeggerEval_...zip
```

Expected layout once provided:

```
vendor/segger/
  USB-D/Inc/*.h
  USB-D/Config/USB_Conf.h
  USB-D/Lib/libUSBD_v7m_t_vfpv4_le_r.a
  SEGGER/Inc/SEGGER.h, Global.h
```

Full instructions: [`docs/SEGGER_SETUP.md`](../../docs/SEGGER_SETUP.md).
File identities are pinned in [`vendor/segger.lock`](../segger.lock).
