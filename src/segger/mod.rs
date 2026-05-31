//! Integration with the vendored SEGGER emUSB-Device stack (`emUSB-Device-MSD`).
//!
//! This module owns everything `unsafe`/FFI about USB so the rest of the firmware
//! (the RTIC app in `main.rs`) stays safe and `#![deny(unsafe_code)]`-clean. It
//! provides:
//!   * the porting layer the prebuilt library calls back into -- [`os`] (the
//!     `USB_OS_*` target-OS interface), [`config`] (`USBD_X_Config` + the OTG_FS
//!     ISR), [`log`] and [`libc`]; and
//!   * a small safe facade -- [`start`], [`task`], [`dispatch_interrupt`] and the
//!     re-exported [`ImageStore`]/[`install_store`] -- that the app drives.
//!
//! See `Doc/UM09001_emUSBD.pdf`. The C-callable symbols live in the submodules;
//! Rust never calls them, the linked archive does.
#![allow(unsafe_code)]

pub mod sys;

mod config;
mod libc;
mod log;
mod os;
mod storage;

pub use os::tick as systick;
pub use storage::{install as install_store, ImageStore};

use core::ffi::c_char;

use static_cell::{ConstStaticCell, StaticCell};

use crate::{IMAGE_BLOCKS_NUM, IMAGE_BLOCK_SIZE, USB_PID, USB_VID};

/// Enable the OTG_FS peripheral clock (RCC AHB2ENR.OTGFSEN). Must be called once
/// in `#[init]` before [`start`]; the SEGGER ST driver programs the OTG core, but
/// it needs the bus clock gated on first. Kept here so the app stays unsafe-free.
pub fn enable_otg_fs_clock() {
    // SAFETY: read-modify-write of a single RCC enable bit; this is the only
    // writer of OTGFSEN and it runs once, before interrupts are unmasked.
    unsafe {
        let rcc = &*stm32f4xx_hal::pac::RCC::ptr();
        rcc.ahb2enr().modify(|_, w| w.otgfsen().set_bit());
    }
}

/// Dispatch a pending OTG_FS interrupt into the SEGGER driver. Call from the RTIC
/// `#[task(binds = OTG_FS)]` handler.
pub fn dispatch_interrupt() {
    config::dispatch_interrupt();
}

/// Run one iteration of the MSD task. Call in a loop from RTIC `#[idle]`; it
/// blocks (on `WFI`, via [`os`]) between USB transfers.
pub fn task() {
    unsafe { sys::USBD_MSD_Task() };
}

/// Bring up emUSB-Device as a single read-only MSD volume and start it.
///
/// Preconditions (done in `#[init]` before this call): the OTG_FS peripheral
/// clock is enabled, PA11/PA12 are in alternate function 10, SysTick is ticking,
/// and [`install_store`] has been called. Mirrors the reference sequence in
/// SEGGER's `USB_MSD_FS_Start.c`, adapted for Full-Speed and our custom storage.
pub fn start() {
    // Buffers and descriptors the stack keeps pointers to must be 'static.
    static DEVICE_INFO: StaticCell<sys::USB_DEVICE_INFO> = StaticCell::new();
    static LUN_INFO: StaticCell<sys::USB_MSD_LUN_INFO> = StaticCell::new();
    // OUT endpoint buffer (one Full-Speed bulk max packet).
    static OUT_BUF: ConstStaticCell<[u8; sys::USB_FS_BULK_MAX_PACKET_SIZE as usize]> =
        ConstStaticCell::new([0; sys::USB_FS_BULK_MAX_PACKET_SIZE as usize]);
    // Sector transfer buffer the MSD class fills via `pfRead` (16 sectors).
    static SECTOR_BUF: ConstStaticCell<[u8; 16 * IMAGE_BLOCK_SIZE as usize]> =
        ConstStaticCell::new([0; 16 * IMAGE_BLOCK_SIZE as usize]);

    let device_info = DEVICE_INFO.init(sys::USB_DEVICE_INFO {
        VendorId: USB_VID,
        ProductId: USB_PID,
        sVendorName: c"Schuwi".as_ptr() as *const c_char,
        sProductName: c"STM32 Archive".as_ptr() as *const c_char,
        // 12+ chars for Mass Storage Device Bootability compliance.
        sSerialNumber: c"000000000001".as_ptr() as *const c_char,
    });

    let lun_info = LUN_INFO.init(sys::USB_MSD_LUN_INFO {
        pVendorName: c"SCHUWI".as_ptr() as *const c_char,
        pProductName: c"STM32 Archive".as_ptr() as *const c_char,
        pProductVer: c"0.1".as_ptr() as *const c_char,
        pSerialNo: c"000000000001".as_ptr() as *const c_char,
    });

    let out_buf = OUT_BUF.take();
    let sector_buf = SECTOR_BUF.take();

    unsafe {
        sys::USBD_Init();

        // Bulk IN / OUT endpoints.
        let ep_in_info = sys::USB_ADD_EP_INFO {
            MaxPacketSize: sys::USB_FS_BULK_MAX_PACKET_SIZE,
            Interval: 0,
            Flags: 0,
            InDir: sys::USB_DIR_IN as u8,
            TransferType: sys::USB_TRANSFER_TYPE_BULK as u8,
            ISO_Type: 0,
        };
        let ep_out_info = sys::USB_ADD_EP_INFO {
            InDir: sys::USB_DIR_OUT as u8,
            ..ep_in_info
        };
        let ep_in = sys::USBD_AddEPEx(&ep_in_info, core::ptr::null_mut(), 0);
        let ep_out =
            sys::USBD_AddEPEx(&ep_out_info, out_buf.as_mut_ptr(), out_buf.len() as u32);

        let init_data = sys::USB_MSD_INIT_DATA {
            EPIn: ep_in as u8,
            EPOut: ep_out as u8,
            InterfaceNum: 0,
        };

        sys::USBD_SetDeviceInfo(device_info);
        sys::USBD_MSD_Init();
        sys::USBD_MSD_Add(&init_data);

        // Logical unit 0: our read-only, write-protected custom storage.
        let inst = sys::USB_MSD_INST_DATA {
            pAPI: &storage::STORAGE_API,
            DriverData: sys::USB_MSD_INST_DATA_DRIVER {
                NumSectors: IMAGE_BLOCKS_NUM,
                SectorSize: IMAGE_BLOCK_SIZE as u16,
                pSectorBuffer: sector_buf.as_mut_ptr() as *mut core::ffi::c_void,
                NumBytes4Buffer: sector_buf.len() as u32,
                ..Default::default()
            },
            IsPresent: 1,
            IsWriteProtected: 1,
            pLunInfo: lun_info,
            ..Default::default()
        };
        sys::USBD_MSD_AddUnit(&inst);

        sys::USBD_Start();
    }
}
