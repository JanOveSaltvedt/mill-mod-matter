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

> **Milestone 1: the heater interface is simulated.** Commissioning, Thread, SRP, the
> data model and NVS persistence are all real. `src/heater.rs` fakes the room
> temperature and a 1 kW element; it is the only module that real hardware I/O
> replaces.

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

Once commissioned, the monitor shows the simulation running: `Heater: heating ON/OFF`
as the hysteresis band opens and closes the relay, and a simulated front-panel press
nudging the setpoint once a minute.

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

`active-power` reads `1000000` (mW) while the relay is closed and `0` otherwise.

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

`CLAUDE.md` carries the working notes: the four-repository layout and why the
`[patch.crates-io]` entries exist, where each part of `main.rs` was ported from, and a
list of non-obvious gotchas in this corner of the Matter stack.

## Licence

MIT OR Apache-2.0.
