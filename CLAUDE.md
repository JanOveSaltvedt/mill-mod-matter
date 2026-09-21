# CLAUDE.md

Guidance for working in this repository.

## What this is

Firmware for a **hardware mod to a Mill Gen 2 WiFi panel heater**: its WiFi
controller is replaced with an ESP32-C6 that presents the heater to Matter as a
heating Thermostat over **Thread**, metering its own heating element.

The mod does **not** drive a relay and does **not** read a thermistor. The Mill's
own MCU keeps the sensor, the triac and the whole control loop; this board replaces
the WiFi module and speaks that module's 9600-baud UART protocol - it *receives*
status and *sends* setpoint and on/off requests. `MILL-HARDWARE-INTERFACE.md` is the
reference for every byte of it.

`no_std`, no-alloc-except-where-forced, single `embassy` executor task. One binary,
built from `src/main.rs`.

**Target specification version: Matter 1.6**, inherited from `rs-matter`.

### Milestone status

| | |
| --- | --- |
| Milestone 1 (**done**) | Real BLE commissioning, Thread join, SRP, data model and NVS persistence, against a simulated heater. |
| Milestone 2 (**written, not yet run on hardware**) | The real Mill UART: `src/mill.rs` (wire protocol) and `src/heater.rs` (UART + reported state) replaced the simulation, and `src/thermostat.rs` lost its control loop. |
| Next | Bench bring-up. The nine open questions at the end of `MILL-HARDWARE-INTERFACE.md` are all answered by logs from a wired unit; the first frames are logged raw at `info` on purpose. |

## The four repositories

This crate only builds inside a checkout that has its three siblings next to it:

```
workspace/iot/
  mill-mod-matter/    <- here
  rs-matter/          FORK, branch `feature/thermostat-energy-metering`
  rs-matter-stack/    unmodified upstream master
  rs-matter-embassy/  unmodified upstream master
  connectedhomeip/    the CSA SDK: chip-tool, the 1.6 data-model XML
```

`rs-matter` is **our fork**, one commit ahead of upstream, and that commit is what
adds the Thermostat, Electrical Power/Energy Measurement and Power Topology cluster
handlers this firmware is built on. Neither `rs-matter-stack` nor `rs-matter-embassy`
knows about the fork - they both pull `rs-matter` from crates.io - which is the whole
reason `Cargo.toml` carries:

```toml
[patch.crates-io]
rs-matter       = { path = "../rs-matter/rs-matter" }
rs-matter-stack = { path = "../rs-matter-stack" }
```

**`../rs-matter/CLAUDE.md` is the authority for anything cluster- or spec-related**:
how to look up conformance in `../connectedhomeip/data_model/1.6/clusters/*.xml`, the
`ClusterHandler` / `<Cluster>Hooks` patterns, and the codegen rules. Read it before
touching a `*Hooks` impl. `../rs-matter/WORK-REMAINING.md` records what is
deliberately *not* implemented in the thermostat clusters (exported energy, presets
and schedules, EPM `Ranges`, Power Topology beyond `NODE`) so nobody re-derives it.

The `esp-*` crates are also patched, to the single git revision
`rs-matter-embassy/examples/esp/Cargo.toml` pins. They must all come from the same
revision or the peripheral singletons stop matching.

## Build, flash, monitor

```sh
cargo build --release
cargo clippy --no-deps --release --all-targets -- -Dwarnings
cargo fmt -- --check

# One-off
cargo install espflash

# Flash + monitor. The `.cargo/config.toml` runner does the same, so `cargo run
# --release` works too.
espflash flash --monitor --partition-table partitions.csv --baud 1500000 \
    target/riscv32imac-unknown-none-elf/release/mill-mod-matter
```

The target (`riscv32imac-unknown-none-elf`) comes from `.cargo/config.toml`; there is
no need to pass `--target`. **Nightly and `rust-src` are load-bearing**, not
incidental: `.cargo/config.toml` sets `build-std = ["core", "alloc", "panic_abort"]`,
so `core` and `alloc` are rebuilt from source with the size-tuning flags.
`rust-toolchain.toml` pins that. Note the C6 is RISC-V, so no `espup`/Xtensa fork is
needed.

`partitions.csv` is sized from a measurement of the release ELF, not a guess - see
the comment in the file. Re-check it when the image grows:

```sh
readelf -lW target/riscv32imac-unknown-none-elf/release/mill-mod-matter |
    awk '/LOAD/{t+=strtonum($5)} END{printf "%.2f MiB\n", t/1048576}'
```

## Layout, and where the code came from

```
src/main.rs        stack wiring, the `NODE` metadata, the handler chain, NVS store,
                   factory reset, the Mill UART's construction
src/mill.rs        the wire protocol: framing, checksum, the status frame, the two
                   command frames. Pure logic, no peripherals.
src/heater.rs      MillHeater: the UART halves, the state the Mill reports, the energy
                   counters. The ONLY module that touches hardware.
