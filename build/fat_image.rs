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
use std::env;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use fatfs::{
    Date, DateTime, Dir, FatType, FileSystem, FormatVolumeOptions, FsOptions, ReadWriteSeek, Time,
    TimeProvider,
};

/// Start of the first (and only) partition, in 512-byte LBA sectors. 2048 (= 1
/// MiB) is the alignment every modern partitioner (Windows, Linux, Android)
/// defaults to, so it is the most broadly recognised. The skipped sectors are
/// all-zero and dedupe/ZX0-compress to nothing in the embedded blob, so the only
/// real cost is 1 MiB of the volume's addressable space.
const PART_START_LBA: u64 = 2048;

/// MBR partition-type byte for a FAT16 volume addressed via LBA. The LBA variant
/// (0x0E, vs the CHS 0x04/0x06) is the one removable-media hosts recognise most
/// reliably, and the layout this builder deliberately emits and verifies.
const PART_TYPE_FAT16_LBA: u8 = 0x0E;

/// Bytes per cluster we format the volume with. One sector (512 B) keeps the
/// small partition above the FAT16 cluster floor so it is FAT16 rather than
/// FAT12; see the `bytes_per_cluster` call in [`build`].
const CLUSTER_SIZE: u32 = 512;

/// Fixed 32-bit volume serial, matching the Python tool's default.
const VOLUME_ID: u32 = 0x1234_5678;

/// Volume label (max 11 chars, space-padded on disk).
const VOLUME_LABEL: &[u8] = b"STM ARCHIVE";

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

/// Build a `size`-byte disk image whose single partition holds a FAT filesystem
/// mirroring `src_dir`, stamping entries with git commit times resolved against
/// the `repo_dir` work tree.
///
/// The image carries a real MBR partition table (one partition starting at
/// [`PART_START_LBA`]) rather than the bare "superfloppy" layout (a filesystem at
/// LBA 0 with no partition table). Hosts that only mount filesystems found via a
/// partition table -- notably Android -- reject a superfloppy, so the MBR is what
/// makes the device mount there.
///
/// `size` must be a multiple of the 512-byte sector size and large enough to hold
/// the MBR plus a partition. Panics (the right move in a build script) on any
/// error, including the tree not fitting in the partition.
pub fn build(src_dir: &Path, size: usize, repo_dir: &Path, bundle_source: bool) -> Vec<u8> {
    assert!(
        size % 512 == 0,
        "image size {size} is not a multiple of the 512-byte sector size"
    );
    let total_sectors = (size / 512) as u64;
    assert!(
        total_sectors > PART_START_LBA,
        "image size {size} is too small to hold an MBR plus a partition at LBA {PART_START_LBA}"
    );
    let part_sectors = total_sectors - PART_START_LBA;
    let part_size = part_sectors as usize * 512;

    // `fatfs` works against any seekable byte sink; an in-memory zeroed buffer
    // gives us the raw partition contents directly. The FAT type is auto-selected
    // from the resulting cluster count. We format only the partition region here
    // and splice it in behind the MBR below.
    let mut cursor = Cursor::new(vec![0u8; part_size]);

    fatfs::format_volume(
        &mut cursor,
        FormatVolumeOptions::new()
            .bytes_per_sector(512)
            // Pin the cluster size to one sector. `fatfs` otherwise scales clusters
            // up for small volumes (2 KiB here) and, since the partition is < 4 MiB,
            // estimates FAT12 -- which removable-media hosts (Android especially)
            // mount least reliably. With 512-byte clusters the 3 MiB partition holds
            // ~6000 clusters, so `fatfs` auto-selects FAT16 from the cluster count.
            // (We steer the count via the cluster size rather than forcing
            // `fat_type`, which the crate flags as fragile.)
            .bytes_per_cluster(CLUSTER_SIZE)
            .fats(2)
            .max_root_dir_entries(512)
            .volume_id(VOLUME_ID)
            .volume_label(volume_label()),
    )
    .expect("failed to format FAT volume");

    let mut uncommitted = Vec::new();
    let fat_type;
    {
        // Borrow `cursor` so we can reclaim its buffer once the filesystem is
        // unmounted (which flushes all pending writes).
        let fs = FileSystem::new(&mut cursor, FsOptions::new().time_provider(&GIT_TP))
            .expect("failed to open FAT filesystem");
        // Record the auto-selected FAT type so the MBR partition entry can name it.
        fat_type = fs.fat_type();
        add_dir(&fs.root_dir(), src_dir, repo_dir, &mut uncommitted);
        // Make the device self-documenting: bundle the source that built it and a
        // file naming the build version (gated by the `source-snapshot` feature).
        // (Each `root_dir()` is a fresh temporary so no borrow outlives
        // `fs.unmount()`, which consumes `fs`.)
        if bundle_source {
            add_source_snapshot(&fs.root_dir(), repo_dir);
            add_version_file(&fs.root_dir(), repo_dir);
        }
        fs.unmount().expect("failed to flush FAT filesystem");
    }

    if !uncommitted.is_empty() {
        println!(
            "cargo:warning={} path(s) under assets/ had no git history; used the \
             1980-01-01 fallback timestamp for them",
            uncommitted.len()
        );
    }

    // Assemble the full disk: MBR in sector 0, then the formatted partition at
    // PART_START_LBA. The gap between them stays zero.
    let mut image = vec![0u8; size];
    let part_off = PART_START_LBA as usize * 512;
    image[part_off..part_off + part_size].copy_from_slice(&cursor.into_inner());
    write_mbr(&mut image, PART_START_LBA, part_sectors, fat_type);

    println!(
        "cargo:warning=MBR disk: 1 partition ({fat_type:?}) at LBA {PART_START_LBA}, \
         {part_sectors} sectors ({} KiB usable)",
        part_size / 1024
    );


    // Guard the wide-compatibility layout against regressions: cheap byte-level
    // checks on the hand-written MBR, plus an independent `fsck.fat` pass over the
    // filesystem properties.
    verify_mbr(&image, part_sectors);
    verify_filesystem(&image[part_off..part_off + part_size]);

    image
}

