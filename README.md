# mill-mod-matter

Matter-over-Thread thermostat firmware for an **ESP32-C6** retrofitted into a
**Mill Gen 2 WiFi panel heater**, in place of its original WiFi controller.

The heater appears on a Matter fabric as a heating Thermostat that also meters its own
heating element:

| Endpoint | Device types | Clusters |
| --- | --- | --- |
| 0 | Root Node | the Matter system clusters + Thread Diagnostics (supplied by `rs-matter-stack`) |
| 1 | Thermostat (`0x0301`) + Electrical Sensor (`0x0510`) | Descriptor, Identify, Groups, Thermostat (`HEAT`), Power Topology (`NODE`), Electrical Power Measurement (`AC`), Electrical Energy Measurement (`IMPE｜CUME｜PERE`) |

Both device types share endpoint 1: Electrical Sensor is a *utility* device type, and
Core spec 9.2.1 allows any number of those beside the single application one — so the
thermostat can meter itself without a second endpoint.

> **The Mill keeps its own control loop.** Its microcontroller owns the temperature
> sensor, the triac and every decision about when the element is on. This board
> replaces the heater's **WiFi module** and speaks that module's 9600-baud UART
> protocol: it receives status frames and sends setpoint and on/off requests. The
> Matter thermostat is therefore an advisory peer - it reports what the heater is
> doing, including a setpoint somebody changed on the front panel, and asks for
> changes it cannot enforce. `src/mill.rs` documents the protocol byte by byte.

## Wiring

| ESP32-C6 | Mill |
| --- | --- |
| `GPIO16` | RX of the heater's MCU (ESP → Mill) |
| `GPIO17` | TX of the heater's MCU (Mill → ESP) |
| GND | GND |

9600 8N1, no flow control, 3.3 V TTL — the header carried an **HF-LPT120A** WiFi
module. Pin order, the supply rail and whether the Mill can feed a C6 with the
Thread radio running are **not** documented anywhere; confirm them against the
physical board before wiring anything up.

`GPIO16`/`GPIO17` are also the C6's default UART0 console pins, so the Mill is given
UART1 and the log console goes out over USB Serial/JTAG. The ROM bootloader still
says its piece on UART0 at reset, and those bytes do reach the heater's MCU, which
makes nothing of them.

`GPIO9` (Boot Mode) stays free for the factory reset.

## Prerequisites

- An ESP32-C6 board (4 MB flash — see `partitions.csv` for other sizes).
- The three sibling checkouts this crate patches in: `../rs-matter` (on branch
  `feature/thermostat-energy-metering`), `../rs-matter-stack`, `../rs-matter-embassy`.
- A Matter controller **with a Thread border router**: Home Assistant with the Matter
  integration and a border router on the network, or a hub that is both at once - Apple
  TV/HomePod, a screen-equipped Google Nest, Echo Hub, SmartThings hub, IKEA Dirigera.
- `cargo install espflash`

The toolchain (nightly + `rust-src` + the RISC-V target) is pinned by
`rust-toolchain.toml` and installs itself on first build.

## Configuration

Per-unit settings live in **`config.toml`** at the repo root, which documents every key
inline. `build.rs` validates the file and generates `src/config.rs` from it, so a bad
value fails the build with a sentence instead of reaching a board.

It is resolved at build time, not read at boot: there is no filesystem on the device,
and several of the values land in `const` contexts that a runtime value could not
satisfy.

To change something, either edit `config.toml`, or - better, since it keeps your tree
clean - put just the keys you want in **`config.local.toml`**, which is gitignored and
merged over the defaults key by key:

```toml
[heater]
element_watts = 1200
```

Then `cargo build --release` and reflash. Touching either file re-runs the generator.

### The one key to get right

**`heater.element_watts`.** There is no metering hardware in the Mill and the mod did
not add any, so every reading the Electrical Power and Electrical Energy Measurement
clusters serve is this number gated on one bit of the Mill's status frame. Get it wrong
and the power reading is wrong and the lifetime energy total is wrong by the same
factor. Read it off the plate on the back of the heater - Mill ships the same Gen 2
panel from roughly 250 W to 2000 W, and the default here is the 600 W unit this was
developed against.

