//! Matter-over-Thread thermostat firmware for an ESP32-C6 retrofitted into a
//! Mill Gen 2 panel heater, replacing its WiFi controller.
//!
//! Endpoint 1 carries two device types. The Thermostat (`0x0301`, `HEAT`) is backed
//! by [`MillHeater`], which observes rather than drives: the Mill's own
//! microcontroller keeps the temperature sensor, the triac and the control loop, and
//! this board replaces only the WiFi module that advises it over a 9600-baud UART.
//! Beside the thermostat sits an Electrical Sensor (`0x0510`) - a *utility* device
//! type, so the two share an endpoint - reporting the heating element through Power
//! Topology, Electrical Power Measurement and Electrical Energy Measurement: what
//! the element draws while the Mill has it on, and how much energy it has drawn
//! over the device's lifetime. There is no metering hardware behind either, only
//! the element's plate rating and the on/off bit the Mill reports, which is why
//! `meter.rs` serves `ActivePower` and the energy totals and nothing else.
//!
//! Endpoint 2 is where that plate rating is set. `config.toml` supplies the default,
//! but a board sealed inside a heater cannot be rebuilt to correct it, so a Mode
//! Select cluster offers the ratings Mill ships and a manufacturer-specific cluster
//! beside it takes the exact figure - see `element.rs`.
//!
//! The wire protocol is in `mill.rs` and the UART that carries it in `heater.rs`.
//!
//! The structure is lifted from `rs-matter-embassy`'s own examples: see
//! `../rs-matter-embassy/examples/esp/src/bin/light_thread.rs` for the stack wiring
//! and `light_wifi_persistent.rs` for the NVS store and factory reset.
#![no_std]
#![no_main]
// The handler chain is a deeply nested type; the stack's own examples need this too.
#![recursion_limit = "256"]

use core::borrow::BorrowMut;
use core::pin::pin;

use embassy_embedded_hal::adapter::BlockingAsync;
use embassy_executor::Spawner;
use embassy_futures::select::{select, select3, Either, Either3};

use esp_alloc::heap_allocator;
use esp_backtrace as _;
use esp_bootloader_esp_idf::partitions::{
    read_partition_table, DataPartitionSubType, PartitionType, PARTITION_TABLE_MAX_LEN,
};
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::ram;
use esp_hal::timer::timg::TimerGroup;
use esp_metadata_generated::memory_range;
use esp_storage::FlashStorage;

use log::{info, warn};

use rs_matter_embassy::matter::crypto::{default_crypto, Crypto};
use rs_matter_embassy::matter::dm::clusters::app::elec_energy_meas::{self, ElecEnergyMeasHooks};
use rs_matter_embassy::matter::dm::clusters::app::elec_pwr_meas::{self, ElecPwrMeasHooks};
use rs_matter_embassy::matter::dm::clusters::app::power_topology::{self, PowerTopologyHandler};
// Aliased: the local `thermostat` module below holds our device logic, and
// `HandlerAdaptor` is a name every generated cluster module exports.
use rs_matter_embassy::matter::dm::clusters::app::thermostat::{
    HandlerAdaptor as ThermostatHandlerAdaptor, ThermostatHandler, ThermostatHooks,
};
use rs_matter_embassy::matter::dm::clusters::basic_info::BasicInfoConfig;
use rs_matter_embassy::matter::dm::clusters::desc::{self, ClusterHandler as _};
use rs_matter_embassy::matter::dm::clusters::groups::{self, ClusterHandler as _};
use rs_matter_embassy::matter::dm::clusters::identify::{self, IdentifyHandler};
use rs_matter_embassy::matter::dm::clusters::mode_select::{self, ModeSelectHandler};
use rs_matter_embassy::matter::dm::devices::test::{DAC_PRIVKEY, TEST_DEV_ATT};
use rs_matter_embassy::matter::dm::devices::{
    DEV_TYPE_ELECTRICAL_SENSOR, DEV_TYPE_MODE_SELECT, DEV_TYPE_ROOT_NODE, DEV_TYPE_THERMOSTAT,
};
use rs_matter_embassy::matter::dm::endpoints::ROOT_ENDPOINT_ID;
use rs_matter_embassy::matter::dm::{Async, Dataver, EmptyHandler, Endpoint, Node};
use rs_matter_embassy::matter::error::Error;
use rs_matter_embassy::matter::persist::KvBlobStore;
use rs_matter_embassy::matter::sc::pase::{Spake2pVerifierPassword, Spake2pVerifierPasswordRef};
use rs_matter_embassy::matter::utils::init::InitMaybeUninit;
use rs_matter_embassy::matter::BasicCommData;
use rs_matter_embassy::matter::{clusters, devices};
use rs_matter_embassy::persist::SeqMapKvBlobStore;
use rs_matter_embassy::stack::rand::reseeding_csprng;
use rs_matter_embassy::wireless::esp::EspThreadDriver;
use rs_matter_embassy::wireless::{EmbassyThread, EmbassyThreadMatterStack};

