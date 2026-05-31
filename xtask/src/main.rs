//! Host-side helper for the (non-redistributable) SEGGER emUSB-Device files.
//!
//! Subcommands:
//!   * `setup-segger [--zip <path>]` -- extract the files this firmware links from
//!     a SEGGER eval ZIP into `vendor/segger/`, then verify them against the
//!     committed lockfile.
//!   * `relock-segger` -- recompute `vendor/segger.lock` from the files currently
//!     in `vendor/segger/` (run after a sanctioned SEGGER version bump).
//!
//! Why this exists: SEGGER's license forbids redistributing their files, so they
//! cannot be committed. This turns "bring your own copy" into one command. See
//! docs/SEGGER_SETUP.md.

use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use md5::Md5;
use sha2::{Digest, Sha256};

/// The eval bundle this firmware is developed/locked against.
const BUNDLE_NAME: &str = "SEGGER emPower, Embedded Studio (2023-06-26)";
/// SEGGER's own published MD5 of that bundle ZIP (from the download page).
const BUNDLE_MD5: &str = "f0cc414564ea198195c44e5f8c09e409";
const DOWNLOAD_URL: &str = "https://www.segger.com/downloads/empower/";

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let cmd = args.next();
    match cmd.as_deref() {
        Some("setup-segger") => setup(args.collect()),
        Some("relock-segger") => relock(),
        _ => {
            eprintln!(
                "usage:\n  \
                 cargo xtask setup-segger [--zip <path-to-eval.zip>]\n  \
                 cargo xtask relock-segger"
            );
            std::process::exit(2);
        }
    }
}

/// Repo root = parent of this xtask package directory.
fn repo_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow!("cannot locate repo root from {}", manifest.display()))
}

/// Is `rel` (a forward-slash path inside the bundle / vendor dir) one of the
/// files this firmware actually consumes? Keep in sync with build/segger.rs.
fn is_consumed(rel: &str) -> bool {
    (rel.starts_with("USB-D/Inc/") && rel.ends_with(".h"))
        || rel == "USB-D/Config/USB_Conf.h"
        || rel == "USB-D/Lib/libUSBD_v7m_t_vfpv4_le_r.a"
        || rel == "SEGGER/Inc/SEGGER.h"
        || rel == "SEGGER/Inc/Global.h"
}

/// Files that must exist for the build to work (a sanity check after extraction).
const REQUIRED: &[&str] = &[
    "USB-D/Inc/USB.h",
    "USB-D/Inc/USB_MSD.h",
    "USB-D/Config/USB_Conf.h",
    "USB-D/Lib/libUSBD_v7m_t_vfpv4_le_r.a",
    "SEGGER/Inc/SEGGER.h",
    "SEGGER/Inc/Global.h",
];

// ---------------------------------------------------------------------------
// setup-segger
// ---------------------------------------------------------------------------

fn setup(args: Vec<String>) -> Result<()> {
    let root = repo_root()?;
    let vendor = root.join("vendor/segger");

    let zip_path = resolve_zip(&args)?;
    println!("Using eval ZIP: {}", zip_path.display());

    // Whole-ZIP integrity against SEGGER's published MD5. A mismatch is most
    // likely a newer release; we continue and fall back to per-file checks.
    match md5_file(&zip_path) {
        Ok(md5) if md5 == BUNDLE_MD5 => println!("ZIP MD5 matches {BUNDLE_NAME}."),
        Ok(md5) => println!(
            "note: ZIP MD5 {md5} != expected {BUNDLE_MD5}.\n      \
             Likely a newer bundle than locked; will verify per file instead."
        ),
        Err(e) => println!("note: could not MD5 the ZIP ({e}); continuing."),
    }

    // Extract the consumed files.
    let file = fs::File::open(&zip_path)
        .with_context(|| format!("open ZIP {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("read ZIP archive")?;
    let mut extracted = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let Some(name) = entry.enclosed_name() else {
            continue;
        };
        let rel = name.to_string_lossy().replace('\\', "/");
        if !entry.is_file() || !is_consumed(&rel) {
            continue;
        }
        let dst = vendor.join(&rel);
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;
        fs::write(&dst, &buf).with_context(|| format!("write {}", dst.display()))?;
        extracted += 1;
    }
    println!("Extracted {extracted} file(s) into {}", vendor.display());

    // Sanity: required files present.
    let missing: Vec<&str> = REQUIRED
        .iter()
        .copied()
        .filter(|r| !vendor.join(r).exists())
        .collect();
    if !missing.is_empty() {
        bail!(
            "the ZIP did not contain all required files (missing: {}). \
             Is this the emUSB-Device eval bundle from {DOWNLOAD_URL}?",
            missing.join(", ")
        );
    }

    // Verify against the lockfile, if present.
    let lock = root.join("vendor/segger.lock");
    if lock.exists() {
        match verify_against_lock(&vendor, &lock)? {
            Verify::Match => println!("All files verified against vendor/segger.lock. Done."),
            Verify::Mismatch(d) => {
                println!(
                    "\nwarning: {d} file(s) differ from vendor/segger.lock.\n  \
                     If you intentionally moved to a newer SEGGER version and the build\n  \
                     works, run `cargo xtask relock-segger` to update the lock."
                );
            }
        }
    } else {
        println!("No lockfile yet; run `cargo xtask relock-segger` to create one.");
    }
    Ok(())
}

/// Resolve the ZIP path: `--zip <path>`, else `SEGGER_EVAL_ZIP`, else an
/// interactive prompt on a TTY. Fails (rather than hanging) when non-interactive.
fn resolve_zip(args: &[String]) -> Result<PathBuf> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--zip" {
            return Ok(PathBuf::from(
                it.next().ok_or_else(|| anyhow!("--zip needs a path"))?,
            ));
        }
    }
    if let Some(p) = std::env::var_os("SEGGER_EVAL_ZIP") {
        return Ok(PathBuf::from(p));
    }
    if std::io::stdin().is_terminal() {
        println!("Download the eval bundle ({BUNDLE_NAME}) from:\n  {DOWNLOAD_URL}");
        print!("Path to the downloaded SEGGER eval ZIP: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let p = line.trim().trim_matches(['"', '\'']).to_string();
        if p.is_empty() {
            bail!("no path entered");
        }
        return Ok(PathBuf::from(p));
    }
    bail!(
        "no ZIP given. Pass --zip <path> or set SEGGER_EVAL_ZIP \
         (download from {DOWNLOAD_URL}). See docs/SEGGER_SETUP.md."
    )
}