src/thermostat.rs  ThermostatHooks - heating-only, setpoints persisted, no control
                   loop (the Mill owns that)
src/meter.rs       ElecPwrMeasHooks + ElecEnergyMeasHooks - what the element draws
src/vendor_kv.rs   object-safe view of the Matter KV store + our key constants
```

`main.rs` is a port of two `rs-matter-embassy` examples; diff against them when
upstream moves:

- `../rs-matter-embassy/examples/esp/src/bin/light_thread.rs` — the stack wiring,
  heap/bump sizing, crypto setup and handler-chain idiom.
- `../rs-matter-embassy/examples/esp/src/bin/light_wifi_persistent.rs` — the NVS
  store (`get_persistent_store`) and the factory-reset-on-GPIO9 path.

The cluster logic is a port of `../rs-matter/examples/src/bin/thermostat.rs`, with the
KV-backed persistence shape taken from `../rs-matter/tests/src/bin/thermostat_tests.rs`
and `../rs-matter/tests/src/common/vendor_kv.rs`.

Re-exports worth knowing, so you never depend on `rs-matter` directly:
`rs_matter_embassy::matter::*` **is** `rs-matter`, `rs_matter_embassy::stack::*` **is**
`rs-matter-stack`, `rs_matter_embassy::ot::openthread::*` **is** `openthread`.

## Gotchas

Each of these cost real investigation. None is obvious from the code.

- **Events are off by default.** `events-ringbuf-size-0` is the default in
  `rs-matter-stack` *and* `rs-matter-embassy`. Without an `events-ringbuf-size-*`
  feature in `Cargo.toml`, Electrical Energy Measurement's
  `CumulativeEnergyMeasured` / `PeriodicEnergyMeasured` are silently never emitted -
  no error, no warning, subscriptions just stay quiet.
- **The event ring size is a per-event ceiling, not a queue depth.** Each of the
  three priority rings is `N` bytes and an event that does not fit in an *empty*
  ring fails the emit with `ResourceExhausted` (`EventWriter::write`,
  `../rs-matter/rs-matter/src/im/events.rs`). `AddNOC` propagates that, so
  commissioning dies right after `Added operational fabric with local index 1`
  with nothing but `Error invoking command: ResourceExhausted`. The culprit is the
  `AccessControlEntryChanged` event rs-matter emits for the admin ACL entry
  `AddNOC` seeds: 65-67 bytes once the operational node ID is a random 64-bit one,
  which every real controller assigns. `size-64` only ever worked against
  chip-tool, whose fixed node ID 112233 encodes in three bytes. We ship
  `events-ringbuf-size-256`; our own energy events are 45-61 bytes and grow with
  the running totals and uptime.
- **`VENDOR_KEYS_START` is already taken.** On a Thread device `rs-matter-embassy`
  keeps OpenThread's SRP ECDSA key at exactly `VENDOR_KEYS_START`
  (`OT_SRP_ECDSA_KEY`, `../rs-matter-embassy/rs-matter-embassy/src/ot.rs`). Ours start
  at `+1`. Note this differs from the `rs-matter` test drivers, which start at `+0`
  because they are not Thread devices - do not copy their key numbers.
- **Factory reset does not clear our state for us.** `Matter::factory_reset` only
  removes rs-matter's own keys (they grow *downwards* from `VENDOR_KEYS_START`), and
  the `FactoryReset` lifecycle op does not reach the hooks - `ThermostatHooks` and
  `ElecEnergyMeasHooks` have no lifecycle method. `main.rs` removes the two vendor
  blobs explicitly; see the comment there before adding a third.
- **A KV write blocks the entire stack.** `SeqMapKvBlobStore` is an
  `embassy_futures::block_on` around `sequential-storage`, over an `esp-storage`
  `FlashStorage` that takes a critical section - cache off, interrupts off - for
  every flash operation under it. There is one executor task, so each
  `store_blob` stops the OpenThread radio future, its alarms and the Mill UART for
  as long as the write runs. Persist on an event or a slow timer, never on a
  per-sample tick: `heater.rs`'s `persist_energy` is the pattern and
  `ENERGY_PERSIST_INTERVAL` the ceiling.
- **`stack.reset` takes `&mut *stack`**, so it cannot be called while a `kv` from
  `stack.matter().kv(..)` is alive - and our EP1 handlers borrow that `kv`. Hence the
  scope in `main` and the separate, root-only handler built for the reset call.
- **A hooks `run()` must never return.** `ChainedHandler::run` selects over every
  handler's `run`, so one returning ends the whole chain; the SDK panics deliberately
  rather than silently going deaf. Always `loop {}`.
- **No `UserTask` is needed** for the cluster loops. The Interaction Model - which
  `stack.run` owns - polls `AsyncHandler::run` on every chained handler, and each
  cluster handler drives its hooks' `run()` from there. `()` is the right last
  argument to `run`.
- **`root_handler`, never `*SysHandlerBuilder`.** The stack chains the operational
  network clusters (Network/General Commissioning, General and Thread Diagnostics) on
  top of our handler itself, because only it knows the radio state. Using
  `ThreadSysHandlerBuilder` as `../rs-matter/examples/src/bin/thermostat.rs` uses the
  Eth one would chain them twice.
- **`impl ElecEnergyMeasHooks for &T` does not forward `cumulative_energy_reset`.**
  Pass the device logic *by value* into `ElecEnergyMeasHandler::new`, never by
  reference, or that attribute silently reads null.
- **Matcher closures must be non-capturing** — `FnMatcher = fn(EndptId, ClusterId)
  -> bool`. A capturing closure needs `ChainedHandler::new_with_matcher` and makes the
  chain type unnameable.
- **`#![recursion_limit = "256"]`** is required by the handler-chain types.
- **`BUMP_SIZE` and `HEAP_SIZE` are tuned, not arbitrary.** Too small a bump panics
  during stack init. 25000 is for non-concurrent commissioning (`stack.run`); the
  concurrent path (`stack.run_coex`) uses 20000.