use static_cell::StaticCell;

use tinyrlibc as _;

use crate::boot_diag::BootDiagHandler;
use crate::element::{ElementRating, ElementWattsHandler};
use crate::heater::MillHeater;
use crate::meter::{ElecEnergyDeviceLogic, ElecPwrDeviceLogic};
use crate::radio::LoggedRadio;
use crate::supervisor::Watchdog;
use crate::thermostat::ThermostatDeviceLogic;
use crate::vendor_kv::VendorKv;

mod boot_diag;
mod config;
mod element;
mod heater;
mod meter;
mod mill;
mod radio;
mod supervisor;
mod thermostat;
mod vendor_kv;

extern crate alloc;

macro_rules! mk_static {
    ($t:ty) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit()
    }};
}

/// Endpoint 0 (the root endpoint) always runs the hidden Matter system clusters, so
/// the thermostat gets ID=1.
const THERMOSTAT_ENDPOINT: u16 = 1;

/// The heating element's rating - what the meter is scaled by - lives on an endpoint
/// of its own, because Mode Select's device type is an *application* one and core
/// spec 9.2.1 allows a simple endpoint only one of those. See `element.rs`.
const ELEMENT_ENDPOINT: u16 = 2;

/// The amount of memory for allocating all `rs-matter-stack` futures created during
/// the execution of the `run*` methods. This does NOT include the rest of the Matter
/// stack.
///
/// Those futures are allocated with a small bump allocator, which results in a much
/// lower memory use than letting them sit on the program stack. If this is not
/// enough, the program panics during stack initialization - raise it until it does
/// not. 25000 is what `light_thread.rs` uses for non-concurrent commissioning.
const BUMP_SIZE: usize = 25000;

/// Heap, strictly necessary only for Thread+BLE and for the only Matter dependency
/// that needs (~4KB) alloc - `x509`.
const HEAP_SIZE: usize = 100 * 1024;

const RECLAIMED_RAM: usize =
    memory_range!("DRAM2_UNINIT").end - memory_range!("DRAM2_UNINIT").start;

