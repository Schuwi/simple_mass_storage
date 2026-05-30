//! Build a raw FAT12/FAT16 image from a directory tree, for embedding into the
//! firmware via `include_bytes!`.
//!
//! This is a Rust port of the former `tools/make_fat_image.py`, leaning on the
//! `fatfs` crate to do the on-disk FAT layout (geometry, FAT12/16 auto-selection
//! from the cluster count, and VFAT long file names). It is meant to be driven
//! from `build.rs`.
//!
//! Each entry is stamped with the committer date of the last git commit that
//! touched its path (rendered in the build machine's local timezone, matching
//! how hosts display FAT wall-clock times). Paths with no git history (e.g.
//! not-yet-committed files) fall back to 1980-01-01 and are reported via a
//! `cargo:warning`.
//!
//! `fatfs` only exposes timestamps through a single global [`TimeProvider`], so
//! we back one with a thread-local that `add_dir` sets to each entry's git time
//! immediately before creating it. The build script is single-threaded, so this
//! is safe.

use std::cell::Cell;
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use fatfs::{
    Date, DateTime, Dir, FileSystem, FormatVolumeOptions, FsOptions, ReadWriteSeek, Time,
    TimeProvider,
};

/// Fixed 32-bit volume serial, matching the Python tool's default.
const VOLUME_ID: u32 = 0x1234_5678;

/// Volume label (max 11 chars, space-padded on disk).
const VOLUME_LABEL: &[u8] = b"MASS STORE";

/// FAT date/time for entries with no git history (1980-01-01 00:00:00, the
/// earliest representable FAT timestamp).
const FALLBACK: Stamp = Stamp {
    year: 1980,
    month: 1,
    day: 1,
    hour: 0,
    min: 0,
    sec: 0,
};

/// A broken-down wall-clock timestamp, already clamped to FAT's range.
#[derive(Clone, Copy)]
struct Stamp {
    year: u16,
    month: u16,
    day: u16,
    hour: u16,
    min: u16,
    sec: u16,
}

thread_local! {
    /// Timestamp handed to `fatfs` for the entry currently being created.
    /// `add_dir` sets this just before each `create_file` / `create_dir`.
    static CURRENT: Cell<Stamp> = const { Cell::new(FALLBACK) };
}

/// Zero-sized [`TimeProvider`] that reports whatever [`CURRENT`] holds.
#[derive(Debug)]
struct GitTimeProvider;

static GIT_TP: GitTimeProvider = GitTimeProvider;

impl TimeProvider for GitTimeProvider {
    fn get_current_date(&self) -> Date {
        let s = CURRENT.with(Cell::get);
        Date {
            year: s.year,
            month: s.month,
            day: s.day,
        }
    }

    fn get_current_date_time(&self) -> DateTime {
        let s = CURRENT.with(Cell::get);
        DateTime {
            date: Date {
                year: s.year,
                month: s.month,
                day: s.day,
            },
            time: Time {
                hour: s.hour,
                min: s.min,
                sec: s.sec,
                millis: 0,
            },
        }
    }
}

/// Build a `size`-byte FAT image whose root directory mirrors `src_dir`,
/// stamping entries with git commit times resolved against the `repo_dir` work
/// tree.
///
/// `size` must be a multiple of the 512-byte sector size. Panics (the right move
/// in a build script) on any error, including the tree not fitting in `size`.
pub fn build(src_dir: &Path, size: usize, repo_dir: &Path) -> Vec<u8> {
    assert!(
        size % 512 == 0,
        "image size {size} is not a multiple of the 512-byte sector size"
    );

    // `fatfs` works against any seekable byte sink; an in-memory zeroed buffer
    // gives us the raw image directly. The FAT type is auto-selected from the
    // resulting cluster count (FAT12 for small volumes like the default 256 KiB).
    let mut cursor = Cursor::new(vec![0u8; size]);

    fatfs::format_volume(
        &mut cursor,
        FormatVolumeOptions::new()
            .bytes_per_sector(512)
            .fats(2)
            .max_root_dir_entries(512)
            .volume_id(VOLUME_ID)
            .volume_label(volume_label()),
    )
    .expect("failed to format FAT volume");

    let mut uncommitted = Vec::new();
    {
        // Borrow `cursor` so we can reclaim its buffer once the filesystem is
        // unmounted (which flushes all pending writes).
        let fs = FileSystem::new(&mut cursor, FsOptions::new().time_provider(&GIT_TP))
            .expect("failed to open FAT filesystem");
        add_dir(&fs.root_dir(), src_dir, repo_dir, &mut uncommitted);
        fs.unmount().expect("failed to flush FAT filesystem");
    }

    if !uncommitted.is_empty() {
        println!(
            "cargo:warning={} path(s) under assets/ had no git history; used the \
             1980-01-01 fallback timestamp for them",
            uncommitted.len()
        );
    }

    cursor.into_inner()
}

