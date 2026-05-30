#!/usr/bin/env python3
"""Build a raw FAT12/FAT16 disk image from an input file tree.

The image is meant to be embedded into the firmware (see `IMAGE` in
`src/main.rs`) and served read-only over USB mass storage.

Highlights
----------
* Pure standard library -- no external dependencies (git, if used for
  timestamps, is invoked as a subprocess).
* Auto-selects the FAT type (FAT12 / FAT16) from the resulting cluster
  count, which is what real operating systems key off of. This keeps the
  image maximally compatible: a small (e.g. 256 KiB) volume becomes FAT12,
  a larger one (>= ~2.1 MB) becomes FAT16. Use `--fat` to force a type.
* Recreates the whole directory tree, including subdirectories.
* Supports long file names (VFAT LFN) alongside generated 8.3 short names.
* Deterministic by default (fixed timestamps + volume id) so rebuilding the
  same tree yields a byte-identical image -- friendly to `include_bytes!`
  and version control. Use `--timestamps mtime` to stamp each entry with its
  file's real modification time, or `--timestamps git` to use the time of the
  last commit that touched each path.

  Caveat: FAT stores naked wall-clock time with no timezone. With `mtime`/`git`
  the digits are rendered in the *build machine's* local timezone by default,
  so the image bytes depend on where it is built and are not reproducible
  across timezones. Pass `--tz utc` to stamp a build-host independent UTC clock
  face instead (at the cost of the displayed time not matching local wall time).

Example
-------
    python3 tools/make_fat_image.py assets/ -o image.img --size 256K --label MYDRIVE

Then in `src/main.rs` replace the zero-filled `IMAGE` with:

    static IMAGE: [u8; IMAGE_SIZE] = *include_bytes!("../image.img");
"""

import argparse
import os
import subprocess
import sys
import time
from typing import List, Optional, Set, Tuple

# --- on-disk constants --------------------------------------------------------

BYTES_PER_SECTOR = 512          # we only support 512-byte logical sectors
NUM_FATS = 2                    # two FAT copies (standard, redundant)
RESERVED_SECTORS = 1            # just the boot sector
ROOT_ENTRY_COUNT = 512         # number of 32-byte root directory entries
DIR_ENTRY_SIZE = 32
MEDIA_DESCRIPTOR = 0xF8        # 0xF8 = non-removable / fixed disk

ATTR_READ_ONLY = 0x01
ATTR_HIDDEN = 0x02
ATTR_SYSTEM = 0x04
ATTR_VOLUME_ID = 0x08
ATTR_DIRECTORY = 0x10
ATTR_ARCHIVE = 0x20
ATTR_LONG_NAME = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID  # 0x0F

# FAT type boundaries, per the Microsoft FAT spec. The type is a function of
# the count of data clusters, NOT of anything stored in the boot sector.
FAT12_MAX_CLUSTERS = 4084       # <= 4084 clusters -> FAT12
FAT16_MAX_CLUSTERS = 65524      # 4085 .. 65524 clusters -> FAT16

# Deterministic defaults (so rebuilds are reproducible).
DEFAULT_VOLUME_ID = 0x12345678
# Fallback FAT date/time for 2021-01-01 00:00:00, used for entries that have no
# real timestamp of their own (e.g. the volume label).
DEFAULT_FAT_DATE = ((2021 - 1980) << 9) | (1 << 5) | 1
DEFAULT_FAT_TIME = 0

# FAT can only represent 1980-01-01 .. 2107-12-31.
FAT_MIN_YEAR = 1980
FAT_MAX_YEAR = 2107


class BuildError(Exception):
    """Raised for any user-facing failure (bad input, image too small, ...)."""


# --- helpers ------------------------------------------------------------------

def parse_size(text: str) -> int:
    """Parse a human size like '256K', '2M', '512' (bytes) into an int."""
    text = text.strip().upper()
    multipliers = {"K": 1024, "M": 1024 ** 2, "G": 1024 ** 3}
    if text and text[-1] in multipliers:
        value = float(text[:-1]) * multipliers[text[-1]]
    else:
        value = float(text)
    size = int(value)
    if size <= 0:
        raise BuildError(f"size must be positive, got {text!r}")
    if size % BYTES_PER_SECTOR != 0:
        raise BuildError(
            f"size {size} is not a multiple of the sector size ({BYTES_PER_SECTOR})"
        )
    return size