/// How long the Boot Mode pin has to be held low to factory-reset the device.
const RESET_SECS: u64 = 3;

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(_s: Spawner) {
    esp_println::logger::init_logger_from_env();

    info!("Mill Matter thermostat starting...");

    heap_allocator!(size: HEAP_SIZE - RECLAIMED_RAM);
    heap_allocator!(#[ram(reclaimed)] size: RECLAIMED_RAM);

    // Necessary `esp-hal` initialization boilerplate

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // First, before the radio is up: the brownout threshold goes over the analog
    // I2C bus the PHY also uses, and a watchdog is no use if it starts late. From
    // here on, something has to feed it at least every 20 s - see `supervisor.rs`.
    supervisor::arm_brownout_reset();

    let mut watchdog = Watchdog::start(peripherals.TIMG1);

    // Create the crypto provider, using the `esp-hal` TRNG/ADC1 as the source of
    // randomness for a reseeding CSPRNG.
    let _trng_source = esp_hal::rng::TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let crypto = default_crypto(
        reseeding_csprng(esp_hal::rng::Trng::try_new().unwrap(), 1000).unwrap(),
        DAC_PRIVKEY,
    );

    let mut weak_rand = crypto.weak_rand().unwrap();

    // Reads the reset reason, and logs it, early, while the log is still short.
    let boot_diag = BootDiagHandler::new(Dataver::new_rand(&mut weak_rand));

    // Unlike the `rs-matter-embassy` examples, which randomise this per boot to
    // dodge stale SRP registrations, we derive a *stable* EUI-64 from the chip's
    // factory MAC. This device persists its commissioning, so it has to come back
    // with the same Thread and SRP identity it went down with.
    let ieee_eui64 = eui64_from_factory_mac();

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    // Allocate the Matter stack statically: its footprint is ~35-50KB, which would
    // blow the program stack, and the wireless variants require it anyway.
    let stack = mk_static!(EmbassyThreadMatterStack::<BUMP_SIZE, ()>).init_with(
        EmbassyThreadMatterStack::init(dev_det(), dev_comm(), &TEST_DEV_ATT),
    );

    // The Mill UART: GPIO16 out to the heater's MCU, GPIO17 back, 9600 8N1 (the rest
    // of `Config::default()`), no flow control. The pins are the ones wired to the
    // Mill's module header.
    //
    // They are also the ESP32-C6's *default UART0 console* pins, which is why the
    // Mill gets UART1 while the log console goes out over USB Serial/JTAG - see the
    // `esp-println` features in `Cargo.toml`. The ROM bootloader still says its
    // piece on UART0 at reset, before any of this runs; those bytes do reach the
    // Mill's MCU, framed as nothing it understands.
    let mill_uart = esp_hal::uart::Uart::new(
        peripherals.UART1,
        esp_hal::uart::Config::default().with_baudrate(mill::BAUD_RATE),
    )
    .unwrap()
    .with_tx(peripherals.GPIO16)
    .with_rx(peripherals.GPIO17)
    .into_async();

    // The NVS-backed KV store. `get_persistent_store` only needs a small scratch
    // buffer to parse the partition table, so we give it a local one.
    let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut store = get_persistent_store(peripherals.FLASH, &mut pt_buf[..]);

    // Re-hydrate the `Matter` instance (fabrics, basic info, RTC) and open the basic
    // commissioning window if this device has no fabrics yet.
    stack.startup(&crypto, &mut store).await.unwrap();

    if stack.matter().has_fabrics() {
        info!(
            "To factory-reset, press and hold the Boot Mode pin (GPIO9) for {} or more seconds",
            RESET_SECS
        );
    }

    {
        // `kv` only borrows the store (the blanket `&mut T: KvBlobStore`), so
        // dropping it at the end of this scope hands the store back for the factory
        // reset below.
        let kv = stack.matter().kv(&mut store);

        // The heater, and the three cluster device-logic structs that share it. The
        // heater re-hydrates its energy counter from `kv` as it is built; the two
        // meters have no state of their own, they just read the heater. The
        // Thermostat handler persists its attributes itself, under the key given
        // here, and restores and repairs them when `Startup` reaches it.
        let heater = MillHeater::new(&kv, mill_uart);

        let thermostat_handler = ThermostatHandler::new(
            Dataver::new_rand(&mut weak_rand),
            THERMOSTAT_ENDPOINT,
            vendor_kv::THERMOSTAT_ATTRS_KEY,
            ThermostatDeviceLogic::new(&kv, &heater),
        );

        // The Electrical Sensor clusters. Power Topology carries no state at all -
        // with the `NODE` topology it has no attributes to serve.
        let power_handler = elec_pwr_meas::ElecPwrMeasHandler::new(
            Dataver::new_rand(&mut weak_rand),
            THERMOSTAT_ENDPOINT,
            ElecPwrDeviceLogic::new(&heater),
        );

        let energy_handler = elec_energy_meas::ElecEnergyMeasHandler::new(
            Dataver::new_rand(&mut weak_rand),
            THERMOSTAT_ENDPOINT,
            ElecEnergyDeviceLogic::new(&heater),
        );

        // The element's rating, and the two clusters that set it. Built after the
        // heater and before the stack runs: its constructor pushes the default
        // rating onto the heater, and `ModeSelectHandler` validates the mode table,
        // restores `CurrentMode` and repairs a stale one when `Startup` reaches it.
        // `ElementWattsHandler::run` then brings the rating in line with it.
        //
        // Both handlers borrow the rating rather than owning it, which is what keeps
        // the two clusters one value. Unlike `ElecEnergyMeasHooks` - see the note on
        // `cumulative_energy_reset` in CLAUDE.md - `impl ModeSelectHooks for &T`
        // forwards every method, so `&ElementRating` is a complete hooks impl.
        let rating = ElementRating::new(&kv, &heater);

        let mode_handler = ModeSelectHandler::new(
            Dataver::new_rand(&mut weak_rand),
            vendor_kv::ELEMENT_MODE_KEY,
            &rating,
        );

        // Borrows `mode_handler` so that writing an exact wattage moves `CurrentMode`
        // through the cluster that owns it, and subscribers hear about it.
        let watts_handler = ElementWattsHandler::new(
            Dataver::new_rand(&mut weak_rand),
            ELEMENT_ENDPOINT,
            &rating,
            &mode_handler,
        );

        // Chain our endpoint clusters. The chain is matched last-first.
        let handler = EmptyHandler
            // The Endpoint 0 system clusters that are ours to provide. The stack
            // adds the operational network clusters (Network Commissioning, General
            // Commissioning, General Diagnostics and Thread Diagnostics) on top,
            // because only it knows the network driver state - which is why this
            // must be `root_handler` and NOT `ThreadSysHandlerBuilder`, or those
            // clusters would be chained twice.
            .chain(
                |e, _| e == ROOT_ENDPOINT_ID,
                Async(EmbassyThreadMatterStack::<0, ()>::root_handler(
                    &(),
                    &mut weak_rand,
                )),
            )
            // Why the board last reset. Chained after - so matched before - the
            // catch-all EP0 matcher above, which would otherwise claim it.
            .chain(
                |e, c| e == ROOT_ENDPOINT_ID && c == boot_diag::BOOT_DIAG_CLUSTER_ID,
                &boot_diag,
            )
            // Every endpoint needs a Descriptor cluster; use the one `rs-matter`
            // provides out of the box.
            .chain(
                |e, c| e == THERMOSTAT_ENDPOINT && c == desc::DescHandler::CLUSTER.id,
                Async(desc::DescHandler::new(Dataver::new_rand(&mut weak_rand)).adapt()),
            )
            .chain(
                |e, c| e == THERMOSTAT_ENDPOINT && c == identify::CLUSTER.id,
                Async(IdentifyHandler::new(Dataver::new_rand(&mut weak_rand)).adapt()),
            )
            .chain(
                |e, c| e == THERMOSTAT_ENDPOINT && c == groups::GroupsHandler::CLUSTER.id,
                Async(groups::GroupsHandler::new(Dataver::new_rand(&mut weak_rand)).adapt()),
            )
            .chain(
                |e, c| e == THERMOSTAT_ENDPOINT && c == ThermostatDeviceLogic::CLUSTER.id,
                Async(ThermostatHandlerAdaptor(&thermostat_handler)),
            )
            .chain(
                |e, c| e == THERMOSTAT_ENDPOINT && c == power_topology::CLUSTER.id,
                Async(PowerTopologyHandler::new(Dataver::new_rand(&mut weak_rand)).adapt()),
            )
            .chain(
                |e, c| e == THERMOSTAT_ENDPOINT && c == ElecPwrDeviceLogic::CLUSTER.id,
                Async(elec_pwr_meas::HandlerAdaptor(&power_handler)),
            )
            .chain(
                |e, c| e == THERMOSTAT_ENDPOINT && c == ElecEnergyDeviceLogic::CLUSTER.id,
                Async(elec_energy_meas::HandlerAdaptor(&energy_handler)),
            )
            // Endpoint 2: the element's rating.
            .chain(
                |e, c| e == ELEMENT_ENDPOINT && c == desc::DescHandler::CLUSTER.id,
                Async(desc::DescHandler::new(Dataver::new_rand(&mut weak_rand)).adapt()),
            )
            .chain(
                |e, c| e == ELEMENT_ENDPOINT && c == mode_select::CLUSTER.id,
                Async(mode_select::HandlerAdaptor(&mode_handler)),
            )
            .chain(
                |e, c| e == ELEMENT_ENDPOINT && c == element::ELEMENT_WATTS_CLUSTER.id,
                &watts_handler,
            );

        // Run the Matter stack with our handler. `pin!` is optional, but reduces the
        // size of the final future.
        //
        // The three device-logic `run()` loops need no task of their own: the
        // Interaction Model - which `stack.run` owns - polls `AsyncHandler::run` on
        // every handler in the chain, and each cluster handler drives its hooks'
        // `run()` from there. Hence `()` for the user task.
        let mut matter = pin!(stack.run(
            // The Matter stack needs to instantiate an `openthread` Radio. Wrapped
            // so the serial log shows whether BLE is down whenever Thread starts -
            // see `radio.rs`.
            EmbassyThread::new(
                LoggedRadio(EspThreadDriver::new(peripherals.IEEE802154, peripherals.BT)),
                crypto.rand().unwrap(),
                ieee_eui64,
                &kv,
                stack,
                true, // Use a random BLE address
            ),
            // The crypto provider
            &crypto,
            // Our `AsyncHandler` + `AsyncMetadata` impl
            (NODE, &handler),
            // The blob store the stack persists its state to
            &kv,
            // No user task to run
            (),
        ));

        // Run Matter, and also watch for a factory-reset request
        let mut wait_reset = pin!(wait_pin_low(Input::new(
            peripherals.GPIO9,
            InputConfig::default().with_pull(Pull::Down)
        )));

        // Which future finished decides what happens next, so `select` is matched
        // rather than `coalesce`d: `stack.run` can in principle return `Ok(())` when
        // the stack is stopped, and treating that as consent to wipe a commissioned
        // device - as the upstream example's `coalesce().unwrap()` would - is not a
        // trade anybody wants.
        //
        // The watchdog is fed from the same `select`, so the same task polls it.
        // Anything that stops this task being polled - a future blocking the CPU, a
        // panic halting it - stops the feeding too.
        watchdog.feed();

        match select3(&mut matter, &mut wait_reset, watchdog.run()).await {
            Either3::First(result) => {
                result.unwrap();

                warn!("Matter stack stopped; rebooting without touching storage");

                esp_hal::system::software_reset()
            }
            Either3::Second(result) => result.unwrap(),
            Either3::Third(never) => match never {},
        }

        warn!("Factory reset requested");

        // Clear the vendor keys while we still hold the store handle the device
        // logic was built over.
        //
        // The Thermostat and Mode Select handlers would clear their own on the
        // `FactoryReset` lifecycle op, but the reset below cannot chain them - see
        // the comment there - and `ElecEnergyMeasHooks` has no lifecycle method at
        // all. Nor does `Matter::factory_reset` do it - that only removes rs-matter's
        // own keys, which by design grow *downwards* from `VENDOR_KEYS_START`. So
        // this is the one place the setpoints, the lifetime energy counter and the
        // element rating get cleared.
        //
        // Clearing the rating means a reset board meters at `config.toml`'s
        // `heater.element_watts` again, which is the right answer for the same
        // reason the setpoints go back to their defaults: a factory reset returns
        // the device to what it was built as, and the next owner of the fabric
        // should not inherit the last one's calibration.
        for key in [
            vendor_kv::HEATING_ELEMENT_ENERGY_KEY,
            vendor_kv::ELEMENT_RATING_KEY,
            vendor_kv::THERMOSTAT_ATTRS_KEY,
            vendor_kv::ELEMENT_MODE_KEY,
        ] {
            if let Err(e) = kv.remove_blob(key) {
                warn!("Could not remove vendor key {key:#x}: {e}");
            }
        }
    }

    // `stack.reset` takes `&mut *stack`, so it cannot be called while `kv` - which
    // borrows `stack.matter()` - is alive. Hence the scope above, and hence a
    // freshly-built handler here: the EP1 chain borrows `kv` and cannot outlive it.
    //
    // Leaving EP1 and EP2 out of this chain is sound because the only thing `reset`
    // does with a handler is broadcast the `FactoryReset` lifecycle op, and all the
    // EP1/EP2 handlers would do with it is remove their own keys - which the loop
    // above has already done. Everything else that persists through it - ACL, NOC,
    // Group Key Management, General Commissioning - lives inside `root_handler`.
    warn!("Resetting storage");

    let reset_handler = EmptyHandler.chain(
        |e, _| e == ROOT_ENDPOINT_ID,
        Async(EmbassyThreadMatterStack::<0, ()>::root_handler(
            &(),
            &mut weak_rand,
        )),
    );

    // Erasing the `nvs` range blocks for a few seconds at most, well inside the
    // watchdog's timeout - but only if the count starts from zero here.
    watchdog.feed();

    stack
        .reset(&crypto, (NODE, &reset_handler), &mut store)
        .await
        .unwrap();

    warn!("Rebooting...");

    esp_hal::system::software_reset()
}

/// The identity strings that have to differ from board to board, derived once at
/// boot from the chip's factory MAC.
///
/// `rs-matter`'s `TEST_DEV_DET` cannot serve here: it hard-codes
/// `serial_no: "123456789"` and inherits an empty `unique_id` from
/// `BasicInfoConfig::new()`, so *every* board built from it reports the same
/// `SerialNumber` and no `UniqueID` at all. A controller that keys its own device
/// records on the serial number - Home Assistant does, alongside the node ID -
/// folds two physically different nodes into one device on that alone.
///
/// `BasicInfoConfig` borrows every string it serves, so these have to outlive the
/// Matter stack; hence [`dev_det`] and its `StaticCell`s rather than a `const`.
struct DeviceIdentity {
    /// The factory MAC-48 as twelve uppercase hex digits - the same bytes the SRP
    /// host name in the log is built from, so the two can be matched up by eye.
    serial_no: [u8; 12],
    /// The serial number behind [`config::UNIQUE_ID_PREFIX`].
    ///
    /// `UniqueID` is mandatory from Basic Information cluster revision 4, which is
    /// what Matter 1.6 asks for, and `BasicInfoConfig::new()` leaves it empty.
    /// Deriving it from the MAC rather than minting a random one and persisting it
    /// is deliberate: the attribute is `persistence="fixed"`, and a value living in
    /// the KV store would not survive the factory reset that a fixed value must.
    /// A certified device would carry an independent factory-provisioned value.
    unique_id: [u8; config::UNIQUE_ID_PREFIX.len() + 12],
}

impl DeviceIdentity {
    fn from_factory_mac() -> Self {
        let mac = esp_hal::efuse::base_mac_address();

        let mut serial_no = [0; 12];
        hex(mac.as_bytes(), &mut serial_no);

        let mut unique_id = [0; config::UNIQUE_ID_PREFIX.len() + 12];
        unique_id[..config::UNIQUE_ID_PREFIX.len()]
            .copy_from_slice(config::UNIQUE_ID_PREFIX.as_bytes());
        unique_id[config::UNIQUE_ID_PREFIX.len()..].copy_from_slice(&serial_no);

        Self {
            serial_no,
            unique_id,
        }
    }

    fn serial_no(&self) -> &str {
        // Infallible: every byte was put there as an ASCII hex digit.
        core::str::from_utf8(&self.serial_no).unwrap()
    }

    fn unique_id(&self) -> &str {
        // Infallible: an ASCII literal followed by ASCII hex digits.
        core::str::from_utf8(&self.unique_id).unwrap()
    }
}

/// Write `bytes` into `out` as uppercase hex; `out` must be twice as long.
fn hex(bytes: &[u8], out: &mut [u8]) {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";

    for (byte, out) in bytes.iter().zip(out.chunks_exact_mut(2)) {
        out[0] = DIGITS[usize::from(byte >> 4)];
        out[1] = DIGITS[usize::from(byte & 0x0f)];
    }
}

/// Basic info about our device.
///
/// Everything a Mill owner might reasonably want to change comes from `config.toml`
/// by way of [`config`]; what is left inherited from `TEST_DEV_DET` is the part that
/// is not ours to pick. The attestation data is still `rs-matter`'s test DAC/PAI, so
/// a commissioner will warn that the device is uncertified - expected, and harmless
/// on a private fabric - and `vid`/`pid` stay at the CSA *test* values because the
/// test DAC is issued for exactly those: a `BasicInformation` value that disagreed
/// with the certificate would fail device attestation outright rather than merely
/// warn, so those two are deliberately not configurable.
///
/// The serial number and the unique ID are derived per board, which is why this is a
/// function rather than a `const` - see [`DeviceIdentity`].
///
/// Called exactly once; a second call panics on the already-initialised
/// `StaticCell`, which is the right way for that mistake to show up.
fn dev_det() -> &'static BasicInfoConfig<'static> {
    static IDENTITY: StaticCell<DeviceIdentity> = StaticCell::new();
    static DEV_DET: StaticCell<BasicInfoConfig<'static>> = StaticCell::new();

    let identity: &'static DeviceIdentity = IDENTITY.init(DeviceIdentity::from_factory_mac());

    // An explicit `device.serial_number` overrides the MAC-derived one; the unique ID
    // never follows it, because that attribute is `fixed`-quality and has to stay
    // per-board whatever a shared config file says.
    let serial_no = config::SERIAL_NUMBER.unwrap_or_else(|| identity.serial_no());

    info!(
        "Device serial number {}, unique ID {}",
        serial_no,
        identity.unique_id()
    );

    DEV_DET.init(BasicInfoConfig {
        vendor_name: config::VENDOR_NAME,
        product_name: config::PRODUCT_NAME,
        product_label: config::PRODUCT_LABEL,
        part_number: config::PART_NUMBER,
        hw_ver: config::HW_VER,
        hw_ver_str: config::HW_VER_STR,
        sw_ver: config::SW_VER,
        sw_ver_str: config::SW_VER_STR,
        manufacturing_date: config::MANUFACTURING_DATE,
        device_name: config::DEVICE_NAME,
        serial_no,
        unique_id: identity.unique_id(),
        // The mDNS-to-SRP bridge wants this: how long, in ms, a sleepy device may
        // take to answer. Same value the `rs-matter-embassy` Thread example uses.
        sai: Some(500),
        ..rs_matter_embassy::matter::dm::devices::test::TEST_DEV_DET
    })
}

/// The passcode and discriminator behind the pairing codes printed at boot.
///
/// `TEST_DEV_COMM`'s 20202021/3840 are the defaults in `config.toml`, but unlike the
/// VID/PID they are ours to change: nothing cross-checks them against the test
/// certificate. Giving a second board its own discriminator is the point - two boards
/// advertising 3840 at the same time are genuinely ambiguous to a commissioner.
fn dev_comm() -> BasicCommData {
    BasicCommData {
        password: Spake2pVerifierPassword::new_from_ref(Spake2pVerifierPasswordRef::new(
            &config::PASSCODE.to_le_bytes(),
        )),
        discriminator: config::DISCRIMINATOR,
    }
}

/// The Node meta-data describing our Matter device.
///
/// EP1 carries two device types: the Thermostat, which is an *application* device
/// type, and the Electrical Sensor, which is a *utility* one. Core spec 9.2.1 allows
/// a simple endpoint only one application device type but any number of utility
/// ones, which is what lets the thermostat meter itself on the same endpoint rather
/// than needing a second.
const NODE: Node = Node {
    endpoints: &[
        // `EmbassyThreadMatterStack::root_endpoint()`, spelled out - it is
        // `root_endpoint!(thread)` - so that `boot_diag.rs`'s cluster can join the
        // system clusters. `clusters!` takes extra ones after the `;`.
        Endpoint {
            id: ROOT_ENDPOINT_ID,
            device_types: devices!(DEV_TYPE_ROOT_NODE),
            clusters: clusters!(thread; boot_diag::BOOT_DIAG_CLUSTER),
            client_clusters: &[],
            unique_id: None,
            semantic_tags: &[],
        },
        Endpoint::new(
            THERMOSTAT_ENDPOINT,
            devices!(DEV_TYPE_THERMOSTAT, DEV_TYPE_ELECTRICAL_SENSOR),
            clusters!(
                desc::DescHandler::CLUSTER,
                identify::CLUSTER,
                groups::GroupsHandler::CLUSTER,
                ThermostatDeviceLogic::CLUSTER,
                power_topology::CLUSTER,
                ElecPwrDeviceLogic::CLUSTER,
                ElecEnergyDeviceLogic::CLUSTER,
            ),
        ),
        Endpoint::new(
            ELEMENT_ENDPOINT,
            devices!(DEV_TYPE_MODE_SELECT),
            clusters!(
                desc::DescHandler::CLUSTER,
                mode_select::CLUSTER,
                element::ELEMENT_WATTS_CLUSTER,
            ),
        ),
    ],
};

/// Whether `NODE`'s root endpoint is still the stack's own plus `boot_diag.rs`'s
/// cluster at the end - that is, whether spelling it out has not drifted from
/// `root_endpoint()` since `rs-matter-stack` last moved.
const fn root_endpoint_matches_stack() -> bool {
    let stack = EmbassyThreadMatterStack::<0, ()>::root_endpoint();
    let ours = &NODE.endpoints[0];

    if ours.id != stack.id
        || ours.device_types.len() != stack.device_types.len()
        || ours.clusters.len() != stack.clusters.len() + 1
        || ours.clusters[stack.clusters.len()].id != boot_diag::BOOT_DIAG_CLUSTER_ID
    {
        return false;
    }

    let mut i = 0;

    while i < stack.device_types.len() {
        if ours.device_types[i].dtype != stack.device_types[i].dtype {
            return false;
        }

        i += 1;
    }

    let mut i = 0;

    while i < stack.clusters.len() {
        let (a, b) = (&ours.clusters[i], &stack.clusters[i]);

        if a.id != b.id || a.revision != b.revision || a.feature_map != b.feature_map {
            return false;
        }

        i += 1;
    }

    true
}

const _: () = assert!(root_endpoint_matches_stack());

/// Derive a stable IEEE 802.15.4 extended address from the chip's factory MAC-48.
///
/// The standard EUI-48 -> EUI-64 encapsulation: keep the three OUI bytes, insert
/// `FF FE`, then the three device bytes. The U/L bit is deliberately *not* flipped -
/// that is the "modified EUI-64" form IPv6 uses for interface identifiers, and
/// setting it here would mark an address that came out of Espressif's own OUI as
/// locally administered, which it is not.
///
/// `esp-hal` exposes no 802.15.4 interface MAC (only Station / AccessPoint /
/// Bluetooth), so the base MAC is the route. The BLE address is randomised
/// separately, so there is nothing to collide with.
fn eui64_from_factory_mac() -> [u8; 8] {
    let mac = esp_hal::efuse::base_mac_address();
    let mac = mac.as_bytes();

    [mac[0], mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5]]
}

