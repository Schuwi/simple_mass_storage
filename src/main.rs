#![deny(unsafe_code)]
#![no_main]
#![no_std]


// Print panic message to probe console
use panic_probe as _;

use defmt_rtt as _;


#[rtic::app(device = stm32f4xx_hal::pac, peripherals = true, dispatchers = [ADC])]
mod app {
    use stm32f4xx_hal::{
        otg_fs::USB, prelude::*
    };

    #[shared]
    struct Shared {}

    // Local resources go here
    #[local]
    struct Local {}

    #[init]
    fn init(mut ctx: init::Context) -> (Shared, Local) {
        (
            Shared {
               // Initialization of shared resources go here
            },
            Local {
                // Initialization of local resources go here
            },
        )
    }

    // Optional idle, can be removed if not needed.
    #[idle]
    fn idle(_: idle::Context) -> ! {
        loop {
            continue;
        }
    }
}
