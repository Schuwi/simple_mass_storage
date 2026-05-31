//! The emUSB-Device-MSD storage backend: a read-only block device served from the
//! embedded, ZX0-compressed FAT image. Implements [`sys::USB_MSD_STORAGE_API`] by
//! decompressing one chunk at a time (see [`ImageStore`]).
//!
//! All callbacks run synchronously from `USBD_MSD_Task` in RTIC `#[idle]` -- never
//! from interrupt context -- so the single resident [`ImageStore`] is reached
//! through an [`AtomicPtr`] set once at init. This deliberately avoids holding a
//! critical section across the (relatively slow) decompression, which would
//! otherwise stall the OTG_FS interrupt and the SysTick tick.

use core::ffi::{c_schar, c_uchar, c_ulong, c_void};
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use crate::image;
use crate::IMAGE_BLOCK_SIZE;
use crate::IMAGE_BLOCKS_NUM;

use super::sys;

/// Serves the read-only image by decompressing one ZX0 chunk at a time into a
/// single reusable buffer that doubles as the decode window and the chunk cache.
/// Sequential reads stay within a cached chunk; crossing a chunk boundary
/// triggers exactly one decompression.
///
/// (Moved here from the RTIC app module when the SCSI handling migrated to the
/// SEGGER MSD class; the chunk-caching behaviour is unchanged.)
pub struct ImageStore {
    /// Decode window + resident copy of the currently cached chunk.
    buf: &'static mut [u8; image::CHUNK_SIZE],
    /// Index of the chunk currently held in `buf`, if any.
    cached: Option<usize>,
    /// Decompressed length of the cached chunk (always `CHUNK_SIZE` here, since
    /// the 4 MiB image is an exact multiple of the chunk size).
    cached_len: usize,
    /// Core clock, used to report each decompression's wall-clock time. The DWT
    /// cycle counter must be enabled (done in `init`) for this to work.
    clock_hz: u32,
}

impl ImageStore {
    pub fn new(buf: &'static mut [u8; image::CHUNK_SIZE], clock_hz: u32) -> Self {
        Self {
            buf,
            cached: None,
            cached_len: 0,
            clock_hz,
        }
    }

    /// Returns the fully decompressed bytes of chunk `index`, decompressing it
    /// into `buf` first unless it is already cached. Every actual decompression
    /// is timed with the DWT cycle counter and logged; a cache hit does nothing.
    fn chunk(&mut self, index: usize) -> &[u8] {
        if self.cached != Some(index) {
            let (offset, len) = image::CHUNK_TABLE[index];
            let compressed = &image::COMPRESSED[offset as usize..(offset + len) as usize];

            let start = cortex_m::peripheral::DWT::cycle_count();
            let decoded = zx0_decompress::decompress_into(compressed, self.buf.as_mut_slice())
                .expect("embedded ZX0 chunk decodes into CHUNK_SIZE buffer");
            let cycles = cortex_m::peripheral::DWT::cycle_count().wrapping_sub(start);

            self.cached_len = decoded.len();
            self.cached = Some(index);

            defmt::info!(
                "ZX0 decode chunk {}: {} B in {} cycles (~{} us)",
                index,
                self.cached_len,
                cycles,
                (u64::from(cycles) * 1_000_000 / u64::from(self.clock_hz)) as u32,
            );
        }
        &self.buf[..self.cached_len]
    }

    /// Fill `dst` with the image bytes for `num_sectors` starting at
    /// `sector_index`, decompressing chunks on demand and crossing chunk
    /// boundaries as needed. Reads past the end of the image are zero-filled
    /// (defensive; the host should never request them given the reported
    /// capacity).
    fn read_sectors(&mut self, sector_index: u32, dst: &mut [u8]) {
        let mut pos = sector_index as usize * IMAGE_BLOCK_SIZE as usize;
        let mut off = 0;
        while off < dst.len() {
            if pos >= image::IMAGE_LEN {
                dst[off..].fill(0);
                break;
            }
            let chunk_index = pos / image::CHUNK_SIZE;
            let in_chunk = pos % image::CHUNK_SIZE;
            let chunk = self.chunk(chunk_index);
            let n = core::cmp::min(dst.len() - off, chunk.len() - in_chunk);
            dst[off..off + n].copy_from_slice(&chunk[in_chunk..in_chunk + n]);
            off += n;
            pos += n;
        }
    }
}

/// Pointer to the single resident [`ImageStore`], published once by [`install`]
/// and read only from task context. Null until installed.
static STORE: AtomicPtr<ImageStore> = AtomicPtr::new(core::ptr::null_mut());

