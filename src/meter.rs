//! The Electrical Sensor device type's device-specific half: what the heater's
//! heating element is drawing right now, and what it has drawn over its lifetime.
//!
//! There is no metering hardware of any kind. The Mill reports whether its
//! element is on and nothing more, so the one reading served here is the
//! element's plate rating gated on that single bit. Nothing else is served:
//! voltage, current, frequency, power factor and the RMS, apparent and reactive
//! quantities would all have to be *derived* from a nominal supply this board
//! never measures, and a client cannot tell a derived reading from a measured
//! one - it would just see a meter claiming 230 V and 2.6 A that is in truth
//! reporting one bit. `ActivePower` is mandatory and honest about what it is;
//! the optional readings would not be.
//!
//! A current-sense chip is what would change this. It would replace
//! [`MillHeater::active_power_mw`] with a real measurement and could then earn
//! the optional readings back, one served attribute per quantity it actually
//! reads.

use core::cell::Cell;

use rs_matter_embassy::matter::dm::clusters::app::elec_energy_meas::{
    self, ElecEnergyMeasHooks, EnergyMeasurement, Timestamp,
};
use rs_matter_embassy::matter::dm::clusters::app::elec_pwr_meas::{
    self, ElecPwrMeasHooks, PowerModeEnum,
};
use rs_matter_embassy::matter::dm::clusters::app::measurement::{
    MeasurementAccuracy, MeasurementAccuracyRange,
};
use rs_matter_embassy::matter::dm::clusters::decl::globals::MeasurementTypeEnum;
use rs_matter_embassy::matter::dm::Cluster;
use rs_matter_embassy::matter::tlv::Nullable;
use rs_matter_embassy::matter::with;

use crate::heater::{MillHeater, ENERGY_PERIOD, METER_TICK};

/// The top of the power reading's range, in milliwatts.
///
/// A 16 A single-phase circuit at 230 V, which is the most the element could
/// draw whatever it turns out to be rated at.
const MAX_POWER_MW: i64 = 3_680_000;

/// The top of the lifetime energy counter's range, in milliwatt-hours.
///
/// The counter is an `i64` of milliwatt-*seconds*, so this is where it rolls over -
/// which is what `MaxMeasuredValue` is asking for. Quoting `i64::MAX` there would
/// claim a range the counter cannot reach.
const MAX_ENERGY_MWH: i64 = i64::MAX / 3600;

/// How accurate the readings claim to be, in hundredths of a percent.
///
/// Not a meter's datasheet figure, because there is no meter: everything served
/// is the element's plate rating gated on the on/off bit, so this is a claim
/// about how close that rating is to the truth - manufacturing tolerance, and a
/// supply that is only nominally 230 V.
const METER_ACCURACY: u16 = 500;

/// One `Accuracy` entry: a quantity the meter reads across `0..=$max`, at
/// [`METER_ACCURACY`] throughout that range.
///
/// Section 2.13.6.3's list is what tells a client which quantities the meter
/// actually measures, so it carries one entry per reading served - which, here,
/// is one.
macro_rules! meter_accuracy {
    ($type:ident, $max:expr) => {
        MeasurementAccuracy::new(
            MeasurementTypeEnum::$type,
            0,
            $max,
            &[MeasurementAccuracyRange::percent(0, $max, METER_ACCURACY)],
        )
    };
}

/// What the element is drawing right now.
pub struct ElecPwrDeviceLogic<'a> {
    heater: &'a MillHeater<'a>,
    /// The last `ActivePower` handed to a subscriber, so that
    /// [`ElecPwrMeasHooks::run`] only notifies when the reading actually moved.
    reported_power_mw: Cell<i64>,
}

impl<'a> ElecPwrDeviceLogic<'a> {
    pub fn new(heater: &'a MillHeater<'a>) -> Self {
        Self {
            heater,
            reported_power_mw: Cell::new(heater.active_power_mw()),
        }
    }
}