/// Assert the hand-written MBR describes exactly one partition with the layout we
/// emit. Reads the raw table bytes back -- independent of `fatfs`, which never
/// touches the MBR -- so a regression in [`write_mbr`] fails the build.
fn verify_mbr(image: &[u8], part_sectors: u64) {
    const TABLE: usize = 446;
    assert_eq!(
        [image[510], image[511]],
        [0x55, 0xAA],
        "MBR is missing the 0x55AA boot signature"
    );

    let entry = &image[TABLE..TABLE + 16];
    assert_eq!(
        entry[4], PART_TYPE_FAT16_LBA,
        "partition 1 type is {:#04x}, expected FAT16-LBA ({PART_TYPE_FAT16_LBA:#04x})",
        entry[4]
    );
    assert_eq!(
        u32::from_le_bytes(entry[8..12].try_into().unwrap()),
        PART_START_LBA as u32,
        "partition 1 does not start at LBA {PART_START_LBA}"
    );
    assert_eq!(
        u32::from_le_bytes(entry[12..16].try_into().unwrap()),
        part_sectors as u32,
        "partition 1 does not span the rest of the disk ({part_sectors} sectors)"
    );

    // Exactly one partition: the other three table entries must be empty.
    for i in 1..4 {
        let other = &image[TABLE + i * 16..TABLE + (i + 1) * 16];
        assert!(
            other.iter().all(|&b| b == 0),
            "MBR partition entry #{} is not empty; expected a single partition",
            i + 1
        );
    }
}

