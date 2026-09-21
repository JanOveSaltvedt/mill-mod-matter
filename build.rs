//! Turns `config.toml` into the constants `src/config.rs` exposes.
//!
//! Build-time rather than runtime for three reasons: the device has no filesystem to
//! read a config from at boot; several of the values land in `const` contexts that a
//! runtime value could not satisfy (`ThermostatHooks::ABS_MIN_HEAT_SETPOINT`,
//! `ElecPwrMeasHooks::ACCURACY`); and it matches what the ESPHome implementation this
//! replaced did with its own YAML, which was also resolved into generated C++.
//!
//! `config.local.toml`, if it exists, is merged over `config.toml` key by key so that
//! a differently-rated unit builds from a clean tree. The merged table is then
//! deserialised into a `deny_unknown_fields` struct, which is what makes a typo in
//! *either* file a build error with a line and column rather than a silently ignored
//! key.
//!
//! This is a host build script, so `std` is available and none of the crate's
//! `no_std` rules apply here.

use std::fmt::Write as _;
use std::path::Path;
use std::{env, fs};

use serde::Deserialize;
use toml::{Table, Value};

/// The committed defaults, documenting every key.
const CONFIG: &str = "config.toml";

/// Optional, gitignored, partial: overrides [`CONFIG`] key by key.
const LOCAL_CONFIG: &str = "config.local.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    heater: Heater,
    device: Device,
    commissioning: Commissioning,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Heater {
    element_watts: u32,
    min_setpoint_celsius: i32,
    max_setpoint_celsius: i32,
    default_setpoint_celsius: i32,
    metering_accuracy_percent: f64,
    circuit_max_watts: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Device {
    vendor_name: String,
    product_name: String,
    product_label: String,
    part_number: String,
    hardware_version: u16,
    hardware_version_string: String,
    software_version: u32,
    software_version_string: String,
    manufacturing_date: String,
    device_name: String,
    serial_number: String,
    unique_id_prefix: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Commissioning {
    passcode: u32,
    discriminator: u16,
}

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed={CONFIG}");
    // Fires when the file appears, too - a `rerun-if-changed` on a path that does not
    // exist yet is honoured rather than ignored.
    println!("cargo::rerun-if-changed={LOCAL_CONFIG}");

    let config = match load() {
        Ok(config) => config,
        Err(err) => fail(&err),
    };

    if let Err(err) = validate(&config) {
        fail(&err);
    }

    let out = Path::new(&env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("config.rs");

    if let Err(err) = fs::write(&out, emit(&config)) {
        fail(&format!("could not write {}: {err}", out.display()));
    }
}

/// Report a configuration problem and stop the build.
///
/// `cargo::error` puts it in the same place a compile error goes; the panic is what
/// actually fails the build, and its message is suppressed so the diagnostic is not
/// printed twice.
fn fail(message: &str) -> ! {
    for line in message.lines() {
        println!("cargo::error={line}");
    }

    std::process::exit(1);
}

/// Parse [`CONFIG`], merge [`LOCAL_CONFIG`] over it, and deserialise the result.
fn load() -> Result<Config, String> {
    let mut merged = read_table(CONFIG)?.ok_or_else(|| {
        format!("{CONFIG} is missing - it carries the defaults and has to be present")
    })?;

    if let Some(local) = read_table(LOCAL_CONFIG)? {
        merge(&mut merged, local);
    }

    // Deserialising the *merged* table rather than each file separately is what lets
    // `config.local.toml` be partial while still catching unknown keys in it.
    Config::deserialize(Value::Table(merged))
        .map_err(|err| format!("{CONFIG} (or {LOCAL_CONFIG}) is not valid configuration: {err}"))
}

fn read_table(path: &str) -> Result<Option<Table>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("could not read {path}: {err}")),
    };

    text.parse::<Table>()
        .map(Some)
        .map_err(|err| format!("{path} is not valid TOML: {err}"))
}

