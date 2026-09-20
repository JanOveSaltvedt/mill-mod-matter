//! The Thermostat cluster's device-specific half: a heating-only thermostat whose
//! four non-volatile attributes live in the Matter KV store.

use core::cell::Cell;

use log::{error, info};

use rs_matter_embassy::matter::dm::clusters::app::thermostat::{
    ControlSequenceOfOperationEnum, OutOfBandMessage, RelayStateBitmap, SystemModeEnum,
    ThermostatHooks,
};
use rs_matter_embassy::matter::dm::clusters::decl::thermostat as thermostat_cluster;
use rs_matter_embassy::matter::dm::Cluster;
use rs_matter_embassy::matter::error::Error;
use rs_matter_embassy::matter::tlv::Nullable;
use rs_matter_embassy::matter::with;

use crate::heater::{SimulatedHeater, HEATING_RATE, TICK};
use crate::vendor_kv::{VendorKv, THERMOSTAT_STATE_KEY};

/// The setpoint a device with nothing to restore comes up with: 20.00degC.
const DEFAULT_HEATING_SETPOINT: i16 = 2000;

/// How far the simulated front panel moves the setpoint per press: half a degree.
const FRONT_PANEL_STEP: i16 = 50;

/// How many [`TICK`]s between simulated front-panel presses. At a 5 s tick, once a
/// minute.
const FRONT_PANEL_TICKS: u32 = 12;

/// The four non-volatile attributes, as they are laid out in the KV store:
/// `SystemMode` as one byte, then `OccupiedHeatingSetpoint`,
/// `MinHeatSetpointLimit` and `MaxHeatSetpointLimit` as little-endian `i16`s.
///
/// `LocalTemperature` is deliberately absent - it is a live sensor reading, not
/// non-volatile state.
struct PersistentState {
    system_mode: SystemModeEnum,
    occupied_heating_setpoint: i16,
    min_heat_setpoint_limit: i16,
    max_heat_setpoint_limit: i16,
}

impl PersistentState {
    const LEN: usize = 7;

    fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut buf = [0u8; Self::LEN];

        buf[0] = self.system_mode as u8;
        buf[1..3].copy_from_slice(&self.occupied_heating_setpoint.to_le_bytes());
        buf[3..5].copy_from_slice(&self.min_heat_setpoint_limit.to_le_bytes());
        buf[5..7].copy_from_slice(&self.max_heat_setpoint_limit.to_le_bytes());

        buf
    }

    fn from_bytes(buf: &[u8; Self::LEN]) -> Option<Self> {
        // Only the two modes a heating-only thermostat can be in; anything else
        // would be rejected by the handler's startup repair anyway. The generated
        // enums get no `from_repr`, so this is a match on the discriminant.
        let system_mode = match buf[0] {
            m if m == SystemModeEnum::Off as u8 => SystemModeEnum::Off,
            m if m == SystemModeEnum::Heat as u8 => SystemModeEnum::Heat,
            _ => return None,
        };

        Some(Self {
            system_mode,
            occupied_heating_setpoint: i16::from_le_bytes([buf[1], buf[2]]),
            min_heat_setpoint_limit: i16::from_le_bytes([buf[3], buf[4]]),
            max_heat_setpoint_limit: i16::from_le_bytes([buf[5], buf[6]]),
        })
    }
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            system_mode: SystemModeEnum::Off,
            occupied_heating_setpoint: DEFAULT_HEATING_SETPOINT,
            min_heat_setpoint_limit: ThermostatDeviceLogic::ABS_MIN_HEAT_SETPOINT,
            max_heat_setpoint_limit: ThermostatDeviceLogic::ABS_MAX_HEAT_SETPOINT,
        }
    }
}

/// A heating-only thermostat driving [`SimulatedHeater`].
pub struct ThermostatDeviceLogic<'a> {
    occupied_heating_setpoint: Cell<i16>,
    min_heat_setpoint_limit: Cell<i16>,
    max_heat_setpoint_limit: Cell<i16>,
    system_mode: Cell<SystemModeEnum>,
    /// Counts `run` ticks, so the simulated front panel can nudge the setpoint
    /// once in a while.
    ticks: Cell<u32>,
    /// The load this thermostat switches. Its relay flag is the thermostat's
    /// output and the meters' input.
    heater: &'a SimulatedHeater<'a>,
    kv: &'a dyn VendorKv,
}

