//! The Mill Gen 2 wire protocol.
//!
//! The Mill's own microcontroller keeps the temperature sensor, the triac and the
//! whole control loop; the WiFi module this board replaces only ever *talked* to
//! it, over a 9600 8N1 UART. This module is that conversation, and nothing else:
//! no peripherals, no device state beyond the receive [`Decoder`]'s own byte
//! buffer. The hardware half lives in [`crate::heater`].
//!
//! The manufacturer documents none of this: every constant below is
//! reverse-engineered, and the bytes this module leaves opaque are opaque because
//! nobody has worked out what they mean. Two rules are strict on purpose: a frame
//! ends at `0x5B` and at nothing else - notably not at `0x0A`, which is a perfectly
//! ordinary payload byte, being a temperature of 10 degC - and every received
//! checksum is verified before the frame is believed.
//!
//! # Framing
//!
//! Both directions use the same envelope, and the checksum is a plain additive
//! one - the sum of the payload bytes, truncated to 8 bits:
//!
//! ```text
//! 0x5A  <payload bytes...>  <checksum>  0x5B
//! ```

use core::fmt::{self, Display, Formatter};

/// The Mill UART's line rate. Frame format is 8N1, which is `esp-hal`'s default.
pub const BAUD_RATE: u32 = 9600;

const START: u8 = 0x5A;
const END: u8 = 0x5B;

/// Payload offsets, i.e. indices into the bytes *after* `0x5A`.
mod offset {
    /// The frame type, in both directions.
    pub const OPCODE: usize = 4;
    /// The Mill's own setpoint, in whole degC. `0` means "no reading".
    pub const SETPOINT: usize = 6;
    /// The Mill's measured room temperature, in whole degC. `0` means "no
    /// reading".
    pub const ROOM: usize = 7;
    /// `0x00` off, `0x01` heat.
    pub const MODE: usize = 9;
    /// The element right now: `0x00` idle, anything else heating.
    pub const ELEMENT: usize = 11;
}

/// The only inbound frame type that carries anything we understand.
const OPCODE_STATUS: u8 = 0xC9;

/// The shortest status frame we can parse: [`offset::ELEMENT`] has to be there.
const STATUS_MIN_LEN: usize = offset::ELEMENT + 1;

/// The longest payload the [`Decoder`] will buffer.
///
/// The real length of a status frame is not known: only the first twelve payload
/// bytes have meanings we can name, and nothing fixes an upper bound - so this is
/// generous rather than exact. A frame that overruns it is reported as
/// [`Received::TooLong`] rather than silently truncated, so a longer frame than
/// this shows up in the log instead of being mistaken for a short one.
pub const MAX_PAYLOAD: usize = 32;

/// Every outbound frame is the same size: `0x5A`, thirteen payload bytes, the
/// checksum, `0x5B`.
pub const COMMAND_LEN: usize = COMMAND_PAYLOAD_LEN + 3;

/// Thirteen: twelve bytes of command and a thirteenth `0x00` pad, with the
/// checksum computed over all thirteen.
///
/// The pad carries nothing and the Mill would very likely accept a twelve-byte
/// frame too, but a thirteen-byte one is what it is known to accept, so that is
/// what goes out.
const COMMAND_PAYLOAD_LEN: usize = 13;

/// `0x47`: power on/off. The argument is payload byte 5.
const OPCODE_SET_POWER: u8 = 0x47;
const SET_POWER_ARG: usize = 5;

/// `0x46`: set the target temperature, in whole degC. The argument is payload
/// byte 7 - the `0x06` in the template is a placeholder and is always overwritten.
const OPCODE_SET_SETPOINT: u8 = 0x46;
const SET_SETPOINT_ARG: usize = 7;

