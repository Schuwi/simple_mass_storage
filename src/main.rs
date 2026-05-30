#![deny(unsafe_code)]
#![no_main]
#![no_std]

// Print panic message to probe console
use panic_probe as _;

use defmt_rtt as _;

// https://pid.codes
const USB_VID: u16 = 0x1209;
// Test PID - MUST NOT be be used on any device that will be redistributed, sold, or manufactured
// TODO apply for our own PID before proper release
const USB_PID: u16 = 0x0001;

const IMAGE_BLOCK_SIZE: u32 = 512;
const IMAGE_BLOCKS_NUM: u32 = 512;

const IMAGE_SIZE: usize = (IMAGE_BLOCKS_NUM * IMAGE_BLOCK_SIZE) as usize;
// Built from `assets/` at compile time by build.rs (see build/fat_image.rs),
// with entry timestamps taken from git history, and emitted to
// `$OUT_DIR/image.img`. The `[u8; IMAGE_SIZE]` annotation makes a size mismatch
// a compile error, so IMAGE_SIZE here must match build.rs.
static IMAGE: &[u8; IMAGE_SIZE] = include_bytes!(concat!(env!("OUT_DIR"), "/image.img"));

#[rtic::app(device = stm32f4xx_hal::pac, peripherals = true, dispatchers = [ADC])]
mod app {
    use static_cell::{ConstStaticCell, StaticCell};
    use stm32f4xx_hal::{
        otg_fs::{UsbBus, UsbBusType, USB},
        prelude::*,
        rcc,
    };
    use usb_device::{
        bus::UsbBusAllocator,
        device::{StringDescriptors, UsbDevice, UsbDeviceBuilder, UsbVidPid},
        LangID,
    };
    use usbd_storage::{
        subclass::scsi::{Scsi, ScsiCommand},
        transport::{
            bbb::{BulkOnly, BulkOnlyError},
            TransportError,
        },
    };

    use crate::{IMAGE, IMAGE_BLOCK_SIZE, IMAGE_BLOCKS_NUM, USB_PID, USB_VID};

    type ScsiType = Scsi<BulkOnly<'static, UsbBusType, &'static mut [u8]>>;

    #[derive(Default)]
    struct ScsiState {
        storage_offset: usize,
        sense_key: Option<u8>,
        sense_key_code: Option<u8>,
        sense_qualifier: Option<u8>,
    }

    impl ScsiState {
        fn reset(&mut self) {
            *self = ScsiState::default();
        }
    }

    #[shared]
    struct Shared {
        usb_dev: UsbDevice<'static, UsbBusType>,
    }

    // Local resources go here
    #[local]
    struct Local {
        usb_scsi: ScsiType,
        scsi_state: ScsiState,
    }

    #[init]
    fn init(ctx: init::Context) -> (Shared, Local) {
        // `UsbBus::new` needs a `&'static mut` reference
        // use `ConstStaticCell` instead of primitive `static mut` to avoid unsafe code
        static EP_MEMORY: ConstStaticCell<[u32; 1024]> = ConstStaticCell::new([0; 1024]);
        static USB_BUS: StaticCell<UsbBusAllocator<UsbBusType>> = StaticCell::new();

        static SCSI_BUF: ConstStaticCell<[u8; 1024]> = ConstStaticCell::new([0; 1024]);

        let periph = ctx.device;

        // Configure necessary clock tree, will panic if configuration is not legal for hardware
        let mut rcc = periph.RCC.freeze(
            // instruct uC to use external Xtal instead of internal RC oscillator and give it's frequency
            rcc::Config::hse(25.MHz()) // HSE = High-Speed External oscillator
                .sysclk(48.MHz()) // main system clock (CPU core frequency, ...)
                .require_pll48clk(), // needed by OTG_FS peripheral
        );

        let gpioa = periph.GPIOA.split(&mut rcc);

        let usb = USB {
            usb_global: periph.OTG_FS_GLOBAL,
            usb_device: periph.OTG_FS_DEVICE,
            usb_pwrclk: periph.OTG_FS_PWRCLK,
            pin_dm: gpioa.pa11.into(),
            pin_dp: gpioa.pa12.into(),
            hclk: rcc.clocks.hclk(),
        };

        let usb_bus = USB_BUS.init(UsbBus::new(usb, EP_MEMORY.take()));

        let usb_scsi = usbd_storage::subclass::scsi::Scsi::new(
            usb_bus,
            64,
            0, // only one drive, max index = 0
            SCSI_BUF.take().as_mut_slice(),
        )
        .expect("valid packet_size choice and SCSI_BUF.len() > packet_size");

        let usb_dev = UsbDeviceBuilder::new(usb_bus, UsbVidPid(USB_PID, USB_VID))
            .strings(&[StringDescriptors::new(LangID::EN)
                .manufacturer(env!("CARGO_PKG_AUTHORS"))
                .product(env!("CARGO_PKG_NAME"))
                .serial_number(stm32_device_signature::device_id_hex())])
            .expect("only one language")
            .build();
        // no device class set, only interface class

        defmt::info!("Initialized");

        (
            Shared { usb_dev },
            Local {
                usb_scsi,
                scsi_state: ScsiState::default(),
            },
        )
    }

