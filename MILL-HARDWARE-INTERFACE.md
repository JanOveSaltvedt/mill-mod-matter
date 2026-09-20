# The Mill Gen 2 hardware interface

How the mod actually talks to the panel heater, reverse-engineered from the
working ESPHome implementation. This is the reference for replacing
`src/heater.rs` with real I/O (milestone 2); everything here is *what the
shipped ESPHome build does*, not a proposal.

> **Headline:** the mod does **not** drive a relay and does **not** read a
> thermistor. The Mill's own microcontroller keeps the temperature sensor, the
> triac/relay and the whole control loop. Our board replaces the **WiFi module**
> and speaks that module's **9600 baud UART protocol**: it *receives* status and
> *sends* setpoint and on/off requests. See [What this means for our
> code](#what-this-means-for-our-code) - it changes `thermostat.rs`, not just
> `heater.rs`.

## Source of truth

Everything below is derived from these files. Paths are absolute so they can be
opened and diffed directly.

| What | Where |
| --- | --- |
| ESPHome component (current) | `/home/user/workspace/homelab/esphome/esphome/esphome/components/mill_panelheater_gen2/` |
| &nbsp;&nbsp;repo / branch / commit | fork of esphome, branch `dev-mill`, `941ae474b` "Add power measurement to mill device" |
| &nbsp;&nbsp;upstream of the fork | `https://github.com/JanOveSaltvedt/esphome` ref `dev-mill` |
| Device config that uses it | `/home/user/workspace/homelab/esphome/config/mill-salongen.yaml` |
| Original code it was ported from | commit `e97424713`, "Merged code from old custom component from `https://github.com/JDolven/Replacing-HF_LPT120A-in-a-millheat-heater`", file `esphome/components/mill_gen2/mill_gen2.cpp` |

The three component files, as cited throughout:

- `climate.py` - the ESPHome config schema (wattage, power sensor).
- `mill_panelheater_gen2.h` - the offset/marker constants and the two command templates.
- `mill_panelheater_gen2.cpp` - framing, parsing, command building, checksum.

To see the original (pre-cleanup) version, which is sometimes clearer about intent
and carries the author's Norwegian comments:

```sh
cd /home/user/workspace/homelab/esphome/esphome
git show e97424713:esphome/components/mill_gen2/mill_gen2.cpp
```

## The device configuration

`/home/user/workspace/homelab/esphome/config/mill-salongen.yaml` is the hardware
description for the deployed unit - the board, the pin assignment, the console
routing, the radio and the element rating. It is 79 lines; every hardware-relevant
line is accounted for below.

> **It also holds three secrets** - the API encryption key (line 21), the OTA
> password (line 25) and, inside the Thread dataset (line 44), the network key and
> PSKc. Do not copy the file or the dataset TLV into this repository. The
> non-secret radio parameters are reproduced below because they are broadcast over
> the air anyway.

### Board and toolchain (lines 7-11)

| | |
| --- | --- |
| Board | `esp32-c6-devkitc-1` |
| Framework | `esp-idf`, `log_level: DEBUG` |

Same MCU as this firmware, so the pin assignment carries over directly. Note the
ESPHome build ran on a **devkit**, not on a board fitted into the heater - so the
pin choice reflects convenience on a breadboard, not a constraint of the Mill
header.

### Console routing (lines 13-16)

```yaml
logger:
  hardware_uart: USB_SERIAL_JTAG
  level: INFO
```

Deliberate, and directly relevant to us: the log console is moved onto the C6's
**USB Serial/JTAG** peripheral so that the hardware UART is free for the Mill. Our
firmware must do the same - `espflash flash --monitor` already talks over USB, so
this costs nothing, but it does mean the Mill UART and the monitor cannot share
pins.

### The Mill UART (lines 57-61)

From the YAML and `mill_panelheater_gen2.cpp:25` (`check_uart_settings(9600)`):

| | |
| --- | --- |
| UART TX | `GPIO16` - ESP → Mill MCU |
| UART RX | `GPIO17` - Mill MCU → ESP |
| Baud | 9600 |
| Frame format | 8N1 - ESPHome's `uart:` defaults (`esphome/components/uart/__init__.py:263-265`), never overridden in the YAML |
| Flow control | none |
| RX buffer | 256 bytes - ESPHome's default; ~26 status frames' worth, so a slow poll never drops one |

The module being replaced is an **HF-LPT120A** WiFi module (named in the upstream
project title, see the table above), so the header is a 3.3 V TTL UART. Levels,
pin order and the supply rail are not documented in the code - confirm against the
physical board before wiring.

`GPIO16`/`GPIO17` do not clash with this firmware's factory-reset pin (`GPIO9`).

### Radio (lines 38-44)

WiFi is **commented out** (lines 27-35, left in place as a record)
and the device runs Thread-only, with IPv6 explicitly enabled:

```yaml
network:
  enable_ipv6: true

openthread:
  device_type: FTD
  force_dataset: true
  tlv: <operational dataset - contains the network key, not reproduced here>
```

Two things to carry over:

- **`device_type: FTD`.** A mains-powered panel heater is always on, so it joins as
  a Full Thread Device and is router-eligible - not an SED/MTD. Our firmware should
  match; there is no battery to protect and a router in the room is useful.
- **`force_dataset: true` does not apply to us.** The ESPHome build was handed a
  hardcoded operational dataset and skipped commissioning entirely. Our device gets
  its dataset from BLE commissioning, which is the whole point of milestone 1. The
  dataset is only useful here as a *bench-verification* reference - it tells you
  which network the heater was previously on.

For that purpose only, the non-secret parameters of that dataset:

| | |
| --- | --- |
| Network name | `bl9` |
| Channel | 17 |
| PAN ID | `0x5847` |
| Extended PAN ID | `855c9175b96f7bc7` |
| Mesh-local prefix | `fdd8:ad58:c66f:1118::/64` |

### The element (lines 63-79)

Covered in [Metering](#metering): `wattage: 600` and the Home Assistant
`integration` helper that our KV-backed counter replaces.

### The component source (lines 48-54)

```yaml
external_components:
  source:
    type: git
    url: https://github.com/JanOveSaltvedt/esphome
    ref: dev-mill
  refresh: 0s
  components: [ mill_panelheater_gen2 ]
```

Confirms that the deployed firmware is built from the `dev-mill` branch cited in
[Source of truth](#source-of-truth), not from some other local copy.

## Framing

Both directions use the same envelope:

```
0x5A  <payload bytes...>  <checksum>  0x5B
```

- `0x5A` START (`mill_panelheater_gen2.h:32`)
- `0x5B` END (`mill_panelheater_gen2.h:33`)
- `0x0A` is *also* treated as an end-of-frame terminator on receive
  (`mill_panelheater_gen2.h:34`, used at `.cpp:61`)
- checksum = **sum of the payload bytes, truncated to 8 bits**
  (`mill_panelheater_gen2.cpp:122-128`)

The receiver (`recv_with_start_end_markers_`, `mill_panelheater_gen2.cpp:57-74`) is
length-agnostic: it waits for `0x5A`, then appends every byte to a 15-byte buffer
until it sees `0x5B` or `0x0A`, then raises `new_data_`. **It never verifies the
received checksum**, and it never checks the frame length.

## Receive: the status frame

Only one inbound frame type is consumed. It is selected by payload byte 4 being
`0xC9`; everything else is dropped (`mill_panelheater_gen2.cpp:33`).

Offsets are **into the payload**, i.e. index 0 is the first byte *after* `0x5A`
(constants at `mill_panelheater_gen2.h:26-30`, parsing at `.cpp:33-53`):

| Offset | Const | Meaning | Encoding |
| --- | --- | --- | --- |
| 4 | `COMMAND_TYPE_POS` | frame type | `0xC9` = status; anything else ignored |
| 6 | `TARGET_TEMP_POS` | the Mill's current **setpoint** | whole °C, `0` means "no reading", ignored |
| 7 | `CURRENT_TEMP_POS` | the Mill's measured **room temperature** | whole °C, `0` means "no reading", ignored |
| 9 | `MODE_POS` | power state | `0x00` = off, `0x01` = heat |
| 11 | `ACTION_POS` | element state right now | `0x00` = idle, anything else = heating |

Bytes 0-3, 5, 8, 10 and anything past 11 are **not parsed** and their meaning is
unknown. Total frame length is unknown too - the reader just consumes to the
terminator, so at least 12 payload bytes arrive.

The Mill **pushes** these frames unprompted; nothing in the component ever polls or
requests a status. The cadence is not recorded anywhere and should be measured on
hardware (see [Open questions](#open-questions)).

Byte 6 is what makes the **front panel visible to us**: turn the knob on the heater
and the next status frame carries the new setpoint. That is the only feedback path
for local control.

## Transmit: the two commands

Two fixed 12-byte templates, mutated in place before each send
(`mill_panelheater_gen2.h:46-47`):

```c
uint8_t power_command_[12]       = {0x00, 0x10, 0x06, 0x00, 0x47, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00};
uint8_t temperature_command_[12] = {0x00, 0x10, 0x22, 0x00, 0x46, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00};
```

Payload byte 4 is the opcode and selects which byte carries the argument
(`mill_panelheater_gen2.cpp:104-120`):

| Opcode (byte 4) | Command | Argument byte | Values |
| --- | --- | --- | --- |
| `0x47` | power on/off | 5 | `0x00` off, `0x01` heat |
| `0x46` | set target temperature | 7 | whole °C (the `0x06` in the template is a placeholder, always overwritten) |

`send_command_` then writes, in order (`.cpp:114-119`):

1. `0x5A`
2. `len + 1` = **13** payload bytes - the 12 template bytes plus one trailing
   padding byte
3. the checksum, computed over those same 13 bytes
4. `0x5B`

So every outbound frame is **16 bytes** on the wire.

### Worked examples

Verify any reimplementation against these three byte-for-byte:

```
Power ON      5A 00 10 06 00 47 01 00 00 00 00 00 00 00 5E 5B
Power OFF     5A 00 10 06 00 47 00 00 00 00 00 00 00 00 5D 5B
Setpoint 21C  5A 00 10 22 00 46 01 00 15 00 00 00 00 PP CC 5B
```

`0x5E` = `0x10 + 0x06 + 0x47 + 0x01`; `0x5D` is the same without the `0x01`. For
the setpoint frame the fixed part sums to `0x8E` (`0x10 + 0x22 + 0x46 + 0x01 +
0x15`), and `CC = 0x8E + PP` - see the next section for why `PP` is not reliably
zero in the C++.

### The 13th byte is a C++ bug, and it matters

The templates are `uint8_t[12]`, but `send_command_` writes and checksums index 12.
That is one past the end of both arrays (`mill_panelheater_gen2.cpp:111,113,115`):

- For the **power** command, `command_array[len] = 0x00` writes out of bounds. In
  the current member layout the byte after `power_command_` is
  `temperature_command_[0]`, which is already `0x00`, so this happens to be
  harmless and the padding byte really is `0x00`.
- For the **temperature** command, nothing sets index 12 at all, and
  `temperature_command_` is the last data member - so the 13th payload byte and the
  checksum that covers it are **whatever memory follows the object**.

That build works in practice. There are only two explanations, and which one is
true decides our implementation:

1. the Mill MCU **does not validate the checksum** (and tolerates the junk byte), or
2. the trailing byte is reliably `0x00` on this build, so the frame is accidentally
   correct.

Assume the protocol intends **13 payload bytes, the 13th being `0x00` padding, with
a correct additive checksum over all 13**, and send exactly that. It is correct
under both explanations. Then confirm on hardware, per [Open
questions](#open-questions).

### Speculation, flagged as such

`{0x00, 0x10, ...}` looks like a Modbus "write multiple registers" (function
`0x10`) frame wrapped in a custom `0x5A`/`0x5B` envelope with an additive checksum
replacing the CRC-16. Nothing depends on this being true - it is only a hint for
anyone trying to decode the unparsed bytes.

## Control model

This is the part that does not map onto the current simulation.

```
   Matter/Thread                 our ESP32-C6                     Mill Gen 2 MCU
 ----------------                ------------                   ----------------
  SystemMode write   ---->   0x47 frame   -------------------->  power on/off
  Setpoint write     ---->   0x46 frame   -------------------->  target temperature
                                                                      |
                                                                 control loop,
                                                                 NTC, triac
                                                                      |
  LocalTemperature   <----   status[7]    <--------------------  measured °C
  Setpoint (echo)    <----   status[6]    <--------------------  setpoint, incl.
  SystemMode         <----   status[9]    <--------------------  front-panel changes
  RunningState       <----   status[11]   <--------------------  element on/off
```

We are an **advisory peer**, not the controller. Consequences:

- There is **no hysteresis loop to run on our side**. The Mill decides when the
  element is on; we only observe byte 11.
- Commands are **fire-and-forget**. No ack, no retry, no sequence number. ESPHome
  applies the requested mode/setpoint optimistically and lets the next status frame
  correct it (`mill_panelheater_gen2.cpp:92-93,99-100`).
- On boot we know **nothing** until the first `0xC9` frame arrives.
- **Whole degrees only**, in both directions. Matter is 0.01 °C.

## Metering

There is no real metering. `wattage` is a configured constant and the power sensor
reports it verbatim while the element is on (`mill_panelheater_gen2.cpp:48-51`):

```cpp
this->power_sensor_->publish_state(
    this->action == climate::CLIMATE_ACTION_HEATING ? this->wattage_ : 0.0f);
```

`climate.py:41-47` enforces that `power:` requires `wattage:`. The deployed unit is
configured **`wattage: 600`** (`mill-salongen.yaml:67`) - a 600 W panel heater, not
the 1 kW placeholder in `heater.rs`.

Energy was not computed on-device at all: `mill-salongen.yaml:72-79` pushes it to
Home Assistant's `integration` platform (`time_unit: h`, `restore: true`,
`state_class: total_increasing`). Our firmware already does this natively and more
precisely in `SimulatedHeater::integrate` / `close_period` - the KV-backed lifetime
counter is the direct replacement for that HA helper, and needs no change beyond
the wattage constant.

## Temperature range

| Source | Value |
| --- | --- |
| ESPHome visual min/max (`mill_panelheater_gen2.cpp:12-13`) | 5 °C - 35 °C |
| Commit `3e0a203e3` "Testing min" | tried 3 °C |
| Commit `d3888fdb4` | reverted to 5, message: *"Less than 5 min temp works, but the default is min 5 degrees"* |
| Our current `ABS_MIN/MAX_HEAT_SETPOINT` (rs-matter defaults) | 700 / 3000 (7.00 °C / 30.00 °C) |

So the Mill itself accepts below 5 °C, but 5-35 is the range the panel offers. Our
`AbsMin/AbsMaxHeatSetpointLimit` are `fixed`-quality consts and should be set to
match the hardware - `500`/`3500` is the defensible choice - rather than left at the
rs-matter spec defaults.

## Known weaknesses of the ESPHome implementation

Do not port these. Each is a real defect, listed so nobody reproduces it while
"implementing it the same way".

- **`0x0A` terminates a frame mid-payload.** A payload byte of `0x0A` is decimal
  10 - i.e. a room temperature or setpoint of exactly **10 °C**
  (`mill_panelheater_gen2.cpp:61`). When that happens the frame is truncated, the
  index resets, `new_data_` fires anyway, and bytes 6/7/9/11 are read from the
  **previous** frame still sitting in the buffer. Result: silently stale readings
  around 10 °C. Frame on `0x5A`/`0x5B` with an expected length instead, and
  **validate the checksum**.
- **The received checksum is never checked** (`.cpp:57-74`). A corrupted frame is
  accepted as truth.
- **Only the last frame in the UART buffer survives.** `recv_with_start_end_markers_`
  drains everything available in one pass, each frame restarting at index 0, and
  `loop()` parses once. If the buffer ends mid-frame, the parse sees a mixture of
  the new partial frame and the previous one.
- **Buffer overrun on transmit** - the 13th byte, described above.
- **Dead store**: `mill_panelheater_gen2.cpp:41-42` sets `action = OFF` in the
  mode-off branch, then line 46 unconditionally overwrites `action` from byte 11.
  The `OFF` action is never reported. (The original at `e97424713` did not have
  this; it only set `mode` there.)
- **`char` signedness.** The receive buffer is `char[]` and byte 4 needs an explicit
  `(uint8_t)` cast (`.cpp:33`). Parse everything as `u8` in Rust and the class of
  bug disappears.

## What this means for our code

### `src/heater.rs` - rewritten, but not as a relay driver

The current API is roughly right in *shape*; the semantics of two methods invert.

| Today | Becomes |
| --- | --- |
| `set_heating(bool)` - closes the relay | **gone.** We do not command the element. Replace with `set_power(bool)` → `0x47` frame and `set_setpoint(i16)` → `0x46` frame |
| `heating() -> bool` | unchanged shape, but now *reported* state from status byte 11, not something we set |
| `room_temperature() -> i16` | status byte 7 × 100; must be **nullable** until the first frame arrives |
| `tick_room() -> bool` | **gone.** Replaced by "a status frame arrived and byte 7 changed" |
| `active_power_mw()` | `600_000` when `heating()`, else 0. Keep `ELEMENT_POWER_MW`, change the value |
| `active_current_ma()`, `energy_mwh()`, `close_period()`, `integrate()`, KV persistence | **keep as is.** They only depend on `active_power_mw()`, which still works |
| `reset_at_boot()`, `last_period()` | unchanged |

New state the type has to hold: the Mill's reported setpoint (byte 6) and reported
mode (byte 9), plus "have we ever heard from the Mill".

### `src/thermostat.rs` - the control loop goes away

- `update_relay()` (`src/thermostat.rs:167-179`) and the `HEATING_RATE` hysteresis
  band are **deleted**. The Mill owns that.
- A write to `OccupiedHeatingSetpoint` must send a `0x46` frame; a write to
  `SystemMode` must send a `0x47` frame.
- Status byte 6 changing *without* a preceding Matter write is a **front-panel
  change**. That is exactly what the `SetpointChangeSource` attributes in the
  `CLUSTER` definition exist for, and it replaces the simulated front panel
  (`FRONT_PANEL_STEP`, `FRONT_PANEL_TICKS`).
- `LocalTemperature` should read **null** until the first status frame, instead of
  the current always-some.
- Setpoint rounding: Matter gives 0.01 °C, the wire takes whole °C. Decide and
  document one rule (round-to-nearest, then echo back what the Mill reports rather
  than what was asked).

### `src/meter.rs` - unchanged

It reads `active_power_mw()` and the energy counters only. Nothing in it knows where
`heating()` comes from.

### New: the UART driver

Needs an `esp-hal` UART on GPIO16/17 at 9600 8N1, and a receive path that fits the
existing single-`embassy`-task, no-alloc design (`heapless`, a fixed frame buffer,
no `Send`). The `run()` loop that today ticks the simulation becomes the loop that
awaits bytes - and, per the project's own rule, **must never return**.

## Open questions

To answer on hardware, before or during the port:

1. **Status frame cadence and total length.** Log raw frames for a few minutes.
   Determines the watchdog timeout for "the Mill has gone quiet" and whether a
   fixed-length reader is safe.
2. **Does the Mill validate the outbound checksum?** Send one frame with a
   deliberately wrong checksum and see whether the command takes effect. This
   settles the 13th-byte question definitively.
3. **What is in the unparsed status bytes** (0-3, 5, 8, 10, 12+)? Candidates worth
   looking for: firmware version, error/fault flags, a child-lock or window-open
   state, the panel's own display state.
4. **Are there other inbound frame types** besides `0xC9`? Log every frame,
   including the ones the ESPHome code drops.
5. **Is there an ack for `0x46`/`0x47`,** or is the next `0xC9` frame the only
   confirmation? Decides whether we need retry logic.
6. **Does the Mill accept setpoints below 5 °C and above 35 °C,** and what does it
   clamp to? Fixes `ABS_MIN/MAX_HEAT_SETPOINT`.
7. **Power-on behaviour.** Does the Mill send status before we send anything, and
   does it come up in the mode it was left in?
8. **The actual plate rating** of the unit being modded, for `ELEMENT_POWER_MW`.
   `600` in the current YAML is a configured value, not a measurement.
9. **The HF-LPT120A header pinout and rail** - pin order, 3.3 V, and whether the
   Mill can supply enough current for the C6 with the Thread radio running.
