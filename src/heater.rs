//! The heater: the UART link to the Mill's own microcontroller, the state it
//! reports, and the energy counters derived from it.
//!
//! This is the one module that touches hardware. It is *not* a relay driver: the
//! Mill keeps its temperature sensor, its triac and its whole control loop, and
//! this board replaces only the WiFi module that used to advise it. So the two
//! directions are asymmetric, and deliberately so:
//!
//! - **Out** ([`MillHeater::request_power`], [`MillHeater::request_setpoint`]):
//!   fire-and-forget requests. No ack, no sequence number, no retry. The next
//!   status frame is the only confirmation there is.
//! - **In** ([`MillHeater::recv_status`]): what the Mill says the room, the
//!   setpoint, the mode and the element are actually doing - including a setpoint
//!   somebody changed on the front panel, which arrives by exactly the same path.
//!
//! Nothing here runs a hysteresis loop, because there is nothing to run it on.
//! The element's state is *observed*, and the metering half
//! ([`MillHeater::active_power_mw`] and the energy counters) is derived from that
//! observation and the element's plate rating.

use core::cell::{Cell, RefCell};

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;

use esp_hal::uart::{Uart, UartRx, UartTx};
use esp_hal::Async;

use log::{debug, error, info, warn};

use crate::mill::{self, Decoder, Hex, Received, Status, COMMAND_LEN};
use crate::vendor_kv::{VendorKv, HEATING_ELEMENT_ENERGY_KEY};

/// How often the open energy measurement period is closed and published.
pub const ENERGY_PERIOD: embassy_time::Duration = embassy_time::Duration::from_secs(5);

/// How often the electrical readings are resampled. Faster than
/// [`ENERGY_PERIOD`], so that the element starting shows up promptly in
/// `ActivePower`.
pub const METER_TICK: embassy_time::Duration = embassy_time::Duration::from_secs(1);

/// How long the Mill may stay silent before everything it told us is treated as
/// stale.
///
/// The Mill pushes status frames unprompted and nothing ever polls it, so the
/// real cadence is a hardware question still to be answered - hence a timeout
/// generous enough not to trip over a slow one. When it does trip, the readings
/// go null rather than staying frozen at whatever arrived last: a thermostat that
/// keeps reporting a temperature it can no longer see is worse than one that
/// admits it does not know.
pub const STATUS_TIMEOUT: embassy_time::Duration = embassy_time::Duration::from_secs(120);

/// The rated power of the heating element, in milliwatts.
///
/// 600 W, the rating the deployed ESPHome unit is configured with
/// (`mill-salongen.yaml`). That is a *configured* number rather than a
/// measurement, and the Mill has no metering of its own to check it against -
/// every reading the Electrical Sensor clusters serve is this constant gated on
/// the element being on, so the plate rating of the unit being modded is the one
/// thing worth confirming before believing the energy figures.
pub const ELEMENT_POWER_MW: i64 = 600_000;

/// The nominal supply voltage, in millivolts.
pub const SUPPLY_VOLTAGE_MV: i64 = 230_000;

/// The nominal supply frequency, in millihertz.
pub const SUPPLY_FREQUENCY_MHZ: i64 = 50_000;

/// A purely resistive load draws all of its current in phase with the supply, so
/// its power factor is unity - 100.00%, in the hundredths of a percent
/// `PowerFactor` is expressed in.
pub const UNITY_POWER_FACTOR: i64 = 10_000;

/// How many bytes are taken off the UART at a time. One frame's worth, near
/// enough; the decoder is fed byte by byte regardless.
const RX_CHUNK: usize = 32;

/// How many malformed frames get logged in full before the logging backs off to a
/// count.
///
/// The first one is the interesting one: it says whether the Mill's checksum is
/// computed the way this decoder computes it. After that, a frame per second of
/// hex dump would only bury the rest of the log.
const MALFORMED_LOG_LIMIT: u32 = 8;

/// How many well-formed frames are logged raw at `info` before the raw logging
/// drops to `debug`.
///
/// Most of a status frame is still unidentified - bytes 0-3, 5, 8, 10 and
/// anything past 11 - as is its true length and the cadence the Mill pushes it
/// at. Those are bench questions, and the answer to all three is a handful of
/// frames at the top of the log on a default build, rather than a rebuild with
/// `ESP_LOG=debug`.
const RAW_LOG_LIMIT: u32 = 8;

