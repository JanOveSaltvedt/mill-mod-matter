//! The Thermostat cluster's device-specific half.
//!
//! There is no control loop here, and that is the whole shape of this module: the
//! Mill's own microcontroller owns the sensor, the triac and the hysteresis, so
//! the cluster is a *view* of that controller plus a way to make requests of it.
//!
//! - A write to `SystemMode` or `OccupiedHeatingSetpoint` arrives as
//!   [`ThermostatHooks::apply`] and is turned into a Mill command frame.
//! - Everything the attributes report - the local temperature, the running state,
//!   and the setpoint and mode the Mill actually settled on - comes back from the
//!   status stream in [`MillHeater::recv_status`].
//!
//! Two consequences worth stating plainly, because they are visible over Matter:
//!
//! **The Mill wins.** A setpoint or mode that arrives from the heater and was not
//! asked for is a front-panel change, and is adopted into the cluster attributes
//! and reported with `SetpointChangeSource = Manual` - section 4.3.11.30 exists
//! for exactly this. That includes the first status frame after boot: the
//! persisted attributes are what the device came up with, but the Mill is what
//! the device *is*.
//!
//! **Whole degrees.** Matter is 0.01degC and the wire is 1degC, so a written
//! setpoint is rounded to the nearest whole degree as it is stored, and a
//! controller that writes 21.50degC reads back 22.00degC. Rounding at the write
//! rather than in the command frame keeps the attribute honest about what the
//! hardware was actually asked for.

use core::cell::Cell;

use embassy_futures::select::{select, Either};

use log::{error, info, warn};

use rs_matter_embassy::matter::dm::clusters::app::thermostat::{
    ControlSequenceOfOperationEnum, OutOfBandMessage, RelayStateBitmap, SystemModeEnum,
    ThermostatHooks,
};
use rs_matter_embassy::matter::dm::clusters::decl::thermostat as thermostat_cluster;
use rs_matter_embassy::matter::dm::Cluster;
use rs_matter_embassy::matter::error::Error;
use rs_matter_embassy::matter::tlv::Nullable;
use rs_matter_embassy::matter::with;

use crate::heater::{Changed, MillHeater, STATUS_TIMEOUT};
use crate::vendor_kv::{VendorKv, THERMOSTAT_STATE_KEY};

/// The setpoint a device with nothing to restore comes up with. It only ever reaches
/// the wire if the Mill never says otherwise.
///
/// `heater.default_setpoint_celsius` in `config.toml`.
const DEFAULT_HEATING_SETPOINT: i16 = crate::config::DEFAULT_HEATING_SETPOINT;

/// How long a command frame is given to show up in the status stream before the
/// Mill's own report is believed over it.
///
/// Commands are fire-and-forget - no ack, no sequence number - so the only
/// confirmation is the next status frame carrying the value back. Until this
/// expires, a status frame that disagrees is assumed to have crossed the command
/// on the wire rather than to have overruled it.
const COMMAND_GRACE_MS: u64 = 15_000;

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

/// A heating-only thermostat that fronts a [`MillHeater`].
pub struct ThermostatDeviceLogic<'a> {
    occupied_heating_setpoint: Cell<i16>,
    min_heat_setpoint_limit: Cell<i16>,
    max_heat_setpoint_limit: Cell<i16>,
    system_mode: Cell<SystemModeEnum>,
    /// Whether [`ThermostatHooks::apply`] has been called at all. The handler
    /// calls it once at startup, before anything can have been written and before
    /// the Mill has been heard from; that call must not push the restored state
    /// onto a heater that may have been turned up by hand in the meantime.
    applied: Cell<bool>,
    /// A setpoint command in whole degC awaiting confirmation, and when it was
    /// sent in milliseconds since boot.
    pending_setpoint: Cell<Option<(u8, u64)>>,
    /// A power command awaiting confirmation, likewise.
    pending_mode: Cell<Option<(bool, u64)>>,
    /// The heater this thermostat advises, and the source of every reading it
    /// reports.
    heater: &'a MillHeater<'a>,
    kv: &'a dyn VendorKv,
}

