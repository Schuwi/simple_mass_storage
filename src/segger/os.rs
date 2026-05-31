//! emUSB-Device OS abstraction layer (`USB_OS_*`), implemented for this bare-metal
//! RTIC firmware with no RTOS. See SEGGER UM09001 "Target OS Interface" and the
//! reference `USB_OS_embOSv5.c`; the contracts mirror those exactly.
//!
//! Threading model: the stack runs as a single cooperative "task" in RTIC's
//! `#[idle]` (which calls `USBD_MSD_Task`), while the OTG_FS interrupt drives the
//! driver and calls [`USB_OS_Signal`]. So:
//!   * `USB_OS_Wait`   blocks idle on `WFI` until the matching `USB_OS_Signal`.
//!   * `USB_OS_IncDI/DecRI` form a nestable critical section via PRIMASK.
//!   * the millisecond tick comes from SysTick (see [`tick`]).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::sys;

/// Number of independent signaling slots the stack uses: one per endpoint plus
/// any extra events. Taken straight from the generated config so it can never
/// drift from the value the prebuilt library was compiled with.
const NUM_EVENTS: usize = (sys::USB_NUM_EPS + sys::USB_EXTRA_EVENTS) as usize;

/// Per-slot latched transaction count + "a signal is pending" flag. This is the
/// no-RTOS equivalent of the reference port's depth-1 mailbox per endpoint.
struct Event {
    cnt: AtomicU32,
    pending: AtomicBool,
}