/// Overlay `over` onto `base`, recursing into tables so a partial `[heater]` section
/// replaces only the keys it names.
fn merge(base: &mut Table, over: Table) {
    for (key, value) in over {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(base)), Value::Table(over)) => merge(base, over),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn validate(config: &Config) -> Result<(), String> {
    let Config {
        heater,
        device,
        commissioning,
    } = config;

    // Basic Information's string maxima, from the doc comments on `BasicInfoConfig`
    // in `../rs-matter/rs-matter/src/dm/clusters/basic_info.rs`. A longer value would
    // be truncated or rejected by a controller rather than caught here.
    for (key, value, max) in [
        ("device.vendor_name", &device.vendor_name, 32),
        ("device.product_name", &device.product_name, 32),
        ("device.product_label", &device.product_label, 64),
        ("device.part_number", &device.part_number, 32),
        (
            "device.hardware_version_string",
            &device.hardware_version_string,
            64,
        ),
        (
            "device.software_version_string",
            &device.software_version_string,
            64,
        ),
        ("device.manufacturing_date", &device.manufacturing_date, 16),
        ("device.serial_number", &device.serial_number, 32),
    ] {
        let len = value.chars().count();

        if len > max {
            return Err(format!(
                "{key} is {len} characters; the Basic Information cluster allows at most {max}"
            ));
        }
    }

    if device.vendor_name.is_empty() || device.product_name.is_empty() {
        return Err("device.vendor_name and device.product_name may not be empty".into());
    }

    if device.device_name.is_empty() {
        return Err(
            "device.device_name may not be empty - it is what a commissioner shows in its \
             \"device found\" prompt"
                .into(),
        );
    }

    // The generated UniqueID is the prefix followed by twelve hex digits of the
    // factory MAC, and the attribute allows 64 characters.
    let unique_id_len = device.unique_id_prefix.chars().count() + 12;

    if unique_id_len > 64 {
        return Err(format!(
            "device.unique_id_prefix is too long: with the twelve hex digits of the factory MAC \
             it would make a {unique_id_len}-character UniqueID, and the attribute allows 64"
        ));
    }

    validate_date(&device.manufacturing_date)?;

    if heater.element_watts == 0 {
        return Err("heater.element_watts must be greater than zero".into());
    }

    if heater.element_watts > heater.circuit_max_watts {
        return Err(format!(
            "heater.element_watts ({}) exceeds heater.circuit_max_watts ({}), so the element \
             would draw more than the range the meter advertises",
            heater.element_watts, heater.circuit_max_watts
        ));
    }

    if heater.min_setpoint_celsius >= heater.max_setpoint_celsius {
        return Err(format!(
            "heater.min_setpoint_celsius ({}) must be below heater.max_setpoint_celsius ({})",
            heater.min_setpoint_celsius, heater.max_setpoint_celsius
        ));
    }

    if !(heater.min_setpoint_celsius..=heater.max_setpoint_celsius)
        .contains(&heater.default_setpoint_celsius)
    {
        return Err(format!(
            "heater.default_setpoint_celsius ({}) is outside the configured range {}..={}",
            heater.default_setpoint_celsius,
            heater.min_setpoint_celsius,
            heater.max_setpoint_celsius
        ));
    }

    // The three setpoints are served as hundredths of a degree in an `i16`.
    for (key, celsius) in [
        ("heater.min_setpoint_celsius", heater.min_setpoint_celsius),
        ("heater.max_setpoint_celsius", heater.max_setpoint_celsius),
        (
            "heater.default_setpoint_celsius",
            heater.default_setpoint_celsius,
        ),
    ] {
        if centidegrees(celsius).is_none() {
            return Err(format!(
                "{key} ({celsius} C) does not fit the cluster's hundredths-of-a-degree i16, \
                 whose range is {}..={} C",
                i16::MIN / 100,
                i16::MAX / 100
            ));
        }
    }

    accuracy_hundredths(heater.metering_accuracy_percent)?;

    // 12-bit, per the Core spec.
    if commissioning.discriminator > 0xfff {
        return Err(format!(
            "commissioning.discriminator ({}) does not fit 12 bits; the maximum is 4095",
            commissioning.discriminator
        ));
    }

    validate_passcode(commissioning.passcode)?;

    Ok(())
}