// ---------------------------------------------------------------------------
// relock-segger
// ---------------------------------------------------------------------------

fn relock() -> Result<()> {
    let root = repo_root()?;
    let vendor = root.join("vendor/segger");
    if !vendor.exists() {
        bail!(
            "{} does not exist; run `cargo xtask setup-segger` first.",
            vendor.display()
        );
    }

    let mut entries: Vec<(String, String)> = Vec::new();
    collect_consumed(&vendor, &vendor, &mut entries)?;
    entries.sort();
    if entries.is_empty() {
        bail!("no SEGGER files found under {}", vendor.display());
    }

    let usb_version = read_usb_version(&vendor).unwrap_or_else(|_| "unknown".into());

    let mut out = String::new();
    out.push_str("# SEGGER emUSB-Device vendored-file lock -- DO NOT edit by hand.\n");
    out.push_str("# Regenerate with `cargo xtask relock-segger` after a sanctioned\n");
    out.push_str("# version bump. These files are NOT committed (SFL forbids it);\n");
    out.push_str("# this lock only pins their identity. See docs/SEGGER_SETUP.md.\n");
    out.push_str(&format!("# bundle = {BUNDLE_NAME}\n"));
    out.push_str(&format!("# bundle_md5 = {BUNDLE_MD5}\n"));
    out.push_str(&format!("# emusb_device_version = {usb_version}\n"));
    for (hash, rel) in &entries {
        out.push_str(&format!("{hash}  {rel}\n"));
    }

    let lock = root.join("vendor/segger.lock");
    fs::write(&lock, out)?;
    println!(
        "Wrote {} ({} files, emUSB-Device {usb_version}).",
        lock.display(),
        entries.len()
    );
    Ok(())
}

fn collect_consumed(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_consumed(base, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(base)?
                .to_string_lossy()
                .replace('\\', "/");
            if is_consumed(&rel) {
                out.push((sha256_file(&path)?, rel));
            }
        }
    }
    Ok(())
}

/// Read `USB_VERSION` (e.g. 36000 -> "3.60.0") from the vendored USB.h.
fn read_usb_version(vendor: &Path) -> Result<String> {
    let text = fs::read_to_string(vendor.join("USB-D/Inc/USB.h"))?;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("#define USB_VERSION") {
            // e.g. "  36000uL // Format ...": drop the comment, take leading digits.
            let token = rest.split("//").next().unwrap_or("").trim();
            let digits: String = token.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(v) = digits.parse::<u32>() {
                return Ok(format!("{}.{}.{}", v / 10000, (v / 100) % 100, v % 100));
            }
        }
    }
    bail!("USB_VERSION not found")
}

// ---------------------------------------------------------------------------
// hashing / verification
// ---------------------------------------------------------------------------

enum Verify {
    Match,
    Mismatch(usize),
}

fn verify_against_lock(vendor: &Path, lock: &Path) -> Result<Verify> {
    let text = fs::read_to_string(lock)?;
    let mut diff = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (hash, rel) = line
            .split_once("  ")
            .ok_or_else(|| anyhow!("malformed lock line: {line}"))?;
        let path = vendor.join(rel);
        let ok = path.exists() && sha256_file(&path)? == hash;
        if !ok {
            eprintln!("  mismatch: {rel}");
            diff += 1;
        }
    }
    Ok(if diff == 0 {
        Verify::Match
    } else {
        Verify::Mismatch(diff)
    })
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn md5_file(path: &Path) -> Result<String> {
    let mut f = fs::File::open(path)?;
    let mut hasher = Md5::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}
