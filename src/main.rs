#![deny(unsafe_code)]
#![no_main]
#![no_std]

// Print panic message to probe console
use panic_probe as _;

use defmt_rtt as _;

/// USB integration built on the vendored SEGGER emUSB-Device stack. Owns all
/// FFI/`unsafe` so this crate stays `#![deny(unsafe_code)]` everywhere else.
mod segger;

// https://pid.codes
const USB_VID: u16 = 0x1209;
// Test PID - MUST NOT be be used on any device that will be redistributed, sold, or manufactured
// TODO apply for our own PID before proper release
const USB_PID: u16 = 0x0001;

const IMAGE_SIZE: usize = 4 * 1024 * 1024; // 4MiB
const IMAGE_BLOCK_SIZE: u32 = 512;

const IMAGE_BLOCKS_NUM: u32 = (IMAGE_SIZE / IMAGE_BLOCK_SIZE as usize) as u32;

const _: () = assert!((IMAGE_BLOCKS_NUM * IMAGE_BLOCK_SIZE) as usize == IMAGE_SIZE);

/// The embedded image, built from `assets/` at compile time by build.rs (see
/// build/fat_image.rs, with entry timestamps from git history), then split into
/// fixed-size chunks that are each ZX0-compressed independently. This generated
/// module exposes the compressed blob, the per-chunk `(offset, len)` table, and
/// `CHUNK_SIZE` / `CHUNK_COUNT` / `IMAGE_LEN`. The firmware decompresses one
/// chunk at a time on demand (see [`segger::ImageStore`]).
mod image {
    include!(concat!(env!("OUT_DIR"), "/image.rs"));
}

// Keep the SCSI-reported capacity and the generated image in lockstep, and make
// sure a single buffer can serve as both the ZX0 decode window and the chunk
// cache (the buffer must exceed ZX0's maximum back-reference distance).
const _: () = assert!(image::IMAGE_LEN == IMAGE_SIZE);
const _: () = assert!(image::CHUNK_SIZE >= zx0_decompress::MIN_WINDOW_LEN);

/// 1 kHz SysTick: drives the emUSB-Device millisecond time base (used for
/// enumeration timeouts and `USB_OS_Delay`). Defined out here, rather than as an
/// RTIC task, because RTIC does not own SysTick in this app.
#[cortex_m_rt::exception]
fn SysTick() {
    segger::systick();
}

#[rtic::app(device = stm32f4xx_hal::pac, peripherals = true)]
mod app {
    use cortex_m::peripheral::syst::SystClkSource;
    use static_cell::{ConstStaticCell, StaticCell};
    use stm32f4xx_hal::{gpio::Speed, prelude::*, rcc};

    use crate::{image, segger};

    #[shared]
    struct Shared {}

    #[local]
    struct Local {}

    #[init]
    fn init(ctx: init::Context) -> (Shared, Local) {
        // Decode window + chunk cache for the compressed image, and the resident
        // store that owns it (handed to the SEGGER MSD storage backend below).
        static CHUNK_BUF: ConstStaticCell<[u8; image::CHUNK_SIZE]> =
            ConstStaticCell::new([0; image::CHUNK_SIZE]);
        static STORE: StaticCell<segger::ImageStore> = StaticCell::new();

        let periph = ctx.device;
        let mut core = ctx.core;

        // Configure the clock tree; panics if illegal for the hardware. The OTG_FS
        // core needs the 48 MHz USB clock, hence `require_pll48clk()`.
        let mut rcc = periph.RCC.freeze(
            rcc::Config::hse(25.MHz()) // 25 MHz external crystal
                .sysclk(48.MHz())
                .require_pll48clk(),
        );

        // OTG_FS D-/D+ on PA11/PA12, alternate function 10, high slew rate. The
        // SEGGER ST_STM32F4xxFS driver drives the OTG core itself; we only set up
        // the pins and gate on the peripheral clock.
        let gpioa = periph.GPIOA.split(&mut rcc);
        let _pa11 = gpioa.pa11.into_alternate::<10>().speed(Speed::VeryHigh);
        let _pa12 = gpioa.pa12.into_alternate::<10>().speed(Speed::VeryHigh);
        segger::enable_otg_fs_clock();

        // Enable the DWT cycle counter so `ImageStore` can time every on-demand
        // decompression in the read path. Must happen before the first decode.
        core.DCB.enable_trace();
        core.DWT.enable_cycle_counter();

        // 1 kHz SysTick for the emUSB-Device time base (see `SysTick` above).
        let reload = rcc.clocks.sysclk().to_Hz() / 1000 - 1;
        let mut syst = core.SYST;
        syst.set_clock_source(SystClkSource::Core);
        syst.set_reload(reload);
        syst.clear_current();
        syst.enable_counter();
        syst.enable_interrupt();

        // Hand the read-only image store to the MSD storage backend, then bring up
        // and start emUSB-Device.
        let store = STORE.init(segger::ImageStore::new(
            CHUNK_BUF.take(),
            rcc.clocks.sysclk().to_Hz(),
        ));
        segger::install_store(store);
        segger::start();

        defmt::info!("Initialized (SEGGER emUSB-Device MSD)");

        (Shared {}, Local {})
    }

    /// The emUSB-Device MSD task runs cooperatively at the lowest priority; it
    /// blocks on `WFI` between transfers (see `segger::os`).
    #[idle]
    fn idle(_: idle::Context) -> ! {
        loop {
            segger::task();
        }
    }

    /// OTG_FS interrupt: hand off to the SEGGER driver's ISR, which signals the
    /// idle task when a transfer completes.
    #[task(binds = OTG_FS)]
    fn otg_fs(_: otg_fs::Context) {
        segger::dispatch_interrupt();
    }
}