impl<'a> ThermostatDeviceLogic<'a> {
    pub fn new(kv: &'a dyn VendorKv, heater: &'a SimulatedHeater<'a>) -> Self {
        let mut buf = [0u8; PersistentState::LEN];

        let state = match kv.load_blob(THERMOSTAT_STATE_KEY, &mut buf) {
            Ok(Some(PersistentState::LEN)) => PersistentState::from_bytes(&buf).unwrap_or_default(),
            _ => PersistentState::default(),
        };

        Self {
            occupied_heating_setpoint: Cell::new(state.occupied_heating_setpoint),
            min_heat_setpoint_limit: Cell::new(state.min_heat_setpoint_limit),
            max_heat_setpoint_limit: Cell::new(state.max_heat_setpoint_limit),
            system_mode: Cell::new(state.system_mode),
            ticks: Cell::new(0),
            heater,
            kv,
        }
    }

    fn save_state(&self) {
        let state = PersistentState {
            system_mode: self.system_mode.get(),
            occupied_heating_setpoint: self.occupied_heating_setpoint.get(),
            min_heat_setpoint_limit: self.min_heat_setpoint_limit.get(),
            max_heat_setpoint_limit: self.max_heat_setpoint_limit.get(),
        };

        if let Err(e) = self.kv.store_blob(THERMOSTAT_STATE_KEY, &state.to_bytes()) {
            error!("Thermostat: could not persist the cluster state: {e}");
        }
    }

    /// Move the heating setpoint, the way a press of the front-panel up/down
    /// buttons would, bouncing off the configured limits.
    ///
    /// Because this happens behind the cluster's back, the handler attributes it to
    /// `Manual` rather than `External` - the whole point of `SetpointChangeSource`.
    fn nudge_setpoint(&self) {
        let previous = self.occupied_heating_setpoint.get();
        let max = self.max_heat_setpoint_limit.get();
        let min = self.min_heat_setpoint_limit.get();

        let next = if previous.saturating_add(FRONT_PANEL_STEP) > max {
            min
        } else {
            previous.saturating_add(FRONT_PANEL_STEP)
        };

        self.occupied_heating_setpoint.set(next);
        self.save_state();
        self.update_relay();

        info!(
            "Thermostat: front panel moved the heating setpoint {}.{:02}C -> {}.{:02}C",
            previous / 100,
            (previous % 100).abs(),
            next / 100,
            (next % 100).abs()
        );
    }

    /// Re-evaluate the heat demand, with a one-notch hysteresis band around the
    /// setpoint so the relay does not chatter every tick.
    fn update_relay(&self) {
        let setpoint = self.occupied_heating_setpoint.get();
        let temperature = self.heater.room_temperature();

        let heating = matches!(self.system_mode.get(), SystemModeEnum::Heat)
            && if self.heater.heating() {
                temperature < setpoint.saturating_add(HEATING_RATE)
            } else {
                temperature < setpoint.saturating_sub(HEATING_RATE)
            };

        self.heater.set_heating(heating);
    }
}