def ceil_div(a: int, b: int) -> int:
    return (a + b - 1) // b


def to_fat_datetime(timestamp: float, use_utc: bool = False) -> Tuple[int, int]:
    """Pack a Unix timestamp into FAT (date, time) words.

    FAT stores naked wall-clock time with no timezone attached: the date is a
    16-bit word of year-since-1980 (7 bits) / month / day, and the time is
    hours / minutes / two-second-units. Consumers render the digits as local
    time (Windows/macOS verbatim; Linux's vfat driver shifts them by the
    mounting host's timezone unless mounted `tz=UTC`).

    By default the timestamp is rendered in the *build machine's* local time,
    which mirrors what a camera or USB device would stamp but makes the bytes
    depend on the builder's timezone. Pass `use_utc=True` for a build-host
    independent (reproducible) UTC clock face instead. Timestamps outside the
    representable range are clamped.
    """
    t = time.gmtime(timestamp) if use_utc else time.localtime(timestamp)
    year = min(max(t.tm_year, FAT_MIN_YEAR), FAT_MAX_YEAR)
    fat_date = ((year - 1980) << 9) | (t.tm_mon << 5) | t.tm_mday
    fat_time = (t.tm_hour << 11) | (t.tm_min << 5) | (t.tm_sec // 2)
    return fat_date, fat_time


def ensure_git_repo(path: str) -> None:
    """Validate that `path` is inside a git work tree (and git is installed)."""
    try:
        out = subprocess.run(
            ["git", "-C", path, "rev-parse", "--is-inside-work-tree"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True,
        )
    except FileNotFoundError:
        raise BuildError("git is not installed but --timestamps git was requested")
    if out.returncode != 0 or out.stdout.strip() != "true":
        raise BuildError(
            f"--timestamps git requires the input to be inside a git work tree: {path}"
        )


def lfn_checksum(short_name_11: bytes) -> int:
    """Checksum of an 11-byte 8.3 name, used to bind LFN entries to it."""
    checksum = 0
    for byte in short_name_11:
        checksum = (((checksum & 1) << 7) + (checksum >> 1) + byte) & 0xFF
    return checksum


# --- geometry -----------------------------------------------------------------

class Geometry:
    def __init__(self, total_sectors, sectors_per_cluster, sectors_per_fat,
                 fat_type, cluster_count):
        self.total_sectors = total_sectors
        self.sectors_per_cluster = sectors_per_cluster
        self.sectors_per_fat = sectors_per_fat
        self.fat_type = fat_type            # 12 or 16
        self.cluster_count = cluster_count

    @property
    def cluster_bytes(self) -> int:
        return self.sectors_per_cluster * BYTES_PER_SECTOR

    @property
    def root_dir_sectors(self) -> int:
        return ceil_div(ROOT_ENTRY_COUNT * DIR_ENTRY_SIZE, BYTES_PER_SECTOR)

    @property
    def fat_start_sector(self) -> int:
        return RESERVED_SECTORS

    @property
    def root_dir_start_sector(self) -> int:
        return RESERVED_SECTORS + NUM_FATS * self.sectors_per_fat

    @property
    def data_start_sector(self) -> int:
        return self.root_dir_start_sector + self.root_dir_sectors


def _sectors_per_fat(total_sectors: int, spc: int, fat_type: int) -> int:
    """Microsoft fatgen103 'FATSz' approximation for the FAT size in sectors."""
    root_dir_sectors = ceil_div(ROOT_ENTRY_COUNT * DIR_ENTRY_SIZE, BYTES_PER_SECTOR)
    tmp1 = total_sectors - (RESERVED_SECTORS + root_dir_sectors)
    tmp2 = 256 * spc + NUM_FATS
    if fat_type == 16:
        tmp2 //= 2  # 16-bit FAT entries: half as many fit per sector pair
    # fat_type == 12 keeps tmp2 (entries are 1.5 bytes; the *3/2 rounding is
    # absorbed by the +tmp2-1 ceiling below, which slightly over-allocates --
    # always safe).
    return ceil_div(tmp1, tmp2)


def _cluster_count(total_sectors: int, spc: int, sectors_per_fat: int) -> int:
    root_dir_sectors = ceil_div(ROOT_ENTRY_COUNT * DIR_ENTRY_SIZE, BYTES_PER_SECTOR)
    data_sectors = (
        total_sectors
        - RESERVED_SECTORS
        - NUM_FATS * sectors_per_fat
        - root_dir_sectors
    )
    if data_sectors <= 0:
        return 0
    return data_sectors // spc


def compute_geometry(
    total_size: int,
    sectors_per_cluster: Optional[int],
    forced_fat_type: Optional[int],
    force_fat16: bool,
) -> Geometry:
    """Work out a consistent FAT geometry for the requested volume size."""
    total_sectors = total_size // BYTES_PER_SECTOR

    # Candidate cluster sizes (in sectors). If the user pinned one, only try
    # that; otherwise try increasing sizes until the cluster count fits a real
    # FAT type.
    if sectors_per_cluster is not None:
        candidates = [sectors_per_cluster]
    else:
        candidates = [1, 2, 4, 8, 16, 32, 64, 128]

    best = None  # type: Optional[Geometry]
    for spc in candidates:
        # Solve the chicken-and-egg between FAT size and cluster count by
        # iterating to a fixed point.
        probe_type = forced_fat_type or 16
        sectors_per_fat = _sectors_per_fat(total_sectors, spc, probe_type)
        for _ in range(8):
            clusters = _cluster_count(total_sectors, spc, sectors_per_fat)
            if clusters <= 0:
                break
            actual_type = _classify(clusters, forced_fat_type, force_fat16)
            new_fat = _sectors_per_fat(total_sectors, spc, actual_type)
            if new_fat == sectors_per_fat:
                break
            sectors_per_fat = new_fat

        clusters = _cluster_count(total_sectors, spc, sectors_per_fat)
        if clusters <= 0:
            continue
        fat_type = _classify(clusters, forced_fat_type, force_fat16)

        geom = Geometry(
            total_sectors=total_sectors,
            sectors_per_cluster=spc,
            sectors_per_fat=sectors_per_fat,
            fat_type=fat_type,
            cluster_count=clusters,
        )

        # Validate the cluster count against the (possibly forced) type unless
        # the user explicitly asked us to stamp FAT16 on a tiny disk.
        if force_fat16:
            return geom
        if fat_type == 12 and clusters <= FAT12_MAX_CLUSTERS:
            best = geom
            if forced_fat_type in (None, 12):
                return geom
        elif fat_type == 16 and FAT12_MAX_CLUSTERS < clusters <= FAT16_MAX_CLUSTERS:
            return geom
        # Too many clusters for this spc -> try a larger cluster.
        best = best or geom

    if best is None:
        raise BuildError(
            "volume is too small to hold a FAT filesystem; increase --size"
        )

    if forced_fat_type == 16 and best.cluster_count <= FAT12_MAX_CLUSTERS:
        raise BuildError(
            f"a {best.cluster_count}-cluster volume cannot be a valid FAT16 "
            f"(FAT16 needs > {FAT12_MAX_CLUSTERS} clusters, i.e. roughly >= 2.1 MB "
            f"at {best.cluster_bytes}-byte clusters).\n"
            "Use a larger --size, drop --fat 16 to let it pick FAT12, or pass "
            "--force-fat16 to stamp FAT16 anyway (non-standard, may not mount "
            "on all hosts)."
        )
    return best


def _classify(clusters: int, forced: Optional[int], force_fat16: bool) -> int:
    if force_fat16:
        return 16
    if forced is not None:
        return forced
    return 12 if clusters <= FAT12_MAX_CLUSTERS else 16


# --- short / long name handling -----------------------------------------------

_INVALID_SHORT_CHARS = set(b'+,;=[]" *?/\\:|<>')


def _short_char(ch: str) -> str:
    code = ord(ch)
    if code < 0x20 or code > 0x7E or code in _INVALID_SHORT_CHARS:
        return "_"
    return ch.upper()


def is_valid_8_3(name: str) -> bool:
    """True if `name` already fits a plain (upper-case) 8.3 short name."""
    if name in (".", ".."):
        return False
    base, dot, ext = name.partition(".")
    if dot and "." in ext:
        return False  # more than one dot
    if not base or len(base) > 8 or len(ext) > 3:
        return False
    for ch in base + ext:
        if _short_char(ch) != ch:  # would be altered -> not already 8.3
            return False
    return True


def make_short_name(name: str, used: Set[str]) -> bytes:
    """Generate a unique, padded 11-byte 8.3 name; '~N' suffix on collision."""
    base, _, ext = name.partition(".")
    if "." in name:  # use the last extension
        base, _, ext = name.rpartition(".")
    base_clean = "".join(_short_char(c) for c in base if c not in " .") or "_"
    ext_clean = "".join(_short_char(c) for c in ext if c not in " .")[:3]

    for n in range(1, 1_000_000):
        suffix = f"~{n}"
        stem = base_clean[: 8 - len(suffix)] + suffix
        stem = stem[:8]
        candidate = f"{stem}.{ext_clean}" if ext_clean else stem
        if candidate not in used:
            used.add(candidate)
            return _pack_8_3(stem, ext_clean)
    raise BuildError(f"could not generate a unique short name for {name!r}")


def _pack_8_3(stem: str, ext: str) -> bytes:
    return (stem.ljust(8)[:8] + ext.ljust(3)[:3]).encode("ascii")


def build_name_entries(name: str, used_short: Set[str]) -> Tuple[bytes, List[bytes]]:
    """Return (short_name_11, [lfn_entries...]) for a directory member.

    LFN entries are returned in on-disk order (to be written *before* the
    8.3 entry). If the name already fits 8.3, no LFN entries are produced.
    """
    if is_valid_8_3(name):
        base, _, ext = name.partition(".")
        short = _pack_8_3(base, ext)
        used_short.add(name.upper())
        return short, []

    short = make_short_name(name, used_short)
    checksum = lfn_checksum(short)

    # UTF-16LE code units, NUL-terminated then 0xFFFF padded to a multiple of 13.
    units = [ord(c) for c in name]
    units.append(0x0000)
    while len(units) % 13 != 0:
        units.append(0xFFFF)

    entries = []  # type: List[bytes]
    total = len(units) // 13
    for seq in range(total):
        chunk = units[seq * 13:(seq + 1) * 13]
        order = seq + 1
        if seq == total - 1:
            order |= 0x40  # mark the last logical entry (first on disk)
        entry = bytearray(32)
        entry[0] = order
        _put_utf16(entry, 1, chunk[0:5])
        entry[11] = ATTR_LONG_NAME
        entry[12] = 0
        entry[13] = checksum
        _put_utf16(entry, 14, chunk[5:11])
        entry[26] = 0
        entry[27] = 0
        _put_utf16(entry, 28, chunk[11:13])
        entries.append(bytes(entry))

    entries.reverse()  # highest sequence number first on disk
    return short, entries


def _put_utf16(buf: bytearray, off: int, units: List[int]) -> None:
    for i, unit in enumerate(units):
        buf[off + i * 2] = unit & 0xFF
        buf[off + i * 2 + 1] = (unit >> 8) & 0xFF


# --- image builder ------------------------------------------------------------

class FatImage:
    def __init__(self, geom: Geometry, label: str, volume_id: int,
                 ts_mode: str = "fixed", git_cwd: Optional[str] = None,
                 use_utc: bool = False):
        self.geom = geom
        self.label = label
        self.volume_id = volume_id
        self.data = bytearray(geom.total_sectors * BYTES_PER_SECTOR)
        self.next_free_cluster = 2
        self.ts_mode = ts_mode                      # "fixed" | "mtime" | "git"
        self.git_cwd = git_cwd                       # repo to query in "git" mode
        self.use_utc = use_utc                       # render times as UTC vs local
        self.fixed_datetime = (DEFAULT_FAT_DATE, DEFAULT_FAT_TIME)
        self.uncommitted = []  # type: List[str]     # paths with no git history

    # -- timestamps ------------------------------------------------------------

    def entry_datetime(self, path: str) -> Tuple[int, int]:
        """Resolve the packed FAT (date, time) for `path` per the chosen mode."""
        if self.ts_mode == "mtime":
            return to_fat_datetime(os.stat(path).st_mtime, self.use_utc)
        if self.ts_mode == "git":
            return self._git_datetime(path)
        return self.fixed_datetime

    def _git_datetime(self, path: str) -> Tuple[int, int]:
        """Commit time of the last commit touching `path` (committer date).

        Falls back to the fixed default for paths with no git history (e.g.
        not-yet-committed files), recording them for a summary warning.
        """
        # Use an absolute pathspec so it resolves regardless of git's -C cwd.
        out = subprocess.run(
            ["git", "-C", self.git_cwd, "log", "-1", "--format=%ct", "--",
             os.path.abspath(path)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True,
        )
        stamp = out.stdout.strip()
        if out.returncode != 0 or not stamp:
            self.uncommitted.append(path)
            return self.fixed_datetime
        return to_fat_datetime(int(stamp), self.use_utc)

    # -- cluster / FAT management ---------------------------------------------

    def alloc_cluster(self) -> int:
        cluster = self.next_free_cluster
        if cluster - 2 >= self.geom.cluster_count:
            raise BuildError(
                "out of space: the input tree does not fit in the requested "
                "image size"
            )
        self.next_free_cluster += 1
        self.set_fat_entry(cluster, self.eoc_marker())
        return cluster

    def eoc_marker(self) -> int:
        return 0xFFF if self.geom.fat_type == 12 else 0xFFFF

    def set_fat_entry(self, cluster: int, value: int) -> None:
        for copy in range(NUM_FATS):
            fat_base = (self.geom.fat_start_sector + copy * self.geom.sectors_per_fat) * BYTES_PER_SECTOR
            if self.geom.fat_type == 16:
                off = fat_base + cluster * 2
                self.data[off] = value & 0xFF
                self.data[off + 1] = (value >> 8) & 0xFF
            else:  # FAT12: 1.5 bytes per entry
                off = fat_base + cluster + (cluster // 2)
                if cluster % 2 == 0:
                    self.data[off] = value & 0xFF
                    self.data[off + 1] = (self.data[off + 1] & 0xF0) | ((value >> 8) & 0x0F)
                else:
                    self.data[off] = (self.data[off] & 0x0F) | ((value << 4) & 0xF0)
                    self.data[off + 1] = (value >> 4) & 0xFF

    def cluster_offset(self, cluster: int) -> int:
        sector = self.geom.data_start_sector + (cluster - 2) * self.geom.sectors_per_cluster
        return sector * BYTES_PER_SECTOR

    def write_chain(self, payload: bytes, first_cluster: Optional[int] = None) -> int:
        """Write `payload` across a freshly allocated cluster chain.

        If `first_cluster` is given it is used as the chain head (already
        allocated), which lets a directory reference its own start cluster in
        its '.' entry before its full size is known.
        """
        cluster_bytes = self.geom.cluster_bytes
        n_clusters = max(1, ceil_div(len(payload), cluster_bytes))

        chain = []
        if first_cluster is not None:
            chain.append(first_cluster)
        while len(chain) < n_clusters:
            chain.append(self.alloc_cluster())

        for i, cluster in enumerate(chain):
            nxt = self.eoc_marker() if i == len(chain) - 1 else chain[i + 1]
            self.set_fat_entry(cluster, nxt)
            start = i * cluster_bytes
            blob = payload[start:start + cluster_bytes]
            off = self.cluster_offset(cluster)
            self.data[off:off + len(blob)] = blob

        return chain[0]

    # -- directory entries -----------------------------------------------------

    def make_dir_entry(
        self, short_name_11: bytes, attr: int, first_cluster: int, size: int,
        fat_date: int = DEFAULT_FAT_DATE, fat_time: int = DEFAULT_FAT_TIME,
    ) -> bytes:
        entry = bytearray(32)
        entry[0:11] = short_name_11
        entry[11] = attr
        entry[12] = 0
        entry[13] = 0
        # creation time/date, last-access date, write time/date.
        entry[14] = fat_time & 0xFF
        entry[15] = (fat_time >> 8) & 0xFF
        entry[16] = fat_date & 0xFF
        entry[17] = (fat_date >> 8) & 0xFF
        entry[18] = fat_date & 0xFF
        entry[19] = (fat_date >> 8) & 0xFF
        entry[20] = 0  # high word of cluster (always 0 on FAT12/16)
        entry[21] = 0
        entry[22] = fat_time & 0xFF
        entry[23] = (fat_time >> 8) & 0xFF
        entry[24] = fat_date & 0xFF
        entry[25] = (fat_date >> 8) & 0xFF
        entry[26] = first_cluster & 0xFF
        entry[27] = (first_cluster >> 8) & 0xFF
        entry[28] = size & 0xFF
        entry[29] = (size >> 8) & 0xFF
        entry[30] = (size >> 16) & 0xFF
        entry[31] = (size >> 24) & 0xFF
        return bytes(entry)

    def build_tree(self, root_path: str) -> None:
        entries = []  # type: List[bytes]

        # Optional volume label entry in the root directory.
        if self.label:
            label = self.label.upper().encode("ascii", "replace")[:11].ljust(11)
            entries.append(self.make_dir_entry(label, ATTR_VOLUME_ID, 0, 0))

        entries.extend(self._process_children(root_path))

        max_root = ROOT_ENTRY_COUNT
        if len(entries) > max_root:
            raise BuildError(
                f"root directory needs {len(entries)} entries but only "
                f"{max_root} are available; move files into subfolders or "
                "raise ROOT_ENTRY_COUNT"
            )

        root_off = self.geom.root_dir_start_sector * BYTES_PER_SECTOR
        blob = b"".join(entries)
        self.data[root_off:root_off + len(blob)] = blob

    def _process_children(self, dir_path: str) -> List[bytes]:
        entries = []  # type: List[bytes]
        used_short = set()  # type: Set[str]

        for name in sorted(os.listdir(dir_path)):
            full = os.path.join(dir_path, name)
            if os.path.islink(full):
                raise BuildError(f"symlinks are not supported: {full}")

            short, lfn_entries = build_name_entries(name, used_short)
            fat_date, fat_time = self.entry_datetime(full)

            if os.path.isdir(full):
                start = self._write_subdir(full, short, used_short)
                entries.extend(lfn_entries)
                entries.append(
                    self.make_dir_entry(
                        short, ATTR_DIRECTORY, start, 0, fat_date, fat_time
                    )
                )
            elif os.path.isfile(full):
                with open(full, "rb") as fh:
                    payload = fh.read()
                start = self.write_chain(payload) if payload else 0
                entries.extend(lfn_entries)
                entries.append(
                    self.make_dir_entry(
                        short, ATTR_ARCHIVE, start, len(payload), fat_date, fat_time
                    )
                )
            else:
                raise BuildError(f"unsupported file type: {full}")

        return entries

    def _write_subdir(self, dir_path: str, short: bytes, _used: Set[str]) -> int:
        # Reserve the directory's first cluster up front so '.' can point at it.
        self_cluster = self.alloc_cluster()

        children = self._process_children(dir_path)

        fat_date, fat_time = self.entry_datetime(dir_path)
        dot = self.make_dir_entry(
            _pack_8_3(".", ""), ATTR_DIRECTORY, self_cluster, 0, fat_date, fat_time
        )
        # '..' points at the parent; 0 means "root" for FAT12/16.
        dotdot = self.make_dir_entry(
            _pack_8_3("..", ""), ATTR_DIRECTORY, 0, 0, fat_date, fat_time
        )
        # NOTE: parent cluster for nested dirs is fixed up below if needed.

        blob = dot + dotdot + b"".join(children)
        self.write_chain(blob, first_cluster=self_cluster)
        return self_cluster

    # -- boot sector / FAT seeding --------------------------------------------

    def write_boot_sector(self) -> None:
        bs = bytearray(BYTES_PER_SECTOR)
        bs[0:3] = bytes([0xEB, 0x3C, 0x90])              # jump
        bs[3:11] = b"MSDOS5.0"                            # OEM name
        _u16(bs, 11, BYTES_PER_SECTOR)
        bs[13] = self.geom.sectors_per_cluster
        _u16(bs, 14, RESERVED_SECTORS)
        bs[16] = NUM_FATS
        _u16(bs, 17, ROOT_ENTRY_COUNT)
        if self.geom.total_sectors < 0x10000:
            _u16(bs, 19, self.geom.total_sectors)        # TotSec16
            _u32(bs, 32, 0)                              # TotSec32
        else:
            _u16(bs, 19, 0)
            _u32(bs, 32, self.geom.total_sectors)
        bs[21] = MEDIA_DESCRIPTOR
        _u16(bs, 22, self.geom.sectors_per_fat)          # FATSz16
        _u16(bs, 24, 32)                                 # sectors per track
        _u16(bs, 26, 2)                                  # number of heads
        _u32(bs, 28, 0)                                  # hidden sectors
        bs[36] = 0x80                                    # drive number
        bs[37] = 0
        bs[38] = 0x29                                    # extended boot sig
        _u32(bs, 39, self.volume_id)
        label = (self.label or "NO NAME").upper().encode("ascii", "replace")
        bs[43:54] = label[:11].ljust(11)
        fs_type = b"FAT12   " if self.geom.fat_type == 12 else b"FAT16   "
        bs[54:62] = fs_type
        bs[510] = 0x55
        bs[511] = 0xAA
        self.data[0:BYTES_PER_SECTOR] = bs

    def seed_fats(self) -> None:
        # FAT[0] = media descriptor in the low byte, rest 1s. FAT[1] = EOC.
        self.set_fat_entry(0, 0xFF00 | MEDIA_DESCRIPTOR if self.geom.fat_type == 16
                           else 0xF00 | MEDIA_DESCRIPTOR)
        self.set_fat_entry(1, self.eoc_marker())


def _u16(buf: bytearray, off: int, value: int) -> None:
    buf[off] = value & 0xFF
    buf[off + 1] = (value >> 8) & 0xFF


def _u32(buf: bytearray, off: int, value: int) -> None:
    buf[off] = value & 0xFF
    buf[off + 1] = (value >> 8) & 0xFF
    buf[off + 2] = (value >> 16) & 0xFF
    buf[off + 3] = (value >> 24) & 0xFF


# --- CLI ----------------------------------------------------------------------

def main(argv: List[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Build a raw FAT12/FAT16 image from a directory tree.",
    )
    parser.add_argument("input", help="directory whose contents become the volume root")
    parser.add_argument("-o", "--output", default="image.img", help="output image path")
    parser.add_argument(
        "--size", default="256K",
        help="total image size, e.g. 256K, 2M (default: 256K, matching IMAGE_SIZE)",
    )
    parser.add_argument(
        "--cluster-size", type=int, default=None, metavar="SECTORS",
        help="sectors per cluster (default: auto). Must be a power of two.",
    )
    parser.add_argument("--label", default="", help="volume label (max 11 chars)")
    parser.add_argument(
        "--volume-id", type=lambda s: int(s, 0), default=DEFAULT_VOLUME_ID,
        help="32-bit volume serial (default: fixed, for reproducible builds)",
    )
    parser.add_argument(
        "--fat", choices=["auto", "12", "16"], default="auto",
        help="force a FAT type (default: auto-select from cluster count)",
    )
    parser.add_argument(
        "--force-fat16", action="store_true",
        help="stamp FAT16 even on a tiny disk (non-standard; may not mount)",
    )
    parser.add_argument(
        "--timestamps", choices=["fixed", "mtime", "git"], default="fixed",
        help="entry timestamps: 'fixed' (default, reproducible), 'mtime' (each "
             "file's modification time), or 'git' (time of the last commit that "
             "touched each path)",
    )
    parser.add_argument(
        "--tz", choices=["local", "utc"], default="local",
        help="timezone for the stored wall-clock time in --timestamps mtime/git "
             "(no effect on 'fixed'). 'local' (default) uses the build machine's "
             "timezone -- matches how hosts render the digits but makes the image "
             "depend on the builder's tz; 'utc' is build-host independent",
    )
    args = parser.parse_args(argv)

    try:
        if not os.path.isdir(args.input):
            raise BuildError(f"input is not a directory: {args.input}")
        if args.cluster_size is not None and (
            args.cluster_size < 1 or args.cluster_size & (args.cluster_size - 1)
        ):
            raise BuildError("--cluster-size must be a power of two (in sectors)")
        if len(args.label) > 11:
            raise BuildError("--label must be at most 11 characters")

        if args.timestamps == "git":
            ensure_git_repo(args.input)

        total_size = parse_size(args.size)
        forced_type = None if args.fat == "auto" else int(args.fat)

        geom = compute_geometry(
            total_size, args.cluster_size, forced_type, args.force_fat16
        )

        image = FatImage(
            geom=geom, label=args.label, volume_id=args.volume_id,
            ts_mode=args.timestamps, git_cwd=args.input,
            use_utc=(args.tz == "utc"),
        )
        image.write_boot_sector()
        image.seed_fats()
        image.build_tree(args.input)

        with open(args.output, "wb") as fh:
            fh.write(image.data)

    except BuildError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1

    if image.uncommitted:
        print(
            f"warning: {len(image.uncommitted)} path(s) had no git history; used "
            "the fixed fallback timestamp for them",
            file=sys.stderr,
        )

    used_clusters = image.next_free_cluster - 2
    print(
        f"Wrote {args.output}: {total_size} bytes, FAT{geom.fat_type}, "
        f"{geom.sectors_per_cluster} sec/cluster ({geom.cluster_bytes} B), "
        f"{geom.cluster_count} clusters total, {used_clusters} used."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