/// What a status frame - or the silence that expires one - moved.
///
/// Only the two pure *readings* are reported this way, because only they are the
/// heater's alone to move. The setpoint and the power state are cluster attributes
/// as well, and reconciling those with what the Mill reports is the thermostat's
/// business - it has to weigh them against a command that may still be in flight,
/// which it can do from [`MillHeater::reported_setpoint`] and
/// [`MillHeater::reported_heat_mode`] whether or not this frame moved them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Changed {
    /// The measured room temperature.
    pub room: bool,
    /// The element.
    pub element: bool,
}

/// What a measurement tick moved, as [`MillHeater::close_period`] reports it.
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

/// The heater, and the UART that is the whole of its interface.
///
/// A plain shared reference is enough to pass it around - the whole firmware runs
/// on one `embassy` executor task, as the `Cell`s throughout already assume.
pub struct MillHeater<'a> {
    /// Outbound commands. Separate from `rx` because a command may well be sent
    /// while [`MillHeater::recv_status`] is parked on the receive half.
    ///
    /// `'static` rather than the heater's own lifetime, and not by accident: the
    /// UART halves have destructors, so a borrowed lifetime here would have to
    /// outlive the heater's drop - and the thermostat and the two meters hold
    /// `&'a MillHeater<'a>`, which is exactly that lifetime. The peripheral
    /// singletons `esp_hal::init` hands out are `'static` anyway.
    tx: RefCell<UartTx<'static, Async>>,
    /// An async mutex rather than a `RefCell`, because this one *is* held across
    /// an await: a second caller has to wait for the frame in flight rather than
    /// panic on a borrow. `NoopRawMutex` because the whole firmware is one
    /// non-`Send` executor task.
    rx: Mutex<NoopRawMutex, UartRx<'static, Async>>,
    decoder: RefCell<Decoder>,
    /// Malformed frames seen since boot.
    malformed: Cell<u32>,
    /// Well-formed frames seen since boot, and when the last one arrived in
    /// milliseconds since boot. Only [`RAW_LOG_LIMIT`] is riding on them.
    frames: Cell<u32>,
    last_frame_ms: Cell<Option<u64>>,

    /// Whether the element is drawing power, as of the last status frame.
    /// `false` while the Mill's state is unknown - which understates the energy
    /// counters rather than inventing energy that may not have been drawn.
    element_on: Cell<bool>,
    /// The measured room temperature in whole degC, or `None` until a status
    /// frame says otherwise. A live sensor reading rather than non-volatile
    /// state, so deliberately not persisted.
    room_c: Cell<Option<u8>>,
    /// The Mill's own setpoint in whole degC, as last reported.
    setpoint_c: Cell<Option<u8>>,
    /// The Mill's own power state, as last reported.
    heat_mode: Cell<Option<bool>>,
    /// When the last status frame arrived, in milliseconds since boot, or `None`
    /// while nothing has ever been heard from the Mill.
    last_status_ms: Cell<Option<u64>>,

    /// When the counters were last brought up to date, in milliseconds since
    /// boot. See [`MillHeater::integrate`].
    mark_ms: Cell<u64>,
    /// Energy drawn in the open measurement period, in milliwatt-seconds.
    period_mws: Cell<i64>,
    /// When the open measurement period began, in milliseconds since boot.
    period_start_ms: Cell<u64>,
    /// The last measurement period that actually drew something: the energy in
    /// it, in milliwatt-hours, and the window it covers in milliseconds since
    /// boot. `None` until the element has run at all, which is what makes
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

