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

> **The mod is not a relay driver.** The Mill's own microcontroller keeps the
> temperature sensor, the triac and the whole control loop. This board replaces the
> heater's **WiFi module** and speaks that module's 9600-baud UART protocol: it
> receives status frames and sends setpoint and on/off requests. So the thermostat
> is an advisory peer - it reports what the heater is doing, including a setpoint
> somebody changed on the front panel, and asks for changes it cannot enforce.
> `MILL-HARDWARE-INTERFACE.md` documents the protocol, byte by byte.

> **Not yet run against real hardware.** The UART half is written and builds, but
> every open question in `MILL-HARDWARE-INTERFACE.md` - the status cadence, the true
> frame length, whether the Mill validates the checksum it is sent - is still open.
> The first eight frames are logged raw, at `info`, for exactly that reason.

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

`GPIO16`/`GPIO17` are also the C6's default UART0 console pins, so the Mill gets
UART1 and the log console is pinned to USB Serial/JTAG. `espflash --monitor` already
talks over USB, so this costs nothing — but the monitor and the Mill cannot share
pins, and the ROM bootloader's own chatter at reset does go out to the heater.

`GPIO9` (Boot Mode) stays free for the factory reset.

## Prerequisites

- An ESP32-C6 board (4 MB flash — see `partitions.csv` for other sizes).
- The three sibling checkouts this crate patches in: `../rs-matter` (on branch
  `feature/thermostat-energy-metering`), `../rs-matter-stack`, `../rs-matter-embassy`.
- A Matter controller **with a Thread border router**: Apple TV/HomePod, a
  screen-equipped Google Nest, Echo Hub, SmartThings hub, or IKEA Dirigera. For
  command-line work, `chip-tool` from `../connectedhomeip` plus a separate border
  router.
- `cargo install espflash`

The toolchain (nightly + `rust-src` + the RISC-V target) is pinned by
`rust-toolchain.toml` and installs itself on first build.

## Build and flash

```sh
cargo build --release

espflash flash --monitor --partition-table partitions.csv --baud 1500000 \
    target/riscv32imac-unknown-none-elf/release/mill-mod-matter
```

`cargo run --release` does the same, via the runner in `.cargo/config.toml`.

## Commissioning

While the device has no fabrics, it opens a commissioning window on boot and prints
a QR code plus a manual pairing code. Scan it from your controller's phone app. It will warn that the device is uncertified — it ships
`rs-matter`'s test attestation and the CSA test VID/PID, which is expected on a
private fabric.

Once commissioned, the monitor shows the link to the heater: the first few status
frames raw (`Mill: RX 5A ...`, with the gap since the previous one), then
`Mill: element ON/OFF` as the Mill's own control loop works, and
`Thermostat: the heater's setpoint moved to 22C on its own` when somebody turns the
knob on the front panel. Writes go the other way as `Mill: requesting ...`.

If the log instead fills with `Mill: dropping malformed frame`, the raw bytes are in
the warning: either the wiring is wrong or the Mill computes its checksum
differently from `src/mill.rs`. If nothing arrives at all, `LocalTemperature` reads
null and `Mill: no status frame for 120 s` appears once.

## Poking at it with chip-tool

```sh
chip-tool thermostat read occupied-heating-setpoint <node-id> 1
chip-tool thermostat write system-mode 4 <node-id> 1          # 4 = Heat
chip-tool thermostat subscribe local-temperature 1 10 <node-id> 1
chip-tool thermostat setpoint-raise-lower 0 10 <node-id> 1    # +5.0 C

chip-tool electricalpowermeasurement read active-power <node-id> 1
chip-tool electricalenergymeasurement read cumulative-energy-imported <node-id> 1
chip-tool electricalenergymeasurement subscribe-event cumulative-energy-measured 1 10 <node-id> 1
```

`active-power` reads `600000` (mW) while the Mill has the element on and `0`
otherwise — the element's plate rating gated on one bit of the status frame. There
is no metering hardware in the heater and none was added, so that rating is worth
confirming against the unit being modded.

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

`MILL-HARDWARE-INTERFACE.md` is the protocol reference: the framing, the status
frame's offsets, the two command frames with worked examples, the defects of the
ESPHome implementation it was reverse-engineered from, and the questions still to be
answered on hardware.

`CLAUDE.md` carries the working notes: the four-repository layout and why the
`[patch.crates-io]` entries exist, where each part of `main.rs` was ported from, and a
list of non-obvious gotchas in this corner of the Matter stack.

## Licence

MIT OR Apache-2.0.