/// `YYYYMMDD`, and a date that exists.
fn validate_date(date: &str) -> Result<(), String> {
    let complain =
        || format!("device.manufacturing_date ({date:?}) must be a real calendar date as YYYYMMDD");

    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return Err(complain());
    }

    let (year, month, day) = (
        date[..4].parse::<u32>().map_err(|_| complain())?,
        date[4..6].parse::<u32>().map_err(|_| complain())?,
        date[6..].parse::<u32>().map_err(|_| complain())?,
    );

    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);

    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return Err(complain()),
    };

    if day == 0 || day > days {
        return Err(complain());
    }

    Ok(())
}

/// The passcode constraints from Core spec 5.1.7.1: a 27-bit value, not zero, and not
/// one of the twelve the spec calls out as too easily guessed.
fn validate_passcode(passcode: u32) -> Result<(), String> {
    /// Section 5.1.7.1's list of invalid passcodes.
    const FORBIDDEN: [u32; 12] = [
        0, 11111111, 22222222, 33333333, 44444444, 55555555, 66666666, 77777777, 88888888,
        99999999, 12345678, 87654321,
    ];

    /// The largest passcode that encodes in the manual pairing code's 27 bits.
    const MAX: u32 = 0x5f5e0fe;

    if passcode > MAX {
        return Err(format!(
            "commissioning.passcode ({passcode}) is above the maximum {MAX} (0x5F5E0FE)"
        ));
    }

    if FORBIDDEN.contains(&passcode) {
        return Err(format!(
            "commissioning.passcode ({passcode:08}) is one of the twelve values Core spec 5.1.7.1 \
             forbids as too easily guessed"
        ));
    }

    Ok(())
}

/// Whole degrees Celsius to the hundredths the Thermostat cluster serves.
fn centidegrees(celsius: i32) -> Option<i16> {
    celsius.checked_mul(100).and_then(|c| i16::try_from(c).ok())
}

/// Percent to the hundredths of a percent `MeasurementAccuracyRange` wants.
fn accuracy_hundredths(percent: f64) -> Result<u16, String> {
    if !(0.0..=100.0).contains(&percent) {
        return Err(format!(
            "heater.metering_accuracy_percent ({percent}) must be between 0 and 100"
        ));
    }

    let hundredths = percent * 100.0;

    // The attribute is in hundredths of a percent, so anything finer is a value the
    // device cannot actually express - better to say so than to round it silently.
    if (hundredths - hundredths.round()).abs() > 1e-6 {
        return Err(format!(
            "heater.metering_accuracy_percent ({percent}) is finer than the hundredth of a \
             percent the cluster can express"
        ));
    }

    Ok(hundredths.round() as u16)
}