impl<'a> ThermostatDeviceLogic<'a> {
    pub fn new(kv: &'a dyn VendorKv, heater: &'a MillHeater<'a>) -> Self {
        let mut buf = [0u8; PersistentState::LEN];

        let state = match kv.load_blob(THERMOSTAT_STATE_KEY, &mut buf) {
            Ok(Some(PersistentState::LEN)) => PersistentState::from_bytes(&buf).unwrap_or_default(),
            _ => PersistentState::default(),
        };

        // The restored limits are clamped into the absolute range, and the setpoint
        // into the result. The blob predates the running firmware, so a narrower
        // `heater.min/max_setpoint_celsius` in `config.toml` would otherwise leave
        // `Min/MaxHeatSetpointLimit` outside `AbsMin/AbsMaxHeatSetpointLimit`, which
        // the cluster does not allow - and it costs nothing to be robust against a
        // corrupt blob at the same time.
        let min = state
            .min_heat_setpoint_limit
            .clamp(Self::ABS_MIN_HEAT_SETPOINT, Self::ABS_MAX_HEAT_SETPOINT);
        let max = state
            .max_heat_setpoint_limit
            .clamp(min, Self::ABS_MAX_HEAT_SETPOINT);

        Self {
            occupied_heating_setpoint: Cell::new(state.occupied_heating_setpoint.clamp(min, max)),
            min_heat_setpoint_limit: Cell::new(min),
            max_heat_setpoint_limit: Cell::new(max),
            system_mode: Cell::new(state.system_mode),
            applied: Cell::new(false),
            pending_setpoint: Cell::new(None),
            pending_mode: Cell::new(None),
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

    /// Snap a setpoint in 0.01degC to the whole degree the wire can carry,
    /// keeping it inside the configured limits.
    ///
    /// Rounding is to the nearest degree, except where that would leave the
    /// `MinHeatSetpointLimit`/`MaxHeatSetpointLimit` band, in which case it goes
    /// the other way - a written setpoint must not come back out of range. A band
    /// narrower than one degree has no whole degree in it at all; then the limit
    /// wins and the command frame carries the rounding of that.
    fn quantize(&self, value: i16) -> i16 {
        let min = self.min_heat_setpoint_limit.get();
        let max = self.max_heat_setpoint_limit.get();

        let mut snapped = i16::from(whole_degrees(value)) * 100;

        if snapped > max {
            snapped -= 100;
        }

        if snapped < min {
            snapped += 100;
        }

        snapped.clamp(min, max)
    }

    /// Send whatever the Mill is not already doing.
    ///
    /// Suppressing a command the heater already agrees with keeps a busy controller
    /// from filling the UART with no-ops, but an *outstanding* command always
    /// wins over what the heater last reported: it may not have been taken up yet.
    fn command(&self, system_mode: SystemModeEnum, heating_setpoint: i16) {
        let now = embassy_time::Instant::now().as_millis();

        let setpoint_c = whole_degrees(heating_setpoint);

        let agreed = match self.pending_setpoint.get() {
            Some((pending, _)) => pending == setpoint_c,
            None => self.heater.reported_setpoint() == Some(i16::from(setpoint_c) * 100),
        };

        if !agreed {
            self.heater.request_setpoint(setpoint_c);
            self.pending_setpoint.set(Some((setpoint_c, now)));
        }

        let heat = matches!(system_mode, SystemModeEnum::Heat);

        let agreed = match self.pending_mode.get() {
            Some((pending, _)) => pending == heat,
            None => self.heater.reported_heat_mode() == Some(heat),
        };

        if !agreed {
            self.heater.request_power(heat);
            self.pending_mode.set(Some((heat, now)));
        }
    }

    /// Adopt the setpoint the Mill reports, unless a command of ours is still in
    /// flight.
    fn sync_setpoint<F: Fn(OutOfBandMessage)>(&self, notify: &F) {
        let Some(reported) = self.heater.reported_setpoint() else {
            return;
        };

        let reported_c = whole_degrees(reported);

        if let Some((pending, sent_ms)) = self.pending_setpoint.get() {
            if pending == reported_c {
                self.pending_setpoint.set(None);
            } else if elapsed_ms(sent_ms) < COMMAND_GRACE_MS {
                // Sent, not yet reflected: this frame was already on its way.
                return;
            } else {
                warn!("Thermostat: the Mill did not take up {pending}C; it reports {reported_c}C");
                self.pending_setpoint.set(None);
            }
        }

        let value = self.quantize(reported);

        // This runs on every status frame, so everything below it has to be
        // conditional on the attribute actually moving - otherwise a steady heater
        // would log a line a second.
        if value == self.occupied_heating_setpoint.get() {
            return;
        }

        info!("Thermostat: the heater's setpoint moved to {reported_c}C on its own");

        if value != reported {
            // The heater is set to something the cluster has promised a controller
            // it will not report. The limits win - a value outside them would be
            // a conformance failure - so the log is where the truth goes.
            warn!(
                "Thermostat: {reported_c}C is outside the configured \
                 {}.{:02}C-{}.{:02}C band; reporting {}.{:02}C instead",
                self.min_heat_setpoint_limit.get() / 100,
                (self.min_heat_setpoint_limit.get() % 100).abs(),
                self.max_heat_setpoint_limit.get() / 100,
                (self.max_heat_setpoint_limit.get() % 100).abs(),
                value / 100,
                (value % 100).abs(),
            );
        }

        self.occupied_heating_setpoint.set(value);
        self.save_state();

        // Behind the cluster's back, so the handler records this as `Manual` -
        // which is precisely what a turn of the knob on the front panel is.
        notify(OutOfBandMessage::OccupiedHeatingSetpoint);
    }

    /// Adopt the power state the Mill reports, unless a command of ours is still
    /// in flight.
    fn sync_mode<F: Fn(OutOfBandMessage)>(&self, notify: &F) {
        let Some(reported) = self.heater.reported_heat_mode() else {
            return;
        };

        if let Some((pending, sent_ms)) = self.pending_mode.get() {
            if pending == reported {
                self.pending_mode.set(None);
            } else if elapsed_ms(sent_ms) < COMMAND_GRACE_MS {
                return;
            } else {
                warn!(
                    "Thermostat: the Mill did not take up power {}; it reports {}",
                    if pending { "ON" } else { "OFF" },
                    if reported { "ON" } else { "OFF" }
                );
                self.pending_mode.set(None);
            }
        }

        let mode = if reported {
            SystemModeEnum::Heat
        } else {
            SystemModeEnum::Off
        };

        if mode as u8 == self.system_mode.get() as u8 {
            return;
        }

        info!("Thermostat: the heater's power state changed to {mode:?} on its own");

        self.system_mode.set(mode);
        self.save_state();

        notify(OutOfBandMessage::SystemMode);
    }

    /// Re-report whatever a status frame - or the silence that expired one -
    /// moved.
    fn report<F: Fn(OutOfBandMessage)>(&self, changed: Changed, notify: &F) {
        if changed.room {
            notify(OutOfBandMessage::LocalTemperature);
        }

        // The Mill's control loop moves the element with nobody having written
        // anything, so this is a relay transition the handler cannot see for
        // itself.
        if changed.element {
            notify(OutOfBandMessage::RunningState);
        }

        // Unconditionally, not only when this frame moved them: a command the
        // Mill never took up shows as a *disagreement* that persists across
        // frames, and nothing else would ever notice it.
        self.sync_setpoint(notify);
        self.sync_mode(notify);
    }
}

/// Round a temperature in 0.01degC to the whole degrees the Mill's wire format
/// carries.
fn whole_degrees(centi: i16) -> u8 {
    // Half away from zero. Every setpoint that reaches here is inside
    // `ABS_MIN_HEAT_SETPOINT..=ABS_MAX_HEAT_SETPOINT`, so the clamp is belt and
    // braces against a reading that never arrives.
    ((i32::from(centi) + 50).div_euclid(100)).clamp(0, i32::from(u8::MAX)) as u8
}

fn elapsed_ms(since_ms: u64) -> u64 {
    embassy_time::Instant::now()
        .as_millis()
        .saturating_sub(since_ms)
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

    /// The range the Mill's own front panel offers (`mill_panelheater_gen2.cpp`
    /// sets 5-35 as its visual min/max), rather than the rs-matter spec defaults
    /// of 7-30. The hardware reportedly accepts below 5degC - a commit that tried
    /// 3degC was reverted with "less than 5 min temp works, but the default is min
    /// 5 degrees" - but the panel is what a user can see and check against.
    ///
    /// `heater.min_setpoint_celsius` and `heater.max_setpoint_celsius` in
    /// `config.toml`. These are associated consts, which is half the reason the
    /// configuration is resolved at build time rather than read at boot.
    const ABS_MIN_HEAT_SETPOINT: i16 = crate::config::ABS_MIN_HEAT_SETPOINT;
    const ABS_MAX_HEAT_SETPOINT: i16 = crate::config::ABS_MAX_HEAT_SETPOINT;

    const CONTROL_SEQUENCE_OF_OPERATION: ControlSequenceOfOperationEnum =
        ControlSequenceOfOperationEnum::HeatingOnly;

    // `utc_now_secs` is deliberately not implemented: this device has no clock of
    // its own, so `SetpointChangeSourceTimestamp` is stamped from the node's
    // Last-Known-Good UTC time, which the handler reads for itself.

    /// Null until the Mill has been heard from, and null again if it goes quiet.
    /// There is no sensor on this side of the UART to fall back on.
    fn local_temperature(&self) -> Nullable<i16> {
        Nullable::new(self.heater.room_temperature())
    }

    fn occupied_heating_setpoint(&self) -> i16 {
        self.occupied_heating_setpoint.get()
    }

    /// Stored at the resolution the hardware can actually hold, so that what a
    /// controller reads back is what the heater was asked for.
    fn set_occupied_heating_setpoint(&self, value: i16) -> Result<(), Error> {
        self.occupied_heating_setpoint.set(self.quantize(value));
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

    /// The heater is a single-stage heater with no fan, so the only relay it can
    /// ever have energised is `Heat` - and it is the Mill that energises it, so
    /// this is a report and not a command.
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
        if !self.applied.replace(true) {
            // The startup call. Nothing has been written yet and the Mill has not
            // been heard from, so there is nothing to push - and pushing the
            // restored state would overwrite a setpoint somebody turned up by
            // hand while this board was rebooting.
            info!(
                "Thermostat: restored {system_mode:?} at {}.{:02}C; waiting for the Mill to report",
                heating_setpoint / 100,
                (heating_setpoint % 100).abs(),
            );

            return;
        }

        info!(
            "Thermostat: {system_mode:?}, heating setpoint {}.{:02}C",
            heating_setpoint / 100,
            (heating_setpoint % 100).abs(),
        );

        self.command(system_mode, heating_setpoint);
    }

    async fn run<F: Fn(OutOfBandMessage)>(&self, notify: F) {
        loop {
            // The timer restarts with every frame, so it is a watchdog on the
            // Mill rather than a poll: the heater pushes status unprompted and
            // nothing here ever asks for it.
            match select(
                self.heater.recv_status(),
                embassy_time::Timer::after(STATUS_TIMEOUT),
            )
            .await
            {
                Either::First(changed) => self.report(changed, &notify),
                // The setpoint and mode attributes deliberately keep their last
                // values here: they are cluster state that SHALL survive a reboot,
                // not sensor readings that can go null.
                Either::Second(()) => self.report(self.heater.forget(), &notify),
            }
        }
    }
}
