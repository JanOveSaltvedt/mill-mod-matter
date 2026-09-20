//! The simulated heater: the load the thermostat switches and the two Electrical
//! Sensor clusters report on.
//!
//! This is the one module the next milestone replaces wholesale. Everything a real
//! Mill Gen 2 mod needs to touch hardware for is behind this one type: closing the
//! relay ([`SimulatedHeater::set_heating`]), reading the room
//! ([`SimulatedHeater::room_temperature`]) and metering the element
//! ([`SimulatedHeater::active_power_mw`] and the energy counters). The Matter side
//! above it does not know or care that any of it is fictitious.

use core::cell::Cell;

use log::{error, info};

use crate::vendor_kv::{VendorKv, HEATING_ELEMENT_ENERGY_KEY};

/// How often the simulated room temperature is recomputed.
pub const TICK: embassy_time::Duration = embassy_time::Duration::from_secs(5);

/// How often the electrical readings are resampled. Faster than [`TICK`] so that
/// closing the relay shows up promptly in `ActivePower`.
pub const METER_TICK: embassy_time::Duration = embassy_time::Duration::from_secs(1);

/// How fast the room warms towards the setpoint while heating, in 0.01degC per
/// [`TICK`].
pub const HEATING_RATE: i16 = 20;

/// How fast the room cools towards [`AMBIENT`] while idle, in 0.01degC per
/// [`TICK`].
pub const COOLING_RATE: i16 = 10;

/// The temperature the simulated room drifts to with the heating off, in 0.01degC.
pub const AMBIENT: i16 = 1600;

/// The rated power of the heating element, in milliwatts.
///
/// A dummy 1 kW element for now. The real Mill heater's plate rating (typically
/// 250 W - 2000 W depending on the model) replaces this when the mod drives real
/// hardware; nothing else in the metering path needs to change, because every
/// other reading is derived from this and [`SUPPLY_VOLTAGE_MV`].
pub const ELEMENT_POWER_MW: i64 = 1_000_000;

/// The nominal supply voltage, in millivolts.
pub const SUPPLY_VOLTAGE_MV: i64 = 230_000;

/// The nominal supply frequency, in millihertz.
pub const SUPPLY_FREQUENCY_MHZ: i64 = 50_000;

/// A purely resistive load draws all of its current in phase with the supply, so
/// its power factor is unity - 100.00%, in the hundredths of a percent
/// `PowerFactor` is expressed in.
pub const UNITY_POWER_FACTOR: i64 = 10_000;

/// What a measurement tick moved, as [`SimulatedHeater::close_period`] reports it.
///
/// The two readings do not move together: the lifetime counter only changes when
/// the running total crosses a whole milliwatt-hour, while a measurement period
/// closes whenever the element ran during it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Accumulated {
    /// `CumulativeEnergyImported` changed.
    pub cumulative: bool,
    /// A new `PeriodicEnergyImported` reading is available.
    pub periodic: bool,
}

/// The simulated heater.
///
/// It is the single source of truth for "is the element drawing power": the
/// thermostat writes that flag from its control loop, the meters read it. A plain
/// shared reference is enough to pass it around - the whole firmware runs on one
/// `embassy` executor task, as the `Cell`s throughout already assume.
pub struct SimulatedHeater<'a> {
    /// Whether the relay is closed and the element is drawing power.
    heating: Cell<bool>,
    /// The simulated room temperature, in 0.01degC. A live sensor reading rather
    /// than non-volatile state, so deliberately not persisted: restoring a stale
    /// room temperature across a reboot would only make the simulation lie.
    room: Cell<i16>,
    /// When the counters were last brought up to date, in milliseconds since
    /// boot. See [`SimulatedHeater::integrate`].
    mark_ms: Cell<u64>,
    /// Energy drawn in the open measurement period, in milliwatt-seconds.
    period_mws: Cell<i64>,
    /// When the open measurement period began, in milliseconds since boot.
    period_start_ms: Cell<u64>,
    /// The last measurement period that actually drew something: the energy in it,
    /// in milliwatt-hours, and the window it covers in milliseconds since boot.
    /// `None` until the element has run at all, which is what makes
    /// `PeriodicEnergyImported` report null on a freshly reset device.
    period: Cell<Option<(i64, u64, u64)>>,
    /// Energy drawn over the device's lifetime.
    ///
    /// Accumulated in milliwatt-*seconds* rather than milliwatt-hours so that a
    /// whole number of seconds at a whole number of milliwatts stays exact;
    /// `CumulativeEnergyImported` wants mWh, which is just a division away.
    energy_mws: Cell<i64>,
    /// The lifetime figure in milliwatt-hours as last reported, so a closing
    /// period can say whether `CumulativeEnergyImported` actually moved.
    reported_mwh: Cell<i64>,
    /// Whether the counter started this boot at zero because there was nothing to
    /// restore - which, for a counter living in the Matter KV store, is what a
    /// factory reset leaves behind.
    reset_at_boot: bool,
    kv: &'a dyn VendorKv,
}