/// The generated module body.
fn emit(config: &Config) -> String {
    let Config {
        heater,
        device,
        commissioning,
    } = config;

    // Every one of these was checked in `validate`.
    let min_setpoint = centidegrees(heater.min_setpoint_celsius).unwrap();
    let max_setpoint = centidegrees(heater.max_setpoint_celsius).unwrap();
    let default_setpoint = centidegrees(heater.default_setpoint_celsius).unwrap();
    let accuracy = accuracy_hundredths(heater.metering_accuracy_percent).unwrap();

    // Empty means "the crate version", which is the useful default for a firmware
    // whose SoftwareVersionString nobody wants to keep in step by hand.
    let sw_ver_str = if device.software_version_string.is_empty() {
        env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION")
    } else {
        device.software_version_string.clone()
    };

    // Empty means "derive it from the factory MAC" - see the key's comment in
    // `config.toml` for why that is the right default.
    let serial_no = if device.serial_number.is_empty() {
        "None".to_string()
    } else {
        format!("Some({:?})", device.serial_number)
    };

    let element_power_mw = i64::from(heater.element_watts) * 1000;
    let circuit_max_power_mw = i64::from(heater.circuit_max_watts) * 1000;

    let vendor_name = &device.vendor_name;
    let product_name = &device.product_name;
    let product_label = &device.product_label;
    let part_number = &device.part_number;
    let hardware_version = device.hardware_version;
    let hardware_version_string = &device.hardware_version_string;
    let software_version = device.software_version;
    let manufacturing_date = &device.manufacturing_date;
    let device_name = &device.device_name;
    let unique_id_prefix = &device.unique_id_prefix;
    let passcode = commissioning.passcode;
    let discriminator = commissioning.discriminator;

    let mut out = String::new();

    let _ = write!(
        out,
        r#"// @generated by build.rs from config.toml - do not edit.
//
// Edit `config.toml` (or `config.local.toml`) and rebuild instead.

/// The heating element's rated power, in milliwatts.
///
/// Gated on the Mill's on/off bit, this is every power and energy reading the device
/// serves; there is no metering hardware to check it against.
pub const ELEMENT_POWER_MW: i64 = {element_power_mw};

/// The top of the reported power range, in milliwatts.
pub const CIRCUIT_MAX_POWER_MW: i64 = {circuit_max_power_mw};

/// The claimed accuracy of every reading, in hundredths of a percent.
pub const METER_ACCURACY: u16 = {accuracy};

/// `AbsMinHeatSetpointLimit`, in hundredths of a degree Celsius.
pub const ABS_MIN_HEAT_SETPOINT: i16 = {min_setpoint};

/// `AbsMaxHeatSetpointLimit`, in hundredths of a degree Celsius.
pub const ABS_MAX_HEAT_SETPOINT: i16 = {max_setpoint};

/// The setpoint a device with nothing to restore comes up with, in hundredths of a
/// degree Celsius.
pub const DEFAULT_HEATING_SETPOINT: i16 = {default_setpoint};

/// Basic Information's `VendorName`.
pub const VENDOR_NAME: &str = {vendor_name:?};

/// Basic Information's `ProductName`.
pub const PRODUCT_NAME: &str = {product_name:?};

/// Basic Information's `ProductLabel`.
pub const PRODUCT_LABEL: &str = {product_label:?};

/// Basic Information's `PartNumber`.
pub const PART_NUMBER: &str = {part_number:?};

/// Basic Information's `HardwareVersion`.
pub const HW_VER: u16 = {hardware_version};

/// Basic Information's `HardwareVersionString`.
pub const HW_VER_STR: &str = {hardware_version_string:?};

/// Basic Information's `SoftwareVersion`.
pub const SW_VER: u32 = {software_version};

/// Basic Information's `SoftwareVersionString`.
pub const SW_VER_STR: &str = {sw_ver_str:?};

/// Basic Information's `ManufacturingDate`, as `YYYYMMDD`.
pub const MANUFACTURING_DATE: &str = {manufacturing_date:?};

/// The name carried in the mDNS commissioning advertisement.
pub const DEVICE_NAME: &str = {device_name:?};

/// Basic Information's `SerialNumber`, or `None` to derive it from the factory MAC.
pub const SERIAL_NUMBER: Option<&str> = {serial_no};

/// Prefixed to the MAC-derived `UniqueID`.
pub const UNIQUE_ID_PREFIX: &str = {unique_id_prefix:?};

/// The commissioning passcode behind the printed pairing codes.
pub const PASSCODE: u32 = {passcode};

/// The 12-bit commissioning discriminator.
pub const DISCRIMINATOR: u16 = {discriminator};
"#
    );

    out
}