impl ElecPwrMeasHooks for ElecPwrDeviceLogic<'_> {
    /// A single-phase AC load, with the four mandatory attributes and nothing
    /// else. The `AC` feature is not a claim to measure anything - one of `AC`
    /// and `DC` has to be selected and `PowerMode` has to agree with it (section
    /// 2.13.5) - but it does gate nine of the optional readings, none of which
    /// this device is in any position to serve.
    const CLUSTER: Cluster<'static> = elec_pwr_meas::FULL_CLUSTER
        .with_revision(3)
        .with_features(elec_pwr_meas::Feature::ALTERNATING_CURRENT.bits())
        .with_attrs(with!(required))
        .with_cmds(with!())
        .with_events(with!());

    const POWER_MODE: PowerModeEnum = PowerModeEnum::AC;

    /// One entry, for the one reading served.
    const ACCURACY: &'static [MeasurementAccuracy] = &[meter_accuracy!(ActivePower, MAX_POWER_MW)];

    fn active_power(&self) -> Nullable<i64> {
        Nullable::some(self.heater.active_power_mw())
    }

    async fn run<F: Fn(elec_pwr_meas::OutOfBandMessage)>(&self, notify: F) {
        loop {
            // Sampling rather than waiting on an event: the element state this
            // is derived from arrives on the Mill's own schedule, which the
            // thermostat loop owns. A metering chip would be awaited here instead.
            embassy_time::Timer::after(METER_TICK).await;

            let power = self.heater.active_power_mw();

            if power != self.reported_power_mw.get() {
                self.reported_power_mw.set(power);

                notify(elec_pwr_meas::OutOfBandMessage::ActivePower);
            }
        }
    }
}

/// What the element has drawn over the device's lifetime.
pub struct ElecEnergyDeviceLogic<'a> {
    heater: &'a MillHeater<'a>,
}

impl<'a> ElecEnergyDeviceLogic<'a> {
    pub fn new(heater: &'a MillHeater<'a>) -> Self {
        Self { heater }
    }
}

impl ElecEnergyMeasHooks for ElecEnergyDeviceLogic<'_> {
    /// Imported energy, both ways round: `IMPE | CUME | PERE`. Each of the latter
    /// two brings an attribute and makes the matching event mandatory - the
    /// lifetime total the device has drawn, and the energy of the most recent
    /// measurement period.
    const CLUSTER: Cluster<'static> = elec_energy_meas::FULL_CLUSTER
        .with_revision(2)
        .with_features(
            elec_energy_meas::Feature::IMPORTED_ENERGY.bits()
                | elec_energy_meas::Feature::CUMULATIVE_ENERGY.bits()
                | elec_energy_meas::Feature::PERIODIC_ENERGY.bits(),
        )
        .with_attrs(with!(
            required;
            elec_energy_meas::AttributeId::CumulativeEnergyImported
                | elec_energy_meas::AttributeId::PeriodicEnergyImported
                | elec_energy_meas::AttributeId::CumulativeEnergyReset
        ))
        .with_cmds(with!())
        .with_events(with!(
            elec_energy_meas::EventId::CumulativeEnergyMeasured
                | elec_energy_meas::EventId::PeriodicEnergyMeasured
        ));

    const ACCURACY: MeasurementAccuracy = meter_accuracy!(ElectricalEnergy, MAX_ENERGY_MWH);

    fn cumulative_energy_imported(&self) -> Option<EnergyMeasurement> {
        // This device has no wall clock, so the reading is located in time by
        // uptime alone - which is exactly what section 2.12.5.2.5 asks for.
        Some(EnergyMeasurement::cumulative(
            self.heater.energy_mwh(),
            Timestamp::systime(embassy_time::Instant::now().as_millis()),
        ))
    }

    fn cumulative_energy_reset(&self) -> Option<Timestamp> {
        self.heater.reset_at_boot().then(|| Timestamp::systime(0))
    }

    /// Section 2.12.5.2: a periodic reading needs both ends of its window. This
    /// device has no wall clock, so both are uptimes.
    fn periodic_energy_imported(&self) -> Option<EnergyMeasurement> {
        let (energy, start, end) = self.heater.last_period()?;

        Some(EnergyMeasurement::periodic(
            energy,
            Timestamp::systime(start),
            Timestamp::systime(end),
        ))
    }

    async fn run<F: Fn(elec_energy_meas::OutOfBandMessage)>(&self, notify: F) {
        loop {
            embassy_time::Timer::after(ENERGY_PERIOD).await;

            // Closing the period here rather than in the thermostat's own tick
            // keeps the energy counter owned by the cluster that reports it.
            let moved = self.heater.close_period();

            // Two notifications from one tick, which the handler's pending mask
            // keeps apart - see `elec_energy_meas::OutOfBandMessage`.
            if moved.cumulative {
                notify(elec_energy_meas::OutOfBandMessage::CumulativeEnergyImported);
            }

            if moved.periodic {
                notify(elec_energy_meas::OutOfBandMessage::PeriodicEnergyImported);
            }
        }
    }
}