Changing it later is safe and needs no migration: the persisted counter holds
milliwatt-*seconds* of energy already integrated, not accumulated on-time, so the
lifetime total stays monotone. It simply becomes a sum of two segments computed at two
rates, which is the honest answer for a device whose plate rating was corrected.

### The rest

| Section | Keys |
| --- | --- |
| `[heater]` | `element_watts`, the setpoint range (`min`/`max`/`default_setpoint_celsius`), `metering_accuracy_percent`, `circuit_max_watts` |
| `[device]` | The Matter Basic Information strings: vendor and product name, product label, part number, hardware and software version (and their strings), manufacturing date, the mDNS `device_name`, an optional `serial_number` override, and the `unique_id_prefix` |
| `[commissioning]` | `passcode` and `discriminator`, which are what the printed QR and manual pairing codes encode |

Three notes:

- **Leave `serial_number` empty.** Empty means each board derives its own from the
  chip's factory MAC, which is what you want: a controller that files devices under the
  serial number as well as the node ID - Home Assistant does - folds two boards sharing
  one serial into a single device record, so setting it in a shared config file costs
  you exactly that. The `UniqueID` is always MAC-derived and never follows an explicit
  serial.
- **Give a second board its own `discriminator`.** Two boards advertising 3840 at once
  are genuinely ambiguous to a commissioner. This is the one shared commissioning
  parameter that actually causes trouble.
- **Narrowing the setpoint range** on an already-commissioned device clamps the stored
  `Min`/`MaxHeatSetpointLimit` into the new band on the next boot rather than preserving
  them, since the cluster does not allow them outside `AbsMin`/`AbsMaxHeatSetpointLimit`.

### What is deliberately *not* configurable

| | Why |
| --- | --- |
| `vendor_id` / `product_id` | The test DAC/PAI is issued for the CSA test VID/PID `0xFFF1`/`0x8001`. A `BasicInformation` value disagreeing with the certificate fails device attestation outright rather than merely warning. Configurable only alongside a real DAC. |
| The wire protocol in `src/mill.rs` | Opcodes, byte offsets, command templates and the checksum. A different protocol is a code change; the compile-time wire fixtures in that file exist to make a mismatch a build error. |
| The 9600 baud rate | Fixed by the Mill, not a choice. |
| UART pins and the factory-reset GPIO | Dictated by the HF-LPT120A header and the C6's Boot Mode pin. |
| `BUMP_SIZE`, `HEAP_SIZE` | Tuned; too small panics during stack init. |
| Sampling, persist and watchdog intervals | Internal cadence. Lowering the energy persist interval puts a flash write that stalls the radio back on the sampling path. |
| Voltage, current, frequency, power factor | Not served at all, deliberately. There is nothing to measure them with, and a client cannot tell a derived reading from a measured one. |

## Build and flash

```sh
cargo build --release

espflash flash --monitor \
    target/riscv32imac-unknown-none-elf/release/mill-mod-matter
```

`cargo run --release` does the same, via the runner in `.cargo/config.toml`. The
partition table and the flashing baud rate come from `espflash.toml`, so run espflash
from the project root - a board flashed with espflash's own default table gets a 24 KiB
`nvs` instead of 64 KiB and comes up with a silently truncated store.

## Commissioning

While the device has no fabrics, it opens a commissioning window on boot and prints a
QR code plus a manual pairing code on the console. Scan or type it into your
controller's app. It will warn that the device is uncertified — it ships `rs-matter`'s
test attestation and the CSA test VID/PID, which is expected on a private fabric.

A console is not actually required to get those codes. Nothing in them is generated at
runtime: they are the `[commissioning]` passcode and discriminator from `config.toml`,
plus the fixed test VID/PID, the standard commissioning flow and BLE as the only
discovery capability - which is what the firmware advertises, since it commissions over
BLE before it has joined Thread. So they can be generated on a workstation, with the
payload utilities in `chip-tool` (no device and no fabric involved):