/// Build the KV store over the `nvs` data partition of the flashed partition table.
///
/// The partition is located by *type* at runtime, so resizing it in `partitions.csv`
/// needs no code change.
fn get_persistent_store<'d>(
    flash: esp_hal::peripherals::FLASH<'d>,
    mut buf: impl BorrowMut<[u8]>,
) -> impl KvBlobStore + 'd {
    let mut flash = FlashStorage::new(flash);
    let pt_buf = &mut buf.borrow_mut()[..PARTITION_TABLE_MAX_LEN];
    let pt = read_partition_table(&mut flash, pt_buf).unwrap();
    let nvs = pt
        .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
        .unwrap()
        .unwrap();

    let start = nvs.offset();
    let end = nvs.offset() + nvs.len();
    info!(
        "Will use NVS partition \"{}\" at {:#x}..{:#x}",
        nvs.label_as_str(),
        start,
        end
    );

    SeqMapKvBlobStore::new(BlockingAsync::new(flash), start..end)
}

/// Resolve once the pin has been held low for [`RESET_SECS`].
async fn wait_pin_low(mut pin: Input<'_>) -> Result<(), Error> {
    loop {
        pin.wait_for_low().await;

        // Debounce
        embassy_time::Timer::after_millis(50).await;

        if pin.is_low() {
            warn!(
                "Detected Boot Mode pin low, keep it low for {} more seconds to reset the storage",
                RESET_SECS
            );

            let result = select(
                pin.wait_for_high(),
                embassy_time::Timer::after_secs(RESET_SECS),
            )
            .await;

            if matches!(result, Either::Second(())) {
                break;
            }
        }
    }

    Ok(())
}