impl<'a> SimulatedHeater<'a> {
    /// Create the heater, restoring its lifetime energy counter from `kv`.
    pub fn new(kv: &'a dyn VendorKv) -> Self {
        let mut buf = [0u8; 8];

        let (energy_mws, reset_at_boot) = match kv.load_blob(HEATING_ELEMENT_ENERGY_KEY, &mut buf) {
            Ok(Some(8)) => (i64::from_le_bytes(buf), false),
            _ => (0, true),
        };

        let now = embassy_time::Instant::now().as_millis();

        Self {
            heating: Cell::new(false),
            room: Cell::new(AMBIENT),
            mark_ms: Cell::new(now),
            period_mws: Cell::new(0),
            period_start_ms: Cell::new(now),
            period: Cell::new(None),
            energy_mws: Cell::new(energy_mws),
            reported_mwh: Cell::new(energy_mws / 3600),
            reset_at_boot,
            kv,
        }
    }

    /// Open or close the relay.
    pub fn set_heating(&self, heating: bool) {
        if heating != self.heating.get() {
            info!("Heater: heating {}", if heating { "ON" } else { "OFF" });
        }

        // Bring the counters up to date at the old power before the relay changes
        // it, so no energy is credited at a power that was never drawn.
        self.integrate();
        self.heating.set(heating);
    }

    /// Whether the element is currently drawing power.
    pub fn heating(&self) -> bool {
        self.heating.get()
    }

    /// The simulated room temperature, in 0.01degC.
    pub fn room_temperature(&self) -> i16 {
        self.room.get()
    }

    /// Advance the room simulation by one [`TICK`], returning `true` if the
    /// temperature changed.
    ///
    /// In a real device this is where a temperature sensor reading lands.
    pub fn tick_room(&self) -> bool {
        let previous = self.room.get();

        let room = if self.heating.get() {
            previous.saturating_add(HEATING_RATE)
        } else {
            previous.saturating_sub(COOLING_RATE).max(AMBIENT)
        };

        self.room.set(room);

        room != previous
    }

    /// The power drawn right now, in milliwatts.
    pub fn active_power_mw(&self) -> i64 {
        if self.heating.get() {
            ELEMENT_POWER_MW
        } else {
            0
        }
    }

    /// The current drawn right now, in milliamps, derived from the power and the
    /// nominal supply voltage so that the three reported readings stay consistent
    /// with one another.
    pub fn active_current_ma(&self) -> i64 {
        self.active_power_mw() * 1000 / SUPPLY_VOLTAGE_MV
    }

    /// The energy drawn over the device's lifetime, in milliwatt-hours.
    pub fn energy_mwh(&self) -> i64 {
        self.energy_mws.get() / 3600
    }

    /// Whether the lifetime counter was zeroed at this boot, as far as the device
    /// can tell.
    ///
    /// It has no wall clock and no record of resets before the current boot, so
    /// the only reset it can date is the one it came up from: nothing was there to
    /// restore. Anything earlier is unknown, and reads as null.
    pub fn reset_at_boot(&self) -> bool {
        self.reset_at_boot
    }

    /// The last measurement period that drew anything: its energy in
    /// milliwatt-hours, and the window it covers in milliseconds since boot.
    pub fn last_period(&self) -> Option<(i64, u64, u64)> {
        self.period.get()
    }

    /// Bring both energy counters up to now, at the power drawn since the last
    /// time this ran.
    ///
    /// A real meter integrates continuously; this one integrates once per sample,
    /// which is the same thing as long as the power only changes at a sample
    /// boundary - and [`SimulatedHeater::set_heating`] calls this first so that it
    /// does. Sampling the relay at the *end* of a fixed tick and crediting the
    /// whole tick at that power would silently drop the energy of a relay that
    /// opened mid-tick and invent energy for one that closed late.
    fn integrate(&self) {
        let now = embassy_time::Instant::now().as_millis();
        let elapsed_ms = now.saturating_sub(self.mark_ms.replace(now)) as i64;

        let drawn_mws = self.active_power_mw() * elapsed_ms / 1000;

        self.energy_mws.set(self.energy_mws.get() + drawn_mws);
        self.period_mws.set(self.period_mws.get() + drawn_mws);
    }

    /// Close the open measurement period and report which readings moved.
    pub fn close_period(&self) -> Accumulated {
        self.integrate();

        let end = embassy_time::Instant::now().as_millis();
        let start = self.period_start_ms.replace(end);
        let drawn_mws = self.period_mws.replace(0);

        // A period in which nothing was drawn carries no information, and
        // publishing one every tick would have an idle device emitting
        // `PeriodicEnergyMeasured` forever. The window is still advanced, so the
        // next period covers only the time the element actually ran.
        if drawn_mws == 0 {
            return Accumulated::default();
        }

        self.period.set(Some((drawn_mws / 3600, start, end)));

        let reported = self.reported_mwh.replace(self.energy_mwh());

        // Only on a period that drew something, so an idle device does not rewrite
        // flash every tick - NOR flash has a write budget, and this one is shared
        // with the Matter state.
        if let Err(e) = self.kv.store_blob(
            HEATING_ELEMENT_ENERGY_KEY,
            &self.energy_mws.get().to_le_bytes(),
        ) {
            error!("Heater: could not persist the energy counter: {e}");
        }

        Accumulated {
            cumulative: self.energy_mwh() != reported,
            periodic: true,
        }
    }
}