```sh
chip-tool payload generate-qrcode \
    --discriminator 3840 --setup-pin-code 20202021 \
    --vendor-id 65521 --product-id 32769 \
    --version 0 --commissioning-mode 0 --rendezvous 2

chip-tool payload generate-manualcode \
    --discriminator 3840 --setup-pin-code 20202021 \
    --version 0 --commissioning-mode 0
```

`3840` and `20202021` are the defaults; pass whatever `config.toml` says if either was
changed. `65521`/`32769` are `0xFFF1`/`0x8001` in decimal, and the manual code needs
neither — on the standard commissioning flow it is the 11-digit short code, which
carries only the discriminator and the passcode. The QR string matches the board's
`MT:...` exactly; the manual code matches too, the board just groups it `XXXX-XXX-XXXX`
for reading aloud. So a heater that was closed up before it was ever commissioned can
still be paired, from a printout made beforehand.

Once commissioned, the monitor shows the link to the heater: the first few status
frames raw (`Mill: RX 5A ...`, with the gap since the previous one), then
`Mill: element ON/OFF` as the Mill's own control loop works, and
`Thermostat: the heater's setpoint moved to 22C on its own` when somebody turns the
knob on the front panel. Writes go the other way as `Mill: requesting ...`.

If the log instead fills with `Mill: dropping malformed frame`, the raw bytes are in
the warning: either the wiring is wrong or the Mill computes its checksum
differently from `src/mill.rs`. If nothing arrives at all, `LocalTemperature` reads
null and `Mill: no status frame for 120 s` appears once.

## Home Assistant

Add it from **Settings → Devices & services → Add integration → Matter**, then scan the
QR code or type the manual pairing code. Home Assistant needs a Thread border router it
can reach; the device itself needs nothing further.

What shows up:

- A **climate entity** in `heat` mode, carrying the heating setpoint and the room
  temperature the Mill reports. Changing the setpoint there sends it to the Mill, which
  is free to round it to a whole degree or ignore it outright; the entity settles on
  whatever the next status frame says. A setpoint changed on the heater's own front
  panel arrives by the same path and shows up unprompted.
- **Power and energy sensors** from the Electrical Power and Electrical Energy
  Measurement clusters. The lifetime energy counter is persisted in flash and is
  monotone across reboots and firmware updates, which is what the Energy dashboard
  wants from an individual device.

Two things worth knowing about those readings. Active power is the element's plate
rating (`heater.element_watts`) gated on one bit of the Mill's status frame — full
rating while the element is on, zero while it is off. There is no metering hardware in
the heater and the mod did not add any, so that number is worth checking against the
plate on the unit being modded.

For the same reason it is the *only* power reading served: no voltage, current,
frequency, power factor or RMS, apparent and reactive quantities. Those are all
optional in the cluster, and serving them would mean deriving them from a nominal
230 V / 50 Hz supply that this board never measures — a client has no way to tell
such a reading from a measured one.

## Persistence and factory reset

State lives in the `nvs` flash partition: the Matter fabrics and Thread credentials,
OpenThread's SRP key, and this firmware's own two blobs — the thermostat's four
non-volatile attributes and the element's lifetime energy counter. Power-cycling the
board keeps all of it, including the commissioning.

To factory-reset, hold the **Boot Mode pin (GPIO9)** low for 3 seconds. The device
wipes both halves of the persisted state, reboots, and comes back advertising for
commissioning with default setpoints and a zeroed energy counter. Remove the device
from your controller as well, or it will keep trying to reach the old fabric.

## Documentation

`src/mill.rs` is the protocol reference: the framing, the status frame's offsets and
the two command frames, with worked examples asserted at compile time.

`CLAUDE.md` carries the working notes: the four-repository layout and why the
`[patch.crates-io]` entries exist, where each part of `main.rs` was ported from, and a
list of non-obvious gotchas in this corner of the Matter stack.

## Licence

MIT OR Apache-2.0.