/// Validate the formatted partition with an independent FAT implementation
/// (`fsck.fat` from dosfstools), rather than re-reading it with the same `fatfs`
/// crate that wrote it. Asserts the volume is FAT16 with 512-byte clusters -- the
/// properties that keep it broadly mountable.
///
/// Skips (with a `cargo:warning`) when `SMS_SKIP_IMAGE_CHECK` is set or when no
/// `fsck.fat` is on `PATH`, so the build stays hermetic on machines without
/// dosfstools. Set the env var to silence the "not installed" warning.
fn verify_filesystem(part: &[u8]) {
    // Re-run the image build (and thus this check) when the skip toggle changes.
    println!("cargo:rerun-if-env-changed=SMS_SKIP_IMAGE_CHECK");
    if std::env::var_os("SMS_SKIP_IMAGE_CHECK").is_some() {
        println!("cargo:warning=SMS_SKIP_IMAGE_CHECK set; skipping fsck.fat image check");
        return;
    }

    // dosfstools ships the checker as `fsck.fat` (older systems: `dosfsck`). It
    // installs under /sbin, which is commonly absent from a non-root `PATH`, so
    // probe the usual sbin locations explicitly as well as the bare names.
    let Some(tool) = ["fsck.fat", "dosfsck"]
        .into_iter()
        .flat_map(|name| {
            [
                name.to_string(),
                format!("/sbin/{name}"),
                format!("/usr/sbin/{name}"),
                format!("/usr/local/sbin/{name}"),
            ]
        })
        .find(|t| Command::new(t).arg("--help").output().is_ok())
    else {
        println!(
            "cargo:warning=fsck.fat (dosfstools) not found on PATH or in /sbin; skipping \
             the independent image check. Install dosfstools or set SMS_SKIP_IMAGE_CHECK=1 \
             to silence."
        );
        return;
    };

    // fsck.fat operates on a bare filesystem, so check the partition region alone.
    let img = PathBuf::from(env::var("OUT_DIR").unwrap_or_else(|_| ".".into()))
        .join("verify_part.img");
    fs::write(&img, part).expect("failed to stage partition image for fsck.fat");

    // `-n` = read-only (answer "no" to any repair prompt); `-v` = verbose, which
    // prints the cluster size and FAT bit-width we assert on.
    let output = Command::new(&tool)
        .args(["-n", "-v"])
        .arg(&img)
        .output()
        .expect("failed to run fsck.fat");
    let report = String::from_utf8_lossy(&output.stdout);
    let _ = fs::remove_file(&img);

    // fsck.fat -v prints e.g. "512 bytes per cluster" and "2 FATs, 16 bit entries".
    assert!(
        report.contains(&format!("{CLUSTER_SIZE} bytes per cluster")),
        "fsck.fat did not confirm {CLUSTER_SIZE}-byte clusters:\n{report}"
    );
    assert!(
        report.contains("16 bit entries"),
        "fsck.fat did not confirm a 16-bit (FAT16) FAT:\n{report}"
    );
    println!("cargo:warning=fsck.fat ({tool}) verified the FAT16 image");
}