- **The whole stack is one non-`Send` future.** Local executors only.
- **Commissioning is non-concurrent** (`stack.run`): BLE first, then Thread. Some
  controllers - `rs-matter-embassy`'s README names Alexa - want the concurrent path.
  Switching is a two-line change; `light_thread_coex.rs` is the reference.
- **The device is uncertified.** It ships `rs-matter`'s test DAC/PAI and the CSA test
  VID/PID, so every commissioner warns about it. Expected on a private fabric.

### The Mill link

- **GPIO16/17 are UART0's default console pins.** The Mill takes them, so the
  console is pinned to USB Serial/JTAG (`esp-println`'s `jtag-serial` feature,
  `default-features = false`) and the Mill gets UART1. Leaving `esp-println` on its
  default `auto` would have it fall back to UART0 whenever no USB host is attached
  and spray log lines at the heater's MCU. The ROM bootloader still talks on UART0 at
  reset; nothing can be done about that and the Mill ignores it.
- **The lifetime on `MillHeater`'s UART halves is `'static`, deliberately.** They have
  destructors, so a borrowed lifetime would have to outlive the heater's own drop -
  and `ThermostatDeviceLogic` holds `&'a MillHeater<'a>`, which *is* that lifetime.
  Borrow-check fails with a dropck error that does not name the real cause.
- **The RX half is an `embassy_sync` `Mutex`, not a `RefCell`**, because it is held
  across an await (`clippy::await_holding_refcell_ref`).
- **`ThermostatHooks::apply` fires once at startup**, from the handler's `repair()`,
  before anything can have been written and before the Mill has said a word. That
  call must *not* push the restored state onto the heater - somebody may have turned
  the knob while the board was rebooting - so `ThermostatDeviceLogic` skips the
  first `apply` and adopts the Mill's state from the first status frame instead.
- **Commands are fire-and-forget.** No ack, no sequence number. A command is
  confirmed only by a later status frame echoing it, which is what the
  `pending_setpoint`/`pending_mode` grace window in `thermostat.rs` is for: a status
  frame that disagrees with a command sent moments ago has probably just crossed it
  on the wire.
- **Every status frame feeds the silence watchdog**, including one identical to the
  last. `MillHeater::recv_status` therefore resolves on *any* status frame, not only
  on one that changed something - a steady-state heater repeating itself must not look
  like a heater that has stopped talking.
- **ESPHome's implementation has real defects** - it breaks frames on `0x0A` (which
  is a room temperature of exactly 10 degC), never verifies a received checksum, and
  overruns its command buffer by one byte. `MILL-HARDWARE-INTERFACE.md` lists them
  under "Known weaknesses"; none is reproduced here. Do not "fix" `mill.rs` to match
  the C++.

## Conventions

- **No allocation, no large stack values.** The heap exists only for OpenThread /
  mbedTLS and `x509`. Use `heapless`, and `Init` / `init!` (`matter::utils::init`) for
  anything big - constructing a large struct by value will blow the MCU stack. The
  Matter stack itself is `StaticCell`-allocated for exactly this reason.
- **Sizing limits are Cargo features, not constants** (fabrics, sessions, ACLs,
  subscriptions, KV scratch buffer). Don't raise a number in code; pick the feature.
- Logging is plain `log::{info, warn, error}` - this is a downstream crate, so
  `rs-matter`'s internal `crate::fmt` rule does not apply.
- Keep `heater.rs` the only module that touches a peripheral. If a change to
  `thermostat.rs` or `meter.rs` starts wanting one, put it behind a `heater.rs`
  method instead. Protocol logic belongs in `mill.rs`, which stays free of
  `esp-hal` so it can be reasoned about (and `const`-asserted) on its own.