    #[task(binds=OTG_FS, shared=[usb_dev], local=[usb_scsi, scsi_state])]
    fn usb_fs(cx: usb_fs::Context) {
        let mut usb_dev = cx.shared.usb_dev;
        let usb_fs::LocalResources {
            usb_scsi,
            scsi_state,
            ..
        } = cx.local;

        let activity = usb_dev.lock(|usb_dev| usb_dev.poll(&mut [usb_scsi]));

        if activity {
            if let Err(usb_err) = usb_scsi.poll(|command| {
                if let Err(err) = process_command(command, scsi_state) {
                    defmt::error!("USB SCSI command error: {}", err);
                }
            }) {
                defmt::error!("USB SCSI error: {}", usb_err);
            }
        }
    }

    fn process_command(
        mut command: usbd_storage::subclass::Command<ScsiCommand, ScsiType>,
        scsi_state: &mut ScsiState,
    ) -> Result<(), TransportError<BulkOnlyError>> {
        defmt::debug!("Handling: {:#X}", command.kind);

        // TODO: proper SBC-2 conformant device handler (e.g. implement missing commands)
        match command.kind {
            ScsiCommand::TestUnitReady { .. } => {
                command.pass();
            }
            ScsiCommand::Inquiry { .. } => {
                command.try_write_data_all(&[
                    0x00, // periph qualifier, periph device type
                    0x80, // Removable
                    0x04, // SPC-2 compliance
                    0x02, // NormACA, HiSu, Response data format
                    0x20, // 36 bytes in total
                    0x00, // additional fields, none set
                    0x00, // additional fields, none set
                    0x00, // additional fields, none set
                    b'S', b'C', b'H', b'U', b'W', b'I', b' ', b' ', // 8-byte T-10 vendor id
                    b'S', b'h', b'a', b'r', b'e', b'd', b' ', b'S', b'e', b'c', b'r', b'e', b't',
                    b's', b' ', b' ', // 16-byte product identification
                    b' ', b'0', b'.', b'1', // 4-byte product revision
                ])?;
                command.pass();
            }
            ScsiCommand::RequestSense { .. } => {
                command.try_write_data_all(&[
                    0x70,                              // RESPONSE CODE. Set to 70h for information on current errors
                    0x00,                              // obsolete
                    scsi_state.sense_key.unwrap_or(0), // Bits 3..0: SENSE KEY. Contains information describing the error.
                    0x00,
                    0x00,
                    0x00,
                    0x00, // INFORMATION. Device-specific or command-specific information.
                    0x00, // ADDITIONAL SENSE LENGTH.
                    0x00,
                    0x00,
                    0x00,
                    0x00,                                    // COMMAND-SPECIFIC INFORMATION
                    scsi_state.sense_key_code.unwrap_or(0),  // ASC
                    scsi_state.sense_qualifier.unwrap_or(0), // ASCQ
                    0x00,
                    0x00,
                    0x00,
                    0x00,
                ])?;
                scsi_state.reset();
                command.pass();
            }
            ScsiCommand::ReadCapacity10 { .. } => {
                let mut data = [0u8; 8];
                let _ = &mut data[0..4].copy_from_slice(&u32::to_be_bytes(IMAGE_BLOCKS_NUM - 1));
                let _ = &mut data[4..8].copy_from_slice(&u32::to_be_bytes(IMAGE_BLOCK_SIZE));
                command.try_write_data_all(&data)?;
                command.pass();
            }
            ScsiCommand::ReadCapacity16 { .. } => {
                let mut data = [0u8; 16];
                let _ = &mut data[0..8].copy_from_slice(&u64::from(IMAGE_BLOCKS_NUM - 1).to_be_bytes());
                let _ = &mut data[8..16].copy_from_slice(&u64::from(IMAGE_BLOCK_SIZE).to_be_bytes());
                command.try_write_data_all(&data)?;
                command.pass();
            }
            ScsiCommand::ReadFormatCapacities { .. } => {
                let mut data = [0u8; 12];
                let _ = &mut data[0..4].copy_from_slice(&[
                    0x00, 0x00, 0x00, 0x08, // capacity list length
                ]);
                let _ = &mut data[4..8].copy_from_slice(&u32::to_be_bytes(IMAGE_BLOCKS_NUM as u32)); // number of blocks
                data[8] = 0x01; //unformatted media
                let block_length_be = u32::to_be_bytes(IMAGE_BLOCK_SIZE);
                data[9] = block_length_be[1];
                data[10] = block_length_be[2];
                data[11] = block_length_be[3];

                command.try_write_data_all(&data)?;
                command.pass();
            }
            ScsiCommand::Read { lba, len } => {
                let len = len as u32;

                if scsi_state.storage_offset != (len * IMAGE_BLOCK_SIZE) as usize {
                    let start = (IMAGE_BLOCK_SIZE * lba) as usize + scsi_state.storage_offset;
                    let end = (IMAGE_BLOCK_SIZE * lba) as usize + (IMAGE_BLOCK_SIZE * len) as usize;

                    // Uncomment this in order to push data in chunks smaller than a USB packet.
                    // let end = min(start + USB_PACKET_SIZE as usize - 1, end);

                    defmt::info!("Data transfer >>>>>>>> [{}..{}]", start, end);
                    let count = command.write_data(&IMAGE[start..end])?;
                    scsi_state.storage_offset += count;
                } else {
                    command.pass();
                    scsi_state.storage_offset = 0;
                }
            }
            // ScsiCommand::Write not supported - write-protected
            ScsiCommand::ModeSense6 { .. } => {
                command.try_write_data_all(&[
                    0x03, // number of bytes that follow
                    0x00, // the media type is SBC
                    0x80, // write-protected, no cache-control bytes support
                    0x00, // no mode-parameter block descriptors
                ])?;
                command.pass();
            }
            ScsiCommand::ModeSense10 { .. } => {
                command.try_write_data_all(&[
                    0x00, // number of bytes that follow (MSB)
                    0x06, // number of bytes that follow (LSB)
                    0x00, // the media type is SBC
                    0x80, // write-protected, no cache-control bytes support
                    0x00, // reserved
                    0x00, // reserved
                    0x00, // no mode-parameter block descriptors (MSB)
                    0x00, // no mode-parameter block descriptors (LSB)
                ])?;
                command.pass();
            }
            ref unknown_scsi_kind => {
                defmt::error!("Unknown SCSI command: {:#X}", unknown_scsi_kind);

                scsi_state.sense_key.replace(0x05); // illegal request Sense Key
                scsi_state.sense_key_code.replace(0x20); // Invalid command operation ASC
                scsi_state.sense_qualifier.replace(0x00); // Invalid command operation ASCQ

                command.fail();
            }
        }

        Ok(())
    }
}