impl ThermostatHooks for ThermostatDeviceLogic<'_> {
    /// A heating-only thermostat: the `HEAT` feature alone, the four mandatory
    /// attributes plus the optional heat setpoint limits, and the one mandatory
    /// command. See the `rs_matter::dm::clusters::app::thermostat` module docs for
    /// why the limits come as a set of four.
    const CLUSTER: Cluster<'static> = thermostat_cluster::FULL_CLUSTER
        .with_revision(11)
        .with_features(thermostat_cluster::Feature::HEATING.bits())
        .with_attrs(with!(
            required;
            thermostat_cluster::AttributeId::AbsMinHeatSetpointLimit
                | thermostat_cluster::AttributeId::AbsMaxHeatSetpointLimit
                | thermostat_cluster::AttributeId::OccupiedHeatingSetpoint
                | thermostat_cluster::AttributeId::MinHeatSetpointLimit
                | thermostat_cluster::AttributeId::MaxHeatSetpointLimit
                | thermostat_cluster::AttributeId::ThermostatRunningState
                // Section 4.3.11.30-32: how the setpoint last moved, which is what
                // lets a controller tell a turn of the knob on the device from its
                // own write. Not feature-gated and not provisional, unlike the
                // `TEVT` event set, which this device leaves off.
                | thermostat_cluster::AttributeId::SetpointChangeSource
                | thermostat_cluster::AttributeId::SetpointChangeAmount
                | thermostat_cluster::AttributeId::SetpointChangeSourceTimestamp
        ))
        .with_cmds(with!(thermostat_cluster::CommandId::SetpointRaiseLower))
        .with_events(with!());

    const CONTROL_SEQUENCE_OF_OPERATION: ControlSequenceOfOperationEnum =
        ControlSequenceOfOperationEnum::HeatingOnly;

    // `utc_now_secs` is deliberately not implemented: this device has no clock of
    // its own, so `SetpointChangeSourceTimestamp` is stamped from the node's
    // Last-Known-Good UTC time, which the handler reads for itself.

    fn local_temperature(&self) -> Nullable<i16> {
        Nullable::some(self.heater.room_temperature())
    }

    fn occupied_heating_setpoint(&self) -> i16 {
        self.occupied_heating_setpoint.get()
    }

    fn set_occupied_heating_setpoint(&self, value: i16) -> Result<(), Error> {
        self.occupied_heating_setpoint.set(value);
        self.save_state();

        Ok(())
    }

    fn min_heat_setpoint_limit(&self) -> i16 {
        self.min_heat_setpoint_limit.get()
    }

    fn set_min_heat_setpoint_limit(&self, value: i16) -> Result<(), Error> {
        self.min_heat_setpoint_limit.set(value);
        self.save_state();

        Ok(())
    }

    fn max_heat_setpoint_limit(&self) -> i16 {
        self.max_heat_setpoint_limit.get()
    }

    fn set_max_heat_setpoint_limit(&self, value: i16) -> Result<(), Error> {
        self.max_heat_setpoint_limit.set(value);
        self.save_state();

        Ok(())
    }

    fn system_mode(&self) -> SystemModeEnum {
        self.system_mode.get()
    }

    fn set_system_mode(&self, value: SystemModeEnum) -> Result<(), Error> {
        self.system_mode.set(value);
        self.save_state();

        Ok(())
    }

    /// The heater is a single-stage heater with no fan, so the only relay it can ever
    /// have energised is `Heat` - and the element the metering clusters watch *is*
    /// the relay, so it can answer for itself.
    fn running_state(&self) -> RelayStateBitmap {
        if self.heater.heating() {
            RelayStateBitmap::HEAT
        } else {
            RelayStateBitmap::empty()
        }
    }

    /// A heating-only device: the cooling setpoint is whatever the hook default
    /// returns, and means nothing here.
    fn apply(&self, system_mode: SystemModeEnum, heating_setpoint: i16, _cooling_setpoint: i16) {
        info!(
            "Thermostat: system mode {:?}, heating setpoint {}.{:02}C, room {}.{:02}C",
            system_mode,
            heating_setpoint / 100,
            (heating_setpoint % 100).abs(),
            self.heater.room_temperature() / 100,
            (self.heater.room_temperature() % 100).abs(),
        );

        // Re-evaluate the relay immediately rather than waiting a tick, so that
        // switching to `Heat` has a visible effect right away.
        self.update_relay();
    }

    async fn run<F: Fn(OutOfBandMessage)>(&self, notify: F) {
        loop {
            // In a real device we would wait on a temperature sensor rather than
            // poll a simulation.
            embassy_time::Timer::after(TICK).await;

            let heating = self.heater.heating();

            // Stand in for somebody pressing the buttons on the heater's front panel,
            // so the `Manual` half of section 4.3.11.30 is visible on a running
            // device. A real device would do this from its own input handling.
            let ticks = self.ticks.get().wrapping_add(1);
            self.ticks.set(ticks);

            if ticks.is_multiple_of(FRONT_PANEL_TICKS) {
                self.nudge_setpoint();
                notify(OutOfBandMessage::OccupiedHeatingSetpoint);
            }

            let room_changed = self.heater.tick_room();

            // Re-evaluate the relay whether or not the room moved: the hysteresis
            // band is relative to the setpoint, which may have moved instead.
            self.update_relay();

            if room_changed {
                notify(OutOfBandMessage::LocalTemperature);
            }

            // The hysteresis band can move the relay with nobody having written
            // anything, so this is the one relay transition the handler cannot see
            // for itself.
            if self.heater.heating() != heating {
                notify(OutOfBandMessage::RunningState);
            }
        }
    }
}