impl<'a> MillHeater<'a> {
    /// Take over the Mill UART, restoring the lifetime energy counter from `kv`.
    pub fn new(kv: &'a dyn VendorKv, uart: Uart<'static, Async>) -> Self {
        let (rx, tx) = uart.split();

        let mut buf = [0u8; 8];

        let (energy_mws, reset_at_boot) = match kv.load_blob(HEATING_ELEMENT_ENERGY_KEY, &mut buf) {
            Ok(Some(8)) => (i64::from_le_bytes(buf), false),
            _ => (0, true),
        };

        let now = embassy_time::Instant::now().as_millis();

        Self {
            tx: RefCell::new(tx),
            rx: Mutex::new(rx),
            decoder: RefCell::new(Decoder::new()),
            malformed: Cell::new(0),
            frames: Cell::new(0),
            last_frame_ms: Cell::new(None),
            element_on: Cell::new(false),
            room_c: Cell::new(None),
            setpoint_c: Cell::new(None),
            heat_mode: Cell::new(None),
            last_status_ms: Cell::new(None),
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

    /// Whether the element is drawing power, as last reported.
    pub fn heating(&self) -> bool {
        self.element_on.get()
    }

    /// The measured room temperature in 0.01degC, or `None` while the Mill's
    /// reading is unknown or stale.
    pub fn room_temperature(&self) -> Option<i16> {
        self.room_c.get().map(|c| i16::from(c) * 100)
    }

    /// The Mill's own target temperature in 0.01degC, as last reported.
    pub fn reported_setpoint(&self) -> Option<i16> {
        self.setpoint_c.get().map(|c| i16::from(c) * 100)
    }

    /// Whether the Mill reports itself in heat mode, as last reported.
    pub fn reported_heat_mode(&self) -> Option<bool> {
        self.heat_mode.get()
    }

    /// Whether anything has been heard from the Mill since boot (or since the
    /// last timeout).
    pub fn is_online(&self) -> bool {
        self.last_status_ms.get().is_some()
    }

    /// Ask the Mill to turn heating on or off - the `0x47` frame.
    pub fn request_power(&self, heat: bool) {
        info!("Mill: requesting power {}", if heat { "ON" } else { "OFF" });

        self.send(&mill::power_command(heat));
    }

    /// Ask the Mill for a target temperature in whole degC - the `0x46` frame.
    pub fn request_setpoint(&self, celsius: u8) {
        info!("Mill: requesting setpoint {celsius}C");

        self.send(&mill::setpoint_command(celsius));
    }

    /// Wait for the next status frame and fold it into the reported state.
    ///
    /// Frames that are not status frames, and frames that fail their checksum,
    /// are logged and skipped without resolving. In practice there is one caller,
    /// the thermostat's own loop; a second would simply queue on the receive
    /// half.
    pub async fn recv_status(&self) -> Changed {
        loop {
            let mut chunk = [0u8; RX_CHUNK];

            let read = {
                let mut rx = self.rx.lock().await;
                rx.read_async(&mut chunk).await
            };

            let read = match read {
                Ok(read) => read,
                Err(e) => {
                    // A framing error or a FIFO overflow costs us bytes, so the
                    // frame in flight is already lost; the decoder resynchronises
                    // on the next start marker by itself.
                    warn!("Mill: UART receive error: {e}");
                    continue;
                }
            };

            for byte in &chunk[..read] {
                let Some(status) = self.decode(*byte) else {
                    continue;
                };

                // Every status frame resolves, even one that says exactly what
                // the last one said: the caller's silence watchdog restarts when
                // this returns, so a steady-state heater repeating itself must not
                // look like a heater that has stopped talking.
                return self.apply(status);
            }
        }
    }

    /// Give up on everything the Mill last said, because it has gone quiet.
    ///
    /// Returns which readings that invalidated, so the cluster can re-report them
    /// as null.
    pub fn forget(&self) -> Changed {
        if !self.is_online() {
            return Changed::default();
        }

        warn!(
            "Mill: no status frame for {} s - the readings are now unknown",
            STATUS_TIMEOUT.as_secs()
        );

        // Credit the energy drawn up to now at the last power we had reason to
        // believe in, before the element is assumed off.
        self.integrate();

        // Back to knowing nothing, so the next frame to arrive is a first frame
        // again - and this warning is not repeated every timeout.
        self.last_status_ms.set(None);

        self.setpoint_c.set(None);
        self.heat_mode.set(None);

        Changed {
            room: self.room_c.replace(None).is_some(),
            element: self.element_on.replace(false),
        }
    }

    /// The power drawn right now, in milliwatts.
    pub fn active_power_mw(&self) -> i64 {
        if self.heating() {
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

    /// Write a command frame, whole.
    fn send(&self, frame: &[u8; COMMAND_LEN]) {
        debug!("Mill: TX {}", Hex(frame));

        let mut tx = self.tx.borrow_mut();

        // The frame is sixteen bytes into a FIFO with room for many more, so a
        // short write means something is wrong rather than merely busy - and
        // half a frame on the wire is worse than none.
        match tx.write(frame) {
            Ok(written) if written == frame.len() => {}
            Ok(written) => warn!("Mill: only {written} of {COMMAND_LEN} bytes reached the UART"),
            Err(e) => error!("Mill: UART send failed: {e}"),
        }
    }

    /// Feed one received byte to the decoder, returning a parsed status frame.
    ///
    /// Everything that is not one - a frame type we do not understand, a bad
    /// checksum, an oversized frame - is logged here and swallowed.
    fn decode(&self, byte: u8) -> Option<Status> {
        let mut decoder = self.decoder.borrow_mut();

        match decoder.push(byte)? {
            Received::Frame(payload) => {
                self.log_raw(payload);

                let status = Status::parse(payload);

                if status.is_none() {
                    // Not a defect, just a frame type the ESPHome component
                    // dropped without ever logging. Worth seeing.
                    debug!(
                        "Mill: ignoring frame with opcode {:?}",
                        mill::opcode(payload)
                    );
                }

                status
            }
            Received::BadChecksum(payload) => {
                self.log_malformed(format_args!("checksum mismatch: {}", Hex(payload)));
                None
            }
            Received::TooLong => {
                self.log_malformed(format_args!(
                    "frame longer than {} bytes",
                    mill::MAX_PAYLOAD
                ));
                None
            }
        }
    }

    /// Log a well-formed frame, raw, with the gap since the previous one - the
    /// first few loudly, the rest at `debug`.
    fn log_raw(&self, payload: &[u8]) {
        let now = embassy_time::Instant::now().as_millis();
        let gap_ms = self
            .last_frame_ms
            .replace(Some(now))
            .map(|previous| now.saturating_sub(previous));

        let count = self.frames.get().saturating_add(1);

        self.frames.set(count);

        if count <= RAW_LOG_LIMIT {
            match gap_ms {
                Some(gap_ms) => info!(
                    "Mill: RX {} ({} bytes, {gap_ms} ms after the previous frame)",
                    Hex(payload),
                    payload.len()
                ),
                None => info!(
                    "Mill: RX {} ({} bytes, the first frame)",
                    Hex(payload),
                    payload.len()
                ),
            }
        } else {
            debug!("Mill: RX {}", Hex(payload));
        }
    }

    /// Log a malformed frame, loudly at first and then only by count.
    ///
    /// The first few matter more than the rest: if the Mill computes its checksum
    /// differently from this decoder, *every* frame lands here, and the raw bytes
    /// in the log are what says so.
    fn log_malformed(&self, what: core::fmt::Arguments<'_>) {
        let count = self.malformed.get() + 1;

        self.malformed.set(count);

        if count <= MALFORMED_LOG_LIMIT {
            warn!("Mill: dropping malformed frame ({count}): {what}");
        } else if count.is_multiple_of(100) {
            warn!("Mill: {count} malformed frames dropped so far");
        }
    }

    /// Fold a status frame into the reported state.
    fn apply(&self, status: Status) -> Changed {
        let first = !self.is_online();

        self.last_status_ms
            .set(Some(embassy_time::Instant::now().as_millis()));

        if first {
            info!("Mill: first status frame: {status:?}");
        }

        let element = status.element_on != self.element_on.get();

        if element {
            // Bring the counters up to date at the old power before the element
            // state changes it, so no energy is credited at a power that was
            // never drawn.
            self.integrate();
            self.element_on.set(status.element_on);

            info!(
                "Mill: element {}",
                if status.element_on { "ON" } else { "OFF" }
            );
        }

        // A frame that carries no reading leaves the last one standing: the
        // protocol's `0` means "I have nothing to say about this", not "zero
        // degrees". Only the silence timeout invalidates a reading.
        if let Some(setpoint_c) = status.setpoint_c {
            self.setpoint_c.set(Some(setpoint_c));
        }

        if let Some(heat_mode) = status.heat_mode {
            self.heat_mode.set(Some(heat_mode));
        }

        Changed {
            room: status
                .room_c
                .is_some_and(|c| self.room_c.replace(Some(c)) != Some(c)),
            element,
        }
    }

    /// Bring both energy counters up to now, at the power drawn since the last
    /// time this ran.
    ///
    /// A real meter integrates continuously; this one integrates once per sample,
    /// which is the same thing as long as the power only changes at a sample
    /// boundary - and every path that changes the element state calls this first
    /// so that it does. Crediting a whole tick at the power sampled at its *end*
    /// would silently drop the energy of an element that stopped mid-tick and
    /// invent energy for one that started late.
    fn integrate(&self) {
        let now = embassy_time::Instant::now().as_millis();
        let elapsed_ms = now.saturating_sub(self.mark_ms.replace(now)) as i64;

        let drawn_mws = self.active_power_mw() * elapsed_ms / 1000;

        self.energy_mws.set(self.energy_mws.get() + drawn_mws);
        self.period_mws.set(self.period_mws.get() + drawn_mws);
    }
}
