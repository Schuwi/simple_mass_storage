//! Target configuration hooks (`USBD_X_*`) and OTG_FS interrupt plumbing.
//!
//! `USBD_X_Config` is called by the stack during init to register the hardware
//! driver. We use the prebuilt `USB_Driver_ST_STM32F4xxFS` driver (the STM32F4
//! OTG_FS / Synopsys DWC2 core). The driver hands us its ISR entry point via
//! `USBD_SetISREnableFunc`; we stash it and dispatch to it from the RTIC
//! `OTG_FS` hardware task (see [`dispatch_interrupt`]).
//!
//! The OTG_FS peripheral clock and PA11/PA12 alternate-function setup are done
//! in `#[init]` before the stack starts; the driver itself programs the OTG core.

use core::sync::atomic::{AtomicUsize, Ordering};

use stm32f4xx_hal::pac::Interrupt;

use super::sys;

/// The driver's ISR entry point, captured in [`enable_isr`]. Stored as a raw
/// address (0 = not yet set) so it can be published from `USBD_X_Config` and
/// read from the interrupt with a plain atomic.
static ISR_HANDLER: AtomicUsize = AtomicUsize::new(0);

/// Called by the stack to enable USB interrupts and register the driver ISR.
extern "C" fn enable_isr(handler: sys::USB_ISR_HANDLER) {
    if let Some(f) = handler {
        ISR_HANDLER.store(f as usize, Ordering::SeqCst);
    }
    // Unmask the OTG_FS line. RTIC also manages this interrupt (it has a bound
    // task), so this is belt-and-suspenders; priority is owned by RTIC.
    unsafe { cortex_m::peripheral::NVIC::unmask(Interrupt::OTG_FS) };
}

/// `USBD_X_Config`: register the STM32F4 OTG_FS driver and our ISR-enable hook.
/// No `ConfigAddr` call is needed -- the ST FS driver knows the OTG_FS base.
#[no_mangle]
pub extern "C" fn USBD_X_Config() {
    unsafe {
        sys::USBD_AddDriver(&sys::USB_Driver_ST_STM32F4xxFS);
        sys::USBD_SetISREnableFunc(Some(enable_isr));
    }
}

/// Optional hooks used only when `USBD_OS_USE_USBD_X_INTERRUPT` is set (it is
/// not, by default), provided so the symbols resolve regardless.
#[no_mangle]
pub extern "C" fn USBD_X_EnableInterrupt() {
    unsafe { cortex_m::peripheral::NVIC::unmask(Interrupt::OTG_FS) };
}

#[no_mangle]
pub extern "C" fn USBD_X_DisableInterrupt() {
    cortex_m::peripheral::NVIC::mask(Interrupt::OTG_FS);
}

/// Dispatch a pending OTG_FS interrupt into the SEGGER driver. Call this from the
/// RTIC `#[task(binds = OTG_FS)]` handler.
pub fn dispatch_interrupt() {
    let addr = ISR_HANDLER.load(Ordering::SeqCst);
    if addr != 0 {
        // SAFETY: `addr` is a function pointer the driver gave us in `enable_isr`;
        // it is only ever set to a valid `extern "C" fn()`.
        let f: unsafe extern "C" fn() = unsafe { core::mem::transmute(addr) };
        unsafe { f() };
    }
}
