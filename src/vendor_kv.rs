//! Device state this firmware keeps across a reboot, in the Matter KV store
//! under the vendor key range.
//!
//! Keeping the application state in the *same* store as the Matter state is what
//! makes a factory reset mean what a user expects: `MatterStack::reset` wipes the
//! store, so the heater really does come back with default setpoints and a zeroed
//! energy counter. State kept somewhere of its own would survive the reset and
//! quietly contradict the freshly-commissioned device.
//!
//! Ported from `rs-matter`'s own DUT drivers - see
//! `../rs-matter/tests/src/common/vendor_kv.rs`.

use rs_matter_embassy::matter::error::Error;
use rs_matter_embassy::matter::persist::{KvBlobStoreAccess, VENDOR_KEYS_START};

/// The Thermostat cluster's four non-volatile attributes, as seven bytes:
/// `SystemMode` then `OccupiedHeatingSetpoint`, `MinHeatSetpointLimit` and
/// `MaxHeatSetpointLimit` as little-endian `i16`s.
///
/// Note the `+ 1`: `VENDOR_KEYS_START` itself is **not** free here. On a Thread
/// device `rs-matter-embassy` stores OpenThread's SRP ECDSA key there
/// (`OT_SRP_ECDSA_KEY` in `../rs-matter-embassy/rs-matter-embassy/src/ot.rs`), and
/// stepping on it would cost the device its SRP identity on every boot.
pub const THERMOSTAT_STATE_KEY: u16 = VENDOR_KEYS_START + 1;

/// The heating element's lifetime energy counter, as a little-endian `i64` in
/// milliwatt-seconds. Reported (divided down to mWh) as the Electrical Energy
/// Measurement cluster's `CumulativeEnergyImported`.
pub const HEATING_ELEMENT_ENERGY_KEY: u16 = VENDOR_KEYS_START + 2;

/// An object-safe view of a [`KvBlobStoreAccess`].
///
/// [`KvBlobStoreAccess::access`] is generic over its closure, so there is no
/// `&dyn KvBlobStoreAccess`. The device-logic structs need to hold a handle to the
/// store without becoming generic themselves - they are named bare in `FnMatcher`
/// expressions, as `ThermostatDeviceLogic::CLUSTER` - so this narrows the store
/// down to the three whole-blob operations they actually use.
pub trait VendorKv {
    /// Read the blob at `key` into `out`, returning how many bytes were written,
    /// or `None` if the key holds nothing.
    ///
    /// A blob longer than `out` is truncated: every caller here stores a fixed
    /// small value and reads it back with a buffer of exactly that size.
    fn load_blob(&self, key: u16, out: &mut [u8]) -> Result<Option<usize>, Error>;

    /// Write `data` to `key`.
    fn store_blob(&self, key: u16, data: &[u8]) -> Result<(), Error>;

    /// Remove `key`, which need not exist.
    fn remove_blob(&self, key: u16) -> Result<(), Error>;
}

impl<K> VendorKv for K
where
    K: KvBlobStoreAccess,
{
    fn load_blob(&self, key: u16, out: &mut [u8]) -> Result<Option<usize>, Error> {
        self.access(|store, buf| {
            let Some(data) = store.load(key, buf)? else {
                return Ok(None);
            };

            let len = data.len().min(out.len());
            out[..len].copy_from_slice(&data[..len]);

            Ok(Some(len))
        })
    }

    fn store_blob(&self, key: u16, data: &[u8]) -> Result<(), Error> {
        self.access(|store, buf| store.store(key, data, buf))
    }

    fn remove_blob(&self, key: u16) -> Result<(), Error> {
        self.access(|store, buf| store.remove(key, buf))
    }
}
