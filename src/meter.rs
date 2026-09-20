//! The Electrical Sensor device type's device-specific half: what the heater's
//! heating element is drawing right now, and what it has drawn over its lifetime.
//!
//! There is no metering hardware. The Mill reports whether its element is on and
//! nothing more, so every reading here is the element's plate rating gated on
//! that one bit, and the rest is derived from the nominal supply - which is the
//! whole reason the spec keeps the RMS and apparent quantities as separate
//! readings rather than deriving them. A current-sense chip would replace the
//! derivations in [`MillHeater`] and leave this module alone.

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

use crate::heater::{
    MillHeater, ENERGY_PERIOD, METER_TICK, SUPPLY_FREQUENCY_MHZ, SUPPLY_VOLTAGE_MV,
    UNITY_POWER_FACTOR,
};

/// The top of the metering hardware's measurable range, in milliamps.
const MAX_CURRENT_MA: i64 = 16_000;

/// The top of the metering hardware's measurable range, in milliwatts.
const MAX_POWER_MW: i64 = 3_680_000;

/// The top of the metering hardware's measurable range, in millivolts.
const MAX_VOLTAGE_MV: i64 = 400_000;

/// The top of the lifetime energy counter's range, in milliwatt-hours.
///
/// The counter is an `i64` of milliwatt-*seconds*, so this is where it rolls over -
/// which is what `MaxMeasuredValue` is asking for. Quoting `i64::MAX` there would
/// claim a range the counter cannot reach.
const MAX_ENERGY_MWH: i64 = i64::MAX / 3600;

/// The top of the `Frequency` and `PowerFactor` ranges, which the spec fixes rather
/// than the hardware.
const MAX_FREQUENCY_MHZ: i64 = 1_000_000;
const MAX_POWER_FACTOR: i64 = 10_000;

/// How accurate the meter claims to be, in hundredths of a percent.
const METER_ACCURACY: u16 = 500;

/// One `Accuracy` entry: a quantity the meter reads across `0..=$max`, at
/// [`METER_ACCURACY`] throughout that range.
///
/// Section 2.13.6.3's list is what tells a client which quantities the meter
/// actually measures, so it has to carry an entry for every reading served - which
/// is why it is as long as it is.
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
    /// A single-phase AC load: the `AC` feature, the four mandatory attributes and
    /// the optional readings that make the power figure checkable.
    const CLUSTER: Cluster<'static> = elec_pwr_meas::FULL_CLUSTER
        .with_revision(3)
        .with_features(elec_pwr_meas::Feature::ALTERNATING_CURRENT.bits())
        .with_attrs(with!(
            required;
            elec_pwr_meas::AttributeId::Voltage
                | elec_pwr_meas::AttributeId::ActiveCurrent
                | elec_pwr_meas::AttributeId::ReactiveCurrent
                | elec_pwr_meas::AttributeId::ApparentCurrent
                | elec_pwr_meas::AttributeId::ReactivePower
                | elec_pwr_meas::AttributeId::ApparentPower
                | elec_pwr_meas::AttributeId::RMSVoltage
                | elec_pwr_meas::AttributeId::RMSCurrent
                | elec_pwr_meas::AttributeId::RMSPower
                | elec_pwr_meas::AttributeId::Frequency
                | elec_pwr_meas::AttributeId::PowerFactor
        ))
        .with_cmds(with!())
        .with_events(with!());

    const POWER_MODE: PowerModeEnum = PowerModeEnum::AC;

    /// One entry per reading served. A real device would quote its meter's
    /// datasheet here.
    const ACCURACY: &'static [MeasurementAccuracy] = &[
        meter_accuracy!(Voltage, MAX_VOLTAGE_MV),
        meter_accuracy!(RMSVoltage, MAX_VOLTAGE_MV),
        meter_accuracy!(ActiveCurrent, MAX_CURRENT_MA),
        meter_accuracy!(ReactiveCurrent, MAX_CURRENT_MA),
        meter_accuracy!(ApparentCurrent, MAX_CURRENT_MA),
        meter_accuracy!(RMSCurrent, MAX_CURRENT_MA),
        meter_accuracy!(ActivePower, MAX_POWER_MW),
        meter_accuracy!(ReactivePower, MAX_POWER_MW),
        meter_accuracy!(ApparentPower, MAX_POWER_MW),
        meter_accuracy!(RMSPower, MAX_POWER_MW),
        meter_accuracy!(Frequency, MAX_FREQUENCY_MHZ),
        meter_accuracy!(PowerFactor, MAX_POWER_FACTOR),
    ];

    fn active_power(&self) -> Nullable<i64> {
        Nullable::some(self.heater.active_power_mw())
    }

    fn voltage(&self) -> Nullable<i64> {
        Nullable::some(SUPPLY_VOLTAGE_MV)
    }

    fn active_current(&self) -> Nullable<i64> {
        Nullable::some(self.heater.active_current_ma())
    }

    // A resistive element on a sinusoidal supply draws all of its current in phase:
    // the RMS readings are the readings, the apparent quantities equal the active
    // ones, and nothing is reactive.

    fn rms_voltage(&self) -> Nullable<i64> {
        Nullable::some(SUPPLY_VOLTAGE_MV)
    }

    fn rms_current(&self) -> Nullable<i64> {
        Nullable::some(self.heater.active_current_ma())
    }

    fn rms_power(&self) -> Nullable<i64> {
        Nullable::some(self.heater.active_power_mw())
    }

    fn apparent_current(&self) -> Nullable<i64> {
        Nullable::some(self.heater.active_current_ma())
    }

    fn apparent_power(&self) -> Nullable<i64> {
        Nullable::some(self.heater.active_power_mw())
    }

    fn reactive_current(&self) -> Nullable<i64> {
        Nullable::some(0)
    }

    fn reactive_power(&self) -> Nullable<i64> {
        Nullable::some(0)
    }

    fn frequency(&self) -> Nullable<i64> {
        Nullable::some(SUPPLY_FREQUENCY_MHZ)
    }

    fn power_factor(&self) -> Nullable<i64> {
        Nullable::some(UNITY_POWER_FACTOR)
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

                // Every served reading is derived from the same element state, so
                // they all move together.
                notify(elec_pwr_meas::OutOfBandMessage::Update);
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
