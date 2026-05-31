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

const IMAGE_SIZE: usize = 4 * 1024 * 1024; // 4MiB
const IMAGE_BLOCK_SIZE: u32 = 512;

const IMAGE_BLOCKS_NUM: u32 = (IMAGE_SIZE / IMAGE_BLOCK_SIZE as usize) as u32;

const _: () = assert!((IMAGE_BLOCKS_NUM * IMAGE_BLOCK_SIZE) as usize == IMAGE_SIZE);

/// The embedded image, built from `assets/` at compile time by build.rs (see
/// build/fat_image.rs, with entry timestamps from git history), then split into
/// fixed-size chunks that are each ZX0-compressed independently. This generated
/// module exposes the compressed blob, the per-chunk `(offset, len)` table, and
/// `CHUNK_SIZE` / `CHUNK_COUNT` / `IMAGE_LEN`. The firmware decompresses one
/// chunk at a time on demand (see [`app::ImageStore`]).
mod image {
    include!(concat!(env!("OUT_DIR"), "/image.rs"));
}

// Keep the SCSI-reported capacity and the generated image in lockstep, and make
// sure a single buffer can serve as both the ZX0 decode window and the chunk
// cache (the buffer must exceed ZX0's maximum back-reference distance).
const _: () = assert!(image::IMAGE_LEN == IMAGE_SIZE);
const _: () = assert!(image::CHUNK_SIZE >= zx0_decompress::MIN_WINDOW_LEN);

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

    use crate::{image, IMAGE_BLOCK_SIZE, IMAGE_BLOCKS_NUM, USB_PID, USB_VID};

    type ScsiType = Scsi<BulkOnly<'static, UsbBusType, &'static mut [u8]>>;

    /// Serves the read-only image by decompressing one ZX0 chunk at a time into a
    /// single reusable buffer that doubles as the decode window and the chunk
    /// cache. Sequential reads stay within a cached chunk; crossing a chunk
    /// boundary triggers exactly one decompression.
    pub struct ImageStore {
        /// Decode window + resident copy of the currently cached chunk.
        buf: &'static mut [u8; image::CHUNK_SIZE],
        /// Index of the chunk currently held in `buf`, if any.
        cached: Option<usize>,
        /// Decompressed length of the cached chunk (`CHUNK_SIZE`, or less for a
        /// short final chunk).
        cached_len: usize,
        /// Core clock, used to report each decompression's wall-clock time. The
        /// DWT cycle counter must be enabled (done in `init`) for this to work.
        clock_hz: u32,
    }

    impl ImageStore {
        fn new(buf: &'static mut [u8; image::CHUNK_SIZE], clock_hz: u32) -> Self {
            Self {
                buf,
                cached: None,
                cached_len: 0,
                clock_hz,
            }
        }

        /// Returns the fully decompressed bytes of chunk `index`, decompressing it
        /// into `buf` first unless it is already cached.
        ///
        /// Every actual decompression is timed with the DWT cycle counter and
        /// logged, so the cost of on-demand decoding in the read path is visible on
        /// device. A cache hit does no work and logs nothing.
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
    }

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
        store: ImageStore,
    }

    #[init]
    fn init(ctx: init::Context) -> (Shared, Local) {
        // `UsbBus::new` needs a `&'static mut` reference
        // use `ConstStaticCell` instead of primitive `static mut` to avoid unsafe code
        static EP_MEMORY: ConstStaticCell<[u32; 1024]> = ConstStaticCell::new([0; 1024]);
        static USB_BUS: StaticCell<UsbBusAllocator<UsbBusType>> = StaticCell::new();

        static SCSI_BUF: ConstStaticCell<[u8; 1024]> = ConstStaticCell::new([0; 1024]);

        // Decode window + chunk cache for the compressed image (see `ImageStore`).
        static CHUNK_BUF: ConstStaticCell<[u8; image::CHUNK_SIZE]> =
            ConstStaticCell::new([0; image::CHUNK_SIZE]);

        let periph = ctx.device;
        let mut core = ctx.core;

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

        // Enable the DWT cycle counter so `ImageStore::chunk` can time every
        // on-demand decompression in the read path. Must happen before the first
        // chunk is decoded.
        core.DCB.enable_trace();
        core.DWT.enable_cycle_counter();
        let store = ImageStore::new(CHUNK_BUF.take(), rcc.clocks.sysclk().to_Hz());

        defmt::info!("Initialized");

        (
            Shared { usb_dev },
            Local {
                usb_scsi,
                scsi_state: ScsiState::default(),
                store,
            },
        )
    }

    #[task(binds=OTG_FS, shared=[usb_dev], local=[usb_scsi, scsi_state, store])]
    fn usb_fs(cx: usb_fs::Context) {
        let mut usb_dev = cx.shared.usb_dev;
        let usb_fs::LocalResources {
            usb_scsi,
            scsi_state,
            store,
            ..
        } = cx.local;

        let activity = usb_dev.lock(|usb_dev| usb_dev.poll(&mut [usb_scsi]));

        if activity {
            if let Err(usb_err) = usb_scsi.poll(|command| {
                if let Err(err) = process_command(command, scsi_state, store) {
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
        store: &mut ImageStore,
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
                    b'S', b'T', b'M', b'3', b'2', b' ', b'A', b'r', b'c', b'h', b'i', b'v', b'e',
                    b' ', b' ', b' ', // 16-byte product identification
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
                let total = len as usize * IMAGE_BLOCK_SIZE as usize;

                // Reject reads that run past the end of the medium before they can
                // index a chunk that does not exist. Use u64 so the sum cannot wrap.
                if lba as u64 + len as u64 > IMAGE_BLOCKS_NUM as u64 {
                    defmt::warn!("Read out of range: lba={}, len={}", lba, len);
                    scsi_state.sense_key.replace(0x05); // ILLEGAL REQUEST
                    scsi_state.sense_key_code.replace(0x21); // LBA OUT OF RANGE
                    scsi_state.sense_qualifier.replace(0x00);
                    scsi_state.storage_offset = 0;
                    command.fail();
                } else if scsi_state.storage_offset != total {
                    // Absolute byte offset into the decompressed image for the next
                    // byte to send. The transport calls us repeatedly, advancing
                    // `storage_offset` until the whole request is served.
                    let abs = lba as usize * IMAGE_BLOCK_SIZE as usize + scsi_state.storage_offset;
                    let chunk_index = abs / image::CHUNK_SIZE;
                    let offset_in_chunk = abs % image::CHUNK_SIZE;

                    // Decompress (or reuse the cached) chunk, then serve at most up
                    // to the chunk boundary so we never read past the cached buffer
                    // in a single call; the next call continues into the next chunk.
                    let chunk = store.chunk(chunk_index);
                    let available = chunk.len() - offset_in_chunk;
                    let remaining = total - scsi_state.storage_offset;
                    let n = available.min(remaining);

                    let count = command.write_data(&chunk[offset_in_chunk..offset_in_chunk + n])?;
                    scsi_state.storage_offset += count;
                } else {
                    command.pass();
                    scsi_state.storage_offset = 0;
                }
            }
            ScsiCommand::Write { len, .. } => {
                // The medium is write-protected, but we must still consume the
                // host's entire data-out phase before reporting status. If we
                // fail the command while bytes remain untransferred, the
                // Bulk-Only transport stalls the bulk-OUT endpoint (see
                // `end_data_transfer`); the host then needs a Clear-Halt plus
                // reset-recovery round trip before the device is usable again
                // (the stall observed on the host). So drain and discard the
                // write payload across successive polls, then fail with DATA
                // PROTECT / WRITE PROTECTED so the host learns the medium is
                // read-only and stops retrying.
                let total = len as usize * IMAGE_BLOCK_SIZE as usize;

                // Drain (and discard) whatever payload the host has sent so far.
                if scsi_state.storage_offset != total {
                    let mut scratch = [0u8; IMAGE_BLOCK_SIZE as usize];
                    let count = command.read_data(&mut scratch)?;
                    scsi_state.storage_offset += count;
                }

                // Reject the write only once the whole data-out phase has been
                // consumed. This MUST happen in the same callback that drains
                // the final byte (as the usbd-storage Write example does): once
                // the data-out phase is empty there may be no further USB
                // activity to drive another poll, so deferring the `fail()` to a
                // later callback can deadlock until the host times out and
                // resets. At this point `data_transfer_len` is 0, so failing
                // does not stall the bulk-OUT endpoint.
                if scsi_state.storage_offset == total {
                    scsi_state.sense_key.replace(0x07); // DATA PROTECT
                    scsi_state.sense_key_code.replace(0x27); // WRITE PROTECTED
                    scsi_state.sense_qualifier.replace(0x00);
                    scsi_state.storage_offset = 0;
                    command.fail();
                }
            }
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