/// Write a single-partition MBR into sector 0 of `image`, describing a partition
/// that starts at `start_lba` and spans `sectors`, holding a `fat_type` volume.
///
/// The 446-byte boot-code area is left zero -- this is a data disk, not bootable.
/// Only the partition table and the 0x55AA boot signature are populated. LBA
/// fields are authoritative for every host that matters today; the CHS fields are
/// filled in with the conventional geometry purely for legacy tooling.
fn write_mbr(image: &mut [u8], start_lba: u64, sectors: u64, fat_type: FatType) {
    /// Offset of the first partition entry within the MBR.
    const TABLE: usize = 446;

    // MBR partition-type byte. The LBA-aware variants (0x0C/0x0E) are the ones
    // removable-media hosts recognise most reliably; FAT12 has no LBA variant.
    let part_type: u8 = match fat_type {
        FatType::Fat12 => 0x01,
        FatType::Fat16 => PART_TYPE_FAT16_LBA,
        FatType::Fat32 => 0x0C, // FAT32 (LBA)
    };

    let entry = &mut image[TABLE..TABLE + 16];
    entry[0] = 0x00; // status: not bootable
    let (h, s, c) = chs(start_lba);
    entry[1] = h;
    entry[2] = s;
    entry[3] = c;
    entry[4] = part_type;
    let (h, s, c) = chs(start_lba + sectors - 1);
    entry[5] = h;
    entry[6] = s;
    entry[7] = c;
    entry[8..12].copy_from_slice(&(start_lba as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&(sectors as u32).to_le_bytes());

    image[510] = 0x55;
    image[511] = 0xAA;
}

/// Convert an LBA to the packed 3-byte CHS triple `(head, sector|cyl-hi, cyl-lo)`
/// stored in an MBR partition entry, using the conventional 255-head/63-sector
/// geometry. LBAs past the CHS limit (~8 GB) clamp to the maximum value
/// (0xFE/0xFF/0xFF), the marker LBA-addressing hosts expect.
fn chs(lba: u64) -> (u8, u8, u8) {
    const HEADS: u64 = 255;
    const SPT: u64 = 63;
    let cyl = lba / (HEADS * SPT);
    if cyl > 1023 {
        return (0xFE, 0xFF, 0xFF);
    }
    let head = (lba / SPT) % HEADS;
    let sector = lba % SPT + 1;
    (
        head as u8,
        (((cyl >> 2) as u8) & 0xC0) | (sector as u8 & 0x3F),
        (cyl & 0xFF) as u8,
    )
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

/// Bundle an unpacked snapshot of the committed source (`git archive HEAD`) under
/// `source/` in the FAT root, so the device documents the exact tree that built it.
///
/// The archive is streamed through the `tar` crate in memory -- no temp files and no
/// external `tar` binary. Every entry shares HEAD's committer date (one git call),
/// since they all come from the same commit. The snapshot is generated fresh and
/// never stored in the repo, so it cannot contain itself.
fn add_source_snapshot<T: ReadWriteSeek>(root: &Dir<T>, repo_dir: &Path) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(["archive", "--format=tar", "HEAD"])
        .output()
        .expect("failed to run `git archive`");
    assert!(
        output.status.success(),
        "`git archive HEAD` failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // All snapshot entries are from HEAD; stamp them with its committer date.
    CURRENT.with(|c| c.set(head_stamp(repo_dir)));
    ensure_dir(root, "source");

    let mut archive = tar::Archive::new(Cursor::new(output.stdout));
    for entry in archive.entries().expect("failed to read git archive") {
        let mut entry = entry.expect("failed to read git archive entry");
        let entry_type = entry.header().entry_type();
        let rel = entry
            .path()
            .expect("git archive entry has no path")
            .into_owned();
        let rel = rel
            .to_str()
            .unwrap_or_else(|| panic!("non-UTF-8 path in git archive: {rel:?}"));
        let dest = format!("source/{}", rel.trim_end_matches('/'));

        if entry_type.is_dir() {
            ensure_dir(root, &dest);
        } else if entry_type.is_file() {
            if let Some((parent, _)) = dest.rsplit_once('/') {
                ensure_dir(root, parent);
            }
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .unwrap_or_else(|e| panic!("failed to read git archive entry {dest:?}: {e}"));
            let mut file = root
                .create_file(&dest)
                .unwrap_or_else(|e| panic!("failed to create file {dest:?}: {e:?}"));
            file.write_all(&data)
                .unwrap_or_else(|e| panic!("failed to write {dest:?}: {e:?}"));
            file.flush()
                .unwrap_or_else(|e| panic!("failed to flush {dest:?}: {e:?}"));
        }
        // Other entry types (symlinks etc.) are skipped; git archive of a normal
        // tree only emits regular files and directories.
    }
}

/// Write a `VERSION.txt` at the FAT root naming the build: `git describe`, the full
/// commit hash, and the commit date. Stamped with HEAD's committer date.
fn add_version_file<T: ReadWriteSeek>(root: &Dir<T>, repo_dir: &Path) {
    let unknown = || "unknown".to_string();
    let describe =
        git_output(repo_dir, &["describe", "--tags", "--always", "--dirty"]).unwrap_or_else(unknown);
    let commit = git_output(repo_dir, &["rev-parse", "HEAD"]).unwrap_or_else(unknown);
    let date = git_output(
        repo_dir,
        &[
            "log",
            "-1",
            "--format=%cd",
            "--date=format-local:%Y-%m-%d %H:%M:%S",
        ],
    )
    .unwrap_or_else(unknown);

    let contents = format!("version: {describe}\ncommit:  {commit}\ndate:    {date}\n");

    CURRENT.with(|c| c.set(head_stamp(repo_dir)));
    let mut file = root
        .create_file("VERSION.txt")
        .expect("failed to create VERSION.txt");
    file.write_all(contents.as_bytes())
        .expect("failed to write VERSION.txt");
    file.flush().expect("failed to flush VERSION.txt");
}

/// Create `rel` (a `/`-separated relative path) as a directory under `root`,
/// creating any missing intermediate directories. `fatfs`' `create_dir` opens an
/// existing directory, so creating each prefix in turn is idempotent.
fn ensure_dir<T: ReadWriteSeek>(root: &Dir<T>, rel: &str) {
    let mut prefix = String::new();
    for component in rel.split('/').filter(|c| !c.is_empty()) {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(component);
        root.create_dir(&prefix)
            .unwrap_or_else(|e| panic!("failed to create dir {prefix:?}: {e:?}"));
    }
}

/// Committer date of HEAD, broken down in local time; stamps the source snapshot.
/// Falls back to [`FALLBACK`] if git is unavailable.
fn head_stamp(repo_dir: &Path) -> Stamp {
    git_output(
        repo_dir,
        &[
            "log",
            "-1",
            "--format=%cd",
            "--date=format-local:%Y %m %d %H %M %S",
            "HEAD",
        ],
    )
    .and_then(|s| parse_stamp(&s))
    .unwrap_or(FALLBACK)
}

/// Run `git -C <repo_dir> <args...>` and return its trimmed stdout on success.
fn git_output(repo_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_dir)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
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