/// The two fixed templates the commands are built from, padded to thirteen bytes.
///
/// `{0x00, 0x10, ...}` looks like a Modbus "write multiple registers" frame in a
/// custom envelope, with an additive checksum where the CRC-16 would be. Nothing
/// here depends on that being true; it is only a hint for whoever decodes the
/// bytes this module still treats as opaque.
const POWER_TEMPLATE: [u8; COMMAND_PAYLOAD_LEN] = [
    0x00, 0x10, 0x06, 0x00, 0x47, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const SETPOINT_TEMPLATE: [u8; COMMAND_PAYLOAD_LEN] = [
    0x00, 0x10, 0x22, 0x00, 0x46, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00, 0x00,
];

// The templates are written out as the bytes they are on the wire, so this is what
// ties them back to the named opcodes and offsets above.
const _: () = assert!(POWER_TEMPLATE[offset::OPCODE] == OPCODE_SET_POWER);
const _: () = assert!(SETPOINT_TEMPLATE[offset::OPCODE] == OPCODE_SET_SETPOINT);

/// The sum of `payload`, truncated to 8 bits.
const fn checksum(payload: &[u8]) -> u8 {
    let mut sum: u8 = 0;
    let mut i = 0;

    while i < payload.len() {
        sum = sum.wrapping_add(payload[i]);
        i += 1;
    }

    sum
}

/// Wrap a payload in the envelope.
const fn envelope(payload: [u8; COMMAND_PAYLOAD_LEN]) -> [u8; COMMAND_LEN] {
    let mut frame = [0u8; COMMAND_LEN];
    let mut i = 0;

    frame[0] = START;

    while i < COMMAND_PAYLOAD_LEN {
        frame[i + 1] = payload[i];
        i += 1;
    }

    frame[COMMAND_LEN - 2] = checksum(&payload);
    frame[COMMAND_LEN - 1] = END;

    frame
}

/// The `0x47` frame: ask the Mill to turn heating on or off.
pub const fn power_command(heat: bool) -> [u8; COMMAND_LEN] {
    let mut payload = POWER_TEMPLATE;

    payload[SET_POWER_ARG] = if heat { 0x01 } else { 0x00 };

    envelope(payload)
}

/// The `0x46` frame: ask the Mill for a target temperature, in whole degC.
pub const fn setpoint_command(celsius: u8) -> [u8; COMMAND_LEN] {
    let mut payload = SETPOINT_TEMPLATE;

    payload[SET_SETPOINT_ARG] = celsius;

    envelope(payload)
}

const fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut i = 0;

    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }

    true
}

// Three worked examples, checked at compile time. The encoder is `const fn`
// precisely so that this costs nothing at runtime:
// there is no test harness on a bare-metal target, and a wire protocol nobody can
// run tests against still deserves a byte-for-byte fixture.
const _: () = assert!(bytes_eq(
    &power_command(true),
    &[
        0x5A, 0x00, 0x10, 0x06, 0x00, 0x47, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5E,
        0x5B
    ]
));
const _: () = assert!(bytes_eq(
    &power_command(false),
    &[
        0x5A, 0x00, 0x10, 0x06, 0x00, 0x47, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x5D,
        0x5B
    ]
));
const _: () = assert!(bytes_eq(
    &setpoint_command(21),
    &[
        0x5A, 0x00, 0x10, 0x22, 0x00, 0x46, 0x01, 0x00, 0x15, 0x00, 0x00, 0x00, 0x00, 0x00, 0x8E,
        0x5B
    ]
));

/// What the Mill says it is doing, as of one status frame.
///
/// Every field is what the heater *reports*, never what we asked for: we are an
/// advisory peer to its control loop, not the controller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Status {
    /// The Mill's own target temperature in whole degC, or `None` when the frame
    /// carried `0` - the protocol's "no reading".
    ///
    /// This is the only feedback path for local control: turn the knob on the
    /// front panel and the next status frame carries the new value.
    pub setpoint_c: Option<u8>,
    /// The measured room temperature in whole degC, or `None` for "no reading".
    pub room_c: Option<u8>,
    /// Whether the Mill is in heat mode, or `None` if byte 9 held something other
    /// than `0x00`/`0x01` - in which case the previous mode is the better guess.
    pub heat_mode: Option<bool>,
    /// Whether the element is drawing power *right now*.
    pub element_on: bool,
}