/// Recursively copy the contents of `path` into the FAT directory `dir`.
///
/// Entries are processed in sorted order, each stamped with its git commit time
/// just before creation. Long names are handled transparently by `fatfs` (which
/// also emits the 8.3 short alias).
fn add_dir<T: ReadWriteSeek>(
    dir: &Dir<T>,
    path: &Path,
    repo_dir: &Path,
    uncommitted: &mut Vec<PathBuf>,
) {
    let mut entries: Vec<_> = fs::read_dir(path)
        .unwrap_or_else(|e| panic!("cannot read directory {}: {e}", path.display()))
        .map(|e| e.expect("failed to read directory entry"))
        .collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let os_name = entry.file_name();
        let name = os_name
            .to_str()
            .unwrap_or_else(|| panic!("non-UTF-8 file name: {os_name:?}"));
        let file_type = entry
            .file_type()
            .unwrap_or_else(|e| panic!("cannot stat {}: {e}", entry.path().display()));
        assert!(
            !file_type.is_symlink(),
            "symlinks are not supported: {}",
            entry.path().display()
        );

        // Stamp this entry with its git commit time before creating it, so the
        // shared TimeProvider reports the right value for the new dir entry.
        let stamp = git_stamp(repo_dir, &entry.path()).unwrap_or_else(|| {
            uncommitted.push(entry.path());
            FALLBACK
        });
        CURRENT.with(|c| c.set(stamp));

        if file_type.is_dir() {
            let sub = dir
                .create_dir(name)
                .unwrap_or_else(|e| panic!("failed to create dir {name:?}: {e:?}"));
            add_dir(&sub, &entry.path(), repo_dir, uncommitted);
        } else if file_type.is_file() {
            let data = fs::read(entry.path())
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", entry.path().display()));
            let mut file = dir
                .create_file(name)
                .unwrap_or_else(|e| panic!("failed to create file {name:?}: {e:?}"));
            file.write_all(&data)
                .unwrap_or_else(|e| panic!("failed to write {name:?}: {e:?}"));
            file.flush()
                .unwrap_or_else(|e| panic!("failed to flush {name:?}: {e:?}"));
        } else {
            panic!("unsupported file type: {}", entry.path().display());
        }
    }
}

/// Committer date of the last commit touching `file`, broken down in local time.
///
/// Returns `None` (caller falls back) when the path has no history, isn't in a
/// git work tree, or git is unavailable. Letting git format the date avoids
/// pulling in a calendar/timezone dependency and naturally honours the build
/// machine's timezone (via `format-local`).
fn git_stamp(repo_dir: &Path, file: &Path) -> Option<Stamp> {
    let abs = file.canonicalize().ok()?;
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args([
            "log",
            "-1",
            "--format=%cd",
            "--date=format-local:%Y %m %d %H %M %S",
            "--",
        ])
        .arg(&abs)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    parse_stamp(stdout.trim())
}

/// Parse `"YYYY MM DD HH MM SS"` into a [`Stamp`], clamping the year to FAT's
/// representable range (1980..=2107).
fn parse_stamp(s: &str) -> Option<Stamp> {
    let mut fields = s.split_whitespace();
    let mut next = || fields.next().and_then(|f| f.parse::<u16>().ok());
    let year = next()?.clamp(1980, 2107);
    let month = next()?;
    let day = next()?;
    let hour = next()?;
    let min = next()?;
    let sec = next()?;
    Some(Stamp {
        year,
        month,
        day,
        hour,
        min,
        sec,
    })
}

/// Pack [`VOLUME_LABEL`] into the fixed 11-byte, space-padded on-disk field.
fn volume_label() -> [u8; 11] {
    let mut label = [b' '; 11];
    let n = VOLUME_LABEL.len().min(11);
    label[..n].copy_from_slice(&VOLUME_LABEL[..n]);
    label
}
