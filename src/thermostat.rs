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
//! **Whole degrees, on the wire only.** Matter is 0.01degC and the wire is 1degC,
//! so the command frame carries a written setpoint rounded to the nearest whole
//! degree - but the attribute keeps exactly what was written. A controller that
//! writes 21.50degC reads back 21.50degC while the heater runs at 22.
//!
//! Correcting the attribute to 22.00 would be more literal, and it would fight any
//! controller that steps in half degrees: an automation that wants 21.50 sees 22.00,
//! writes 21.50 again, and the two loop. Matter has no setpoint-resolution attribute
//! a controller could learn the step from, so the device is the one that has to give.
//! For the same reason a status frame only counts as a front-panel change when its
//! whole degree differs from the attribute's rounding.
//!
//! **The handler owns the state.** `rs-matter`'s `ThermostatHandler` keeps
//! `SystemMode`, the setpoint and the setpoint limits itself, and persists them under
//! [`THERMOSTAT_ATTRS_KEY`]. What is kept here is only a mirror - the last values
//! the handler pushed through [`ThermostatHooks::apply`] or this module reported to
//! it - which is what a status frame is compared against.

use core::cell::Cell;

use embassy_futures::select::{select, Either};

use log::{info, warn};

use rs_matter_embassy::matter::dm::clusters::app::thermostat::{
    ControlSequenceOfOperationEnum, OutOfBandMessage, RelayStateBitmap, SystemModeEnum,
    ThermostatHooks,
};
use rs_matter_embassy::matter::dm::clusters::decl::thermostat as thermostat_cluster;
use rs_matter_embassy::matter::dm::Cluster;
use rs_matter_embassy::matter::with;

use crate::heater::{Changed, MillHeater, STATUS_TIMEOUT};
#[cfg(doc)]
use crate::vendor_kv::THERMOSTAT_ATTRS_KEY;
use crate::vendor_kv::{VendorKv, LEGACY_THERMOSTAT_STATE_KEY};

/// How long a command frame is given to show up in the status stream before the
/// Mill's own report is believed over it.
///
/// Commands are fire-and-forget - no ack, no sequence number - so the only
/// confirmation is the next status frame carrying the value back. Until this
/// expires, a status frame that disagrees is assumed to have crossed the command
/// on the wire rather than to have overruled it.
const COMMAND_GRACE_MS: u64 = 15_000;

/// A heating-only thermostat that fronts a [`MillHeater`].
pub struct ThermostatDeviceLogic<'a> {
    /// `OccupiedHeatingSetpoint` as this module last knew it: pushed by the
    /// handler through [`ThermostatHooks::apply`], or adopted from the Mill and
    /// reported to the handler.
    setpoint: Cell<i16>,
    /// `SystemMode`, likewise.
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
}

impl<'a> ThermostatDeviceLogic<'a> {
    pub fn new(kv: &dyn VendorKv, heater: &'a MillHeater<'a>) -> Self {
        // Firmware before `rs-matter`'s handler took over persistence kept the same
        // four attributes in a blob of its own. Not migrated: the setpoint and the
        // mode are adopted from the Mill's first status frame anyway, and the limits
        // are the only thing lost. Removed only if present, since every removal is a
        // flash write.
        if let Ok(Some(_)) = kv.load_blob(LEGACY_THERMOSTAT_STATE_KEY, &mut [0; 1]) {
            info!("Thermostat: removing the pre-upstream state blob; the Mill's own state wins");

            if let Err(e) = kv.remove_blob(LEGACY_THERMOSTAT_STATE_KEY) {
                warn!("Thermostat: could not remove the pre-upstream state blob: {e}");
            }
        }

        Self {
            setpoint: Cell::new(Self::OCCUPIED_HEATING_SETPOINT),
            system_mode: Cell::new(Self::SYSTEM_MODE),
            applied: Cell::new(false),
            pending_setpoint: Cell::new(None),
            pending_mode: Cell::new(None),
            heater,
        }
    }

    /// Send whatever the Mill is not already doing.
    ///
    /// Suppressing a command the heater already agrees with keeps a busy controller
    /// from filling the UART with no-ops, but an *outstanding* command always
    /// wins over what the heater last reported: it may not have been taken up yet.
    fn command(&self, system_mode: SystemModeEnum, setpoint_c: u8) {
        let now = embassy_time::Instant::now().as_millis();

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

        // This runs on every status frame, so everything below it has to be
        // conditional on the setpoint actually moving - otherwise a steady heater
        // would log a line a second.
        //
        // In whole degrees: that is all the wire carries. It is what leaves a
        // written 21.50 alone while the heater reports the 22 it was commanded -
        // see the module docs. And the handler may have clamped what it was last
        // told into `Min/MaxHeatSetpointLimit`; comparing against the mirror rather
        // than the attribute keeps a heater set outside those limits from being
        // re-reported on every frame.
        if reported_c == whole_degrees(self.setpoint.get()) {
            return;
        }

        info!("Thermostat: the heater's setpoint moved to {reported_c}C on its own");

        self.setpoint.set(reported);

        // Behind the cluster's back, so the handler records this as `Manual` -
        // which is precisely what a turn of the knob on the front panel is. The
        // handler clamps it into the configured limits; the heater keeps running at
        // what it reports.
        notify(OutOfBandMessage::OccupiedHeatingSetpoint(reported));
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

        notify(OutOfBandMessage::SystemMode(mode));
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

    /// The range the Mill's own front panel offers, 5-35, rather than the
    /// rs-matter spec defaults of 7-30. The hardware itself reportedly accepts
    /// setpoints below 5degC, but the panel is what a user can see and check
    /// against.
    ///
    /// `heater.min_setpoint_celsius` and `heater.max_setpoint_celsius` in
    /// `config.toml`. These are associated consts, which is half the reason the
    /// configuration is resolved at build time rather than read at boot.
    const ABS_MIN_HEAT_SETPOINT: i16 = crate::config::ABS_MIN_HEAT_SETPOINT;
    const ABS_MAX_HEAT_SETPOINT: i16 = crate::config::ABS_MAX_HEAT_SETPOINT;

    const CONTROL_SEQUENCE_OF_OPERATION: ControlSequenceOfOperationEnum =
        ControlSequenceOfOperationEnum::HeatingOnly;

    /// What a device with nothing persisted comes up with. It only ever reaches
    /// the wire if the Mill never says otherwise.
    ///
    /// `heater.default_setpoint_celsius` in `config.toml`.
    const OCCUPIED_HEATING_SETPOINT: i16 = crate::config::DEFAULT_HEATING_SETPOINT;

    // `SystemMode` starts `Off` and the limits start at the absolute range - the
    // trait defaults, which are the right ones here.

    // `utc_now_secs` is deliberately not implemented: this device has no clock of
    // its own, so `SetpointChangeSourceTimestamp` is stamped from the node's
    // Last-Known-Good UTC time, which the handler reads for itself.

    /// `None` - reported as null - until the Mill has been heard from, and again if
    /// it goes quiet. There is no sensor on this side of the UART to fall back on.
    fn local_temperature(&self) -> Option<i16> {
        self.heater.room_temperature()
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
        self.system_mode.set(system_mode);
        self.setpoint.set(heating_setpoint);

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

        self.command(system_mode, whole_degrees(heating_setpoint));
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