impl Status {
    /// Parse a checksum-verified payload, or `None` if it is not a status frame.
    pub fn parse(payload: &[u8]) -> Option<Self> {
        if opcode(payload) != Some(OPCODE_STATUS) || payload.len() < STATUS_MIN_LEN {
            return None;
        }

        Some(Self {
            // Both temperatures are whole degrees and unsigned on the wire, and
            // `0` is the protocol's null rather than 0 degC.
            setpoint_c: Some(payload[offset::SETPOINT]).filter(|c| *c != 0),
            room_c: Some(payload[offset::ROOM]).filter(|c| *c != 0),
            heat_mode: match payload[offset::MODE] {
                0x00 => Some(false),
                0x01 => Some(true),
                _ => None,
            },
            element_on: payload[offset::ELEMENT] != 0x00,
        })
    }
}

/// The frame type byte, for a payload long enough to have one.
pub fn opcode(payload: &[u8]) -> Option<u8> {
    payload.get(offset::OPCODE).copied()
}

/// What [`Decoder::push`] found at the end of a frame.
#[derive(Debug, Eq, PartialEq)]
pub enum Received<'a> {
    /// A complete frame whose checksum matched: the payload, with the trailing
    /// checksum byte already stripped.
    Frame(&'a [u8]),
    /// A complete frame whose checksum did not match the bytes it covered. The
    /// payload is handed over anyway - only so it can be logged.
    BadChecksum(&'a [u8]),
    /// A frame that ran past [`MAX_PAYLOAD`] before its terminator arrived. Its
    /// bytes are gone; the decoder resynchronises on the next `0x5A`.
    TooLong,
}

/// A byte-at-a-time receiver for the envelope above.
///
/// Deliberately length-agnostic - the true length of a status frame is not fixed by
/// anything we know - but *not* terminator-agnostic: only `0x5B` ends a frame, and
/// the checksum decides whether what arrived is worth believing.
pub struct Decoder {
    buf: [u8; MAX_PAYLOAD],
    len: usize,
    /// Whether a `0x5A` has been seen and no terminator since.
    in_frame: bool,
    /// Set when the current frame overran [`MAX_PAYLOAD`], so its terminator is
    /// reported once and its bytes are not mistaken for a short frame.
    overrun: bool,
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            buf: [0; MAX_PAYLOAD],
            len: 0,
            in_frame: false,
            overrun: false,
        }
    }

    /// Feed one received byte, returning a frame once its terminator arrives.
    ///
    /// Bytes outside an envelope are dropped, which is also how the decoder
    /// resynchronises: after a truncated or oversized frame it simply waits for
    /// the next `0x5A`.
    pub fn push(&mut self, byte: u8) -> Option<Received<'_>> {
        if !self.in_frame {
            if byte == START {
                self.in_frame = true;
                self.overrun = false;
                self.len = 0;
            }

            return None;
        }

        if byte != END {
            if self.len < MAX_PAYLOAD {
                self.buf[self.len] = byte;
                self.len += 1;
            } else {
                self.overrun = true;
            }

            return None;
        }

        self.in_frame = false;

        if self.overrun {
            return Some(Received::TooLong);
        }

        // The checksum is the last payload byte; a frame too short to hold one
        // carries nothing to verify and nothing to parse.
        let (&sum, data) = self.buf[..self.len].split_last()?;

        if sum == checksum(data) {
            Some(Received::Frame(data))
        } else {
            Some(Received::BadChecksum(data))
        }
    }
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

/// A byte slice as space-separated hex, for logging frames without allocating.
///
/// Most of what the Mill sends is still unparsed - bytes 0-3, 5, 8, 10 and
/// anything past 11 - so the raw frame *is* the finding. This is what puts it in
/// the log.
pub struct Hex<'a>(pub &'a [u8]);

impl Display for Hex<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }

            write!(f, "{byte:02X}")?;
        }

        Ok(())
    }
}