impl Event {
    const fn new() -> Self {
        Self {
            cnt: AtomicU32::new(0),
            pending: AtomicBool::new(false),
        }
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const EVENT_INIT: Event = Event::new();
static EVENTS: [Event; NUM_EVENTS] = [EVENT_INIT; NUM_EVENTS];

/// Free-running millisecond counter, advanced by [`tick`] from the SysTick
/// exception. Wraps after ~49 days; only used for differences/timeouts.
static TICKS: AtomicU32 = AtomicU32::new(0);

/// Nesting depth of `USB_OS_IncDI`/`USB_OS_DecRI` in task context. Only touched
/// while interrupts are disabled, so a plain relaxed counter is sufficient.
static DI_DEPTH: AtomicU32 = AtomicU32::new(0);

/// Advance the millisecond tick. Call once per ms from the SysTick handler.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// True when executing in an exception/interrupt context rather than thread mode.
#[inline]
fn in_interrupt() -> bool {
    cortex_m::peripheral::SCB::vect_active()
        != cortex_m::peripheral::scb::VectActive::ThreadMode
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn USB_OS_Init() {}

#[no_mangle]
pub extern "C" fn USB_OS_DeInit() {}

// ---------------------------------------------------------------------------
// Event signaling
// ---------------------------------------------------------------------------

/// Wake the task waiting on `ep_index` for transaction `transact_cnt`. Called
/// from the OTG_FS interrupt. Latches so a signal raised before the matching
/// `USB_OS_Wait` is not lost.
#[no_mangle]
pub extern "C" fn USB_OS_Signal(ep_index: u32, transact_cnt: u32) {
    if let Some(ev) = EVENTS.get(ep_index as usize) {
        ev.cnt.store(transact_cnt, Ordering::Relaxed);
        ev.pending.store(true, Ordering::Release);
    }
}

/// Block until [`USB_OS_Signal`] is called for `ep_index`/`transact_cnt`.
/// Ignores (discards) signals for other transaction counts, matching the
/// reference port's "loop until the value matches" mailbox semantics.
#[no_mangle]
pub extern "C" fn USB_OS_Wait(ep_index: u32, transact_cnt: u32) {
    let _ = wait_inner(ep_index, transact_cnt, None);
}

/// Like [`USB_OS_Wait`] but with a millisecond timeout. Returns 0 if signaled,
/// 1 on timeout.
#[no_mangle]
pub extern "C" fn USB_OS_WaitTimed(ep_index: u32, ms: u32, transact_cnt: u32) -> i32 {
    if wait_inner(ep_index, transact_cnt, Some(ms)) {
        0
    } else {
        1
    }
}

/// Core wait loop. Returns `true` if the matching signal arrived, `false` on
/// timeout (only possible when `timeout_ms` is `Some`).
///
/// Lost-wakeup safety: the condition is checked with interrupts masked
/// (PRIMASK), and `WFI` is executed while still masked. A USB interrupt that
/// becomes pending wakes `WFI` even though it is masked, and it only runs (and
/// thus calls `USB_OS_Signal`) once interrupts are re-enabled -- after which we
/// re-check. So no signal can slip between the check and the sleep.
fn wait_inner(ep_index: u32, transact_cnt: u32, timeout_ms: Option<u32>) -> bool {
    let Some(ev) = EVENTS.get(ep_index as usize) else {
        return true;
    };
    let start = TICKS.load(Ordering::Relaxed);
    loop {
        cortex_m::interrupt::disable();
        if ev.pending.load(Ordering::Acquire) {
            // Consume this signal regardless; only report success when it matches.
            ev.pending.store(false, Ordering::Relaxed);
            let matched = ev.cnt.load(Ordering::Relaxed) == transact_cnt;
            unsafe { cortex_m::interrupt::enable() };
            if matched {
                return true;
            }
            continue;
        }
        if let Some(ms) = timeout_ms {
            if TICKS.load(Ordering::Relaxed).wrapping_sub(start) >= ms {
                unsafe { cortex_m::interrupt::enable() };
                return false;
            }
        }
        // Sleep with interrupts still masked; a pending IRQ wakes WFI, then runs
        // once we re-enable below.
        cortex_m::asm::wfi();
        unsafe { cortex_m::interrupt::enable() };
    }
}

// ---------------------------------------------------------------------------
// Critical section (nestable interrupt disable)
// ---------------------------------------------------------------------------

/// Enter a critical region: disable interrupts (nestable). A no-op when called
/// from interrupt context, per the OS-layer contract.
#[no_mangle]
pub extern "C" fn USB_OS_IncDI() {
    if in_interrupt() {
        return;
    }
    cortex_m::interrupt::disable();
    DI_DEPTH.fetch_add(1, Ordering::Relaxed);
}

/// Leave a critical region: re-enable interrupts once the nesting count reaches
/// zero. A no-op when called from interrupt context.
#[no_mangle]
pub extern "C" fn USB_OS_DecRI() {
    if in_interrupt() {
        return;
    }
    if DI_DEPTH.fetch_sub(1, Ordering::Relaxed) == 1 {
        // Last level released: re-enable interrupts.
        unsafe { cortex_m::interrupt::enable() };
    }
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Busy-wait `ms` milliseconds against the SysTick counter, sleeping on `WFI`
/// between ticks.
#[no_mangle]
pub extern "C" fn USB_OS_Delay(ms: i32) {
    if ms <= 0 {
        return;
    }
    let start = TICKS.load(Ordering::Relaxed);
    while TICKS.load(Ordering::Relaxed).wrapping_sub(start) < ms as u32 {
        cortex_m::asm::wfi();
    }
}

/// Current system time in milliseconds.
#[no_mangle]
pub extern "C" fn USB_OS_GetTickCnt() -> u32 {
    TICKS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Mutexes -- single core, so the IncDI/DecRI critical section provides the real
// mutual exclusion and these can be trivial. (USB_NUM_MUTEXES == 1.)
// ---------------------------------------------------------------------------

#[no_mangle]
pub extern "C" fn USB_OS_MutexAlloc() -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn USB_OS_MutexFree() {}

#[no_mangle]
pub extern "C" fn USB_OS_MutexLock(_idx: i32) {}

#[no_mangle]
pub extern "C" fn USB_OS_MutexUnlock(_idx: i32) {}
