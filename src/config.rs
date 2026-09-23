//! Per-unit settings, generated at build time from `config.toml`.
//!
//! The body of this module is written by `build.rs` into `OUT_DIR` and included
//! below, so the constants here are not editable in place - change `config.toml`, or
//! a `config.local.toml` overriding it, and rebuild. `build.rs` validates every value
//! before emitting it, so a bad setpoint range or an over-long `VendorName` is a
//! build error rather than something a controller discovers.
//!
//! Build time rather than runtime because there is no filesystem on the device to
//! read a config from at boot, and because several of these land in `const` contexts
//! that a runtime value could not satisfy: [`ABS_MIN_HEAT_SETPOINT`] and
//! [`ABS_MAX_HEAT_SETPOINT`] are associated consts of `ThermostatHooks`, and
//! [`METER_ACCURACY`] feeds the `ACCURACY` consts of both metering hooks.
//!
//! [`DEFAULT_ELEMENT_WATTS`] is the exception that proves it: nothing about the
//! element's rating is `const`-bound - `MillHeater::active_power_mw` only reads it -
//! which is what lets `element.rs` expose it over Matter and leaves this constant as
//! the value a device with nothing stored falls back to. [`CIRCUIT_MAX_WATTS`] is the
//! ceiling on what may be stored, and stays build-time because [`METER_ACCURACY`]'s
//! range is built from it.

include!(concat!(env!("OUT_DIR"), "/config.rs"));