/// Hand the storage backend its [`ImageStore`]. Call once from `#[init]` before
/// the stack is started. Takes a `&'static mut` and keeps it as a raw pointer;
/// the reference is never reconstructed except in the storage callbacks (which
/// run only from `USBD_MSD_Task`), so there is no aliasing.
pub fn install(store: &'static mut ImageStore) {
    STORE.store(store as *mut ImageStore, Ordering::SeqCst);
}

/// Run `f` with the resident store, if installed.
///
/// SAFETY invariant: only ever called from `USBD_MSD_Task` (RTIC `#[idle]`), so
/// the reconstructed `&mut` is the only live reference to the store.
fn with_store<R>(f: impl FnOnce(&mut ImageStore) -> R) -> Option<R> {
    let p = STORE.load(Ordering::SeqCst);
    if p.is_null() {
        None
    } else {
        Some(f(unsafe { &mut *p }))
    }
}

// ---------------------------------------------------------------------------
// USB_MSD_STORAGE_API callbacks
// ---------------------------------------------------------------------------

// The transfer buffer the MSD class fills via `pfRead`. The class obtains it
// through `pfGetReadBuffer` (and `pfGetWriteBuffer`); SEGGER hands it to us once
// in `pfInit` as `DriverData.pSectorBuffer` (set up in `segger::start`). Captured
// here so the buffer hooks can return it. Only touched from task context.
static XFER_BUF: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static XFER_BUF_SECTORS: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn pf_init(_lun: c_uchar, driver_data: *const sys::USB_MSD_INST_DATA_DRIVER) {
    if !driver_data.is_null() {
        let d = unsafe { &*driver_data };
        XFER_BUF.store(d.pSectorBuffer as *mut u8, Ordering::Relaxed);
        XFER_BUF_SECTORS.store(d.NumBytes4Buffer / IMAGE_BLOCK_SIZE, Ordering::Relaxed);
    }
}

/// Hand the class the transfer buffer and the max sectors it holds. The class
/// then calls `pfRead`/`pfWrite` with this buffer. Required for both read and
/// write flows (a null hook is a hard fault on the host's first access).
unsafe extern "C" fn pf_get_buffer(
    _lun: c_uchar,
    _sector_index: c_ulong,
    pp_data: *mut *mut c_void,
    num_sectors: c_ulong,
) -> c_ulong {
    let buf = XFER_BUF.load(Ordering::Relaxed);
    if !pp_data.is_null() {
        unsafe { *pp_data = buf as *mut c_void };
    }
    let cap = XFER_BUF_SECTORS.load(Ordering::Relaxed);
    if buf.is_null() || cap == 0 {
        return 0;
    }
    core::cmp::min(num_sectors, cap)
}

unsafe extern "C" fn pf_get_info(_lun: c_uchar, p_info: *mut sys::USB_MSD_INFO) {
    if !p_info.is_null() {
        unsafe {
            (*p_info).NumSectors = IMAGE_BLOCKS_NUM as c_ulong;
            (*p_info).SectorSize = IMAGE_BLOCK_SIZE as u16;
        }
    }
}

unsafe extern "C" fn pf_read(
    _lun: c_uchar,
    sector_index: c_ulong,
    p_data: *mut c_void,
    num_sectors: c_ulong,
) -> c_schar {
    let len = num_sectors as usize * IMAGE_BLOCK_SIZE as usize;
    if p_data.is_null() || len == 0 {
        return 0;
    }
    let dst = unsafe { core::slice::from_raw_parts_mut(p_data as *mut u8, len) };
    match with_store(|s| s.read_sectors(sector_index, dst)) {
        Some(()) => 0,
        None => -1, // store not installed -> report a read error
    }
}

unsafe extern "C" fn pf_write(
    _lun: c_uchar,
    _sector_index: c_ulong,
    _p_data: *const c_void,
    _num_sectors: c_ulong,
) -> c_schar {
    // The unit is added write-protected, so this should never run.
    -1
}

unsafe extern "C" fn pf_medium_is_present(_lun: c_uchar) -> c_schar {
    1
}

unsafe extern "C" fn pf_deinit(_lun: c_uchar) {}

/// Storage driver vtable handed to `USBD_MSD_AddUnit`. Read paths only; the
/// zero-copy read/write-buffer hooks are left unset so the stack uses `pfRead`.
pub static STORAGE_API: sys::USB_MSD_STORAGE_API = sys::USB_MSD_STORAGE_API {
    pfInit: Some(pf_init),
    pfGetInfo: Some(pf_get_info),
    pfGetReadBuffer: Some(pf_get_buffer),
    pfRead: Some(pf_read),
    pfGetWriteBuffer: Some(pf_get_buffer),
    pfWrite: Some(pf_write),
    pfMediumIsPresent: Some(pf_medium_is_present),
    pfDeInit: Some(pf_deinit),
};
