# CLAUDE.md

Guidance for working in this repository.

## What this is

Firmware for a **hardware mod to a Mill Gen 2 WiFi panel heater**: its WiFi
controller is replaced with an ESP32-C6 that presents the heater to Matter as a
heating Thermostat over **Thread**, metering its own heating element.

The Mill's own MCU keeps the sensor, the triac and the whole control loop. This board
replaces the WiFi module and speaks that module's 9600-baud UART protocol - it
*receives* status and *sends* setpoint and on/off requests, and drives nothing itself.
`src/mill.rs` is the reference for every byte of that protocol.

`no_std`, no-alloc-except-where-forced, single `embassy` executor task. One binary,
built from `src/main.rs`.

**Target specification version: Matter 1.6**, inherited from `rs-matter`.

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

# Flash + monitor. The `.cargo/config.toml` runner is exactly this, so `cargo run
# --release` works too. The partition table and the flashing baud rate come from
# `espflash.toml` - run espflash from the project root so it finds that file, or
# the board silently gets espflash's own table and a 24 KiB `nvs`.
espflash flash --monitor \
    target/riscv32imac-unknown-none-elf/release/mill-mod-matter

# Only the NVS range, when the store has to go but the app need not be reflashed.
# `erase-parts` is the one subcommand that ignores `espflash.toml`'s partition
# table, so it has to be named here (`espflash.rs`'s `erase_parts` only ever reads
# the flag - it errors out rather than guessing).
espflash erase-parts --partition-table partitions.csv nvs
```

The target (`riscv32imac-unknown-none-elf`) comes from `.cargo/config.toml`; there is
no need to pass `--target`. **Nightly and `rust-src` are load-bearing**, not
incidental: `.cargo/config.toml` sets `build-std = ["core", "alloc", "panic_abort"]`,
so `core` and `alloc` are rebuilt from source with the size-tuning flags.
`rust-toolchain.toml` pins that. Note the C6 is RISC-V, so no `espup`/Xtensa fork is
needed.

A board flashed with the wrong partition table does not fail loudly. `main.rs`'s
`get_persistent_store` reads the `nvs` range out of the *flashed* table at runtime,
and `sequential-storage` is a log whose newest copy of a key wins - so a table with
a smaller `nvs` yields a store that is truncated, not empty. Old blobs low in the
log still load while newer writes above the new end are invisible, and the device
boots "already commissioned" with stale setpoints and no Thread credentials
(`No networks available`). The `Will use NVS partition` line at startup is the
check: it must read `0x9000..0x19000`.

`partitions.csv` is sized from a measurement of the release ELF, not a guess - see
the comment in the file. Re-check it when the image grows:

```sh
readelf -lW target/riscv32imac-unknown-none-elf/release/mill-mod-matter |
    awk '/LOAD/{t+=strtonum($5)} END{printf "%.2f MiB\n", t/1048576}'
```

## Layout, and where the code came from

```
config.toml        per-unit settings: the element's wattage, the setpoint range, the
                   Basic Information strings, the pairing passcode/discriminator.
                   Documented inline; `config.local.toml` (gitignored) overrides it
                   key by key
build.rs           parses and validates those, generates `src/config.rs` into OUT_DIR,
                   and writes the QR/manual pairing codes into `commissioning/`
                   (gitignored) using rs-matter's own `pairing::qr`
src/config.rs      six lines: `include!`s the generated constants
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

None of these is obvious from the code.

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
  which every real controller assigns, so nothing below `size-128` can carry it. We
  ship `events-ringbuf-size-256`; our own energy events are 45-61 bytes and grow with
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
- **`TEST_DEV_DET` is the same device on every board.** It hard-codes
  `serial_no: "123456789"` and inherits an empty `unique_id` from
  `BasicInfoConfig::new()` - and Matter 1.6 makes `UniqueID` mandatory (Basic
  Information cluster revision 4, `BasicInformationCluster.xml`). A controller that
  files devices under the serial number as well as the node ID - Home Assistant
  does - then folds two physically different boards into one record, so a bench
  board and a heater commissioned onto the same fabric collapse into one device
  while the controller still sees two nodes. `dev_det()` in `main.rs` derives both
  strings from the factory MAC instead, and `config.toml` supplies the rest of the
  block. The VID/PID and test DAC are still shared by every board and that is fine -
  neither is what a controller files a device under, and the VID/PID deliberately
  cannot be configured because the test DAC is issued for them. The passcode and
  discriminator are shared only by default: `[commissioning]` exists because two
  boards advertising discriminator 3840 at the same time are genuinely ambiguous to a
  commissioner. Those two are also all it takes to produce the printed QR and manual
  codes without a board, which is what `build.rs` writes into `commissioning/` - how a
  board sealed inside a heater gets commissioned.
- **The device is uncertified.** It ships `rs-matter`'s test DAC/PAI and the CSA test
  VID/PID, so every commissioner warns about it. Expected on a private fabric.

### Configuration

- **The configuration is build-time, and has to be.** `config.toml` is read by
  `build.rs`, never by the firmware: there is no filesystem on the device, and three
  of the values are `const` associated items that could not be runtime values anyway -
  `ThermostatHooks::ABS_MIN/MAX_HEAT_SETPOINT` and the `ACCURACY` consts of both
  metering hooks. Adding a key means touching four places together: the `[section]` in
  `config.toml` (with the prose explaining it - that is where the person changing it
  reads, not the Rust), its field in `build.rs`'s `deny_unknown_fields` struct, a rule
  in `validate`, and the `write!` in `emit`. Do not expose `vid`/`pid`: the test DAC is
  issued for `0xFFF1`/`0x8001` and a `BasicInformation` value that disagrees fails
  attestation outright rather than merely warning.
- **`heater.element_watts` does not invalidate the stored energy counter.** The blob is
  milliwatt-*seconds* of energy already integrated, not accumulated on-time, so a
  corrected plate rating leaves the lifetime total monotone - it just becomes two
  segments at two rates, and needs no migration.
- **`build.rs` builds `rs-matter` a second time, for the host.** That is what the
  `[build-dependencies]` entry is, and it is deliberate: `commissioning/`'s QR payload
  comes out of `rs-matter`'s own `pairing::qr` rather than a second implementation of
  the Core spec's bit packing, so the printed code and the one the board advertises
  cannot drift. `default-features = false` is enough - `pairing::qr` needs no crypto
  backend, no transport and no logging facade - and costs about a minute on a clean
  tree. One field *does* differ: the board folds its MAC-derived `SerialNumber` into
  the payload as optional TLV data and a build cannot know it, so the two `MT:`
  strings differ while both still pair.
- **The onboarding artifacts are written only when their bytes change.** `build.rs`
  declares `rerun-if-changed` on the `commissioning/` *directory*, so deleting an
  artifact brings it back - and cargo reads a directory as the newest mtime anywhere
  under it. Rewriting identical files every build would leave that directory
  permanently newer than the build script's own `output` file, which cargo streams
  while the script runs, and the script would rerun, and recompile the crate, forever.
  `write_artifact` compares first for exactly this reason.
- **The restored setpoint limits are clamped to the absolute range.** Narrowing
  `heater.min/max_setpoint_celsius` on a commissioned device would otherwise leave the
  persisted `Min/MaxHeatSetpointLimit` outside `AbsMin/AbsMaxHeatSetpointLimit`, which
  the cluster forbids. `ThermostatDeviceLogic::new` clamps on load; the stored value is
  not preserved.

### The Mill link

- **`esp-println` must be pinned to `jtag-serial`,** with `default-features = false`.
  The console lives on USB Serial/JTAG and the Mill has UART1 on GPIO16/17 - which are
  UART0's default console pins, so on the default `auto` backend `esp-println` falls
  back to UART0 whenever no USB host is attached and sprays log lines at the heater's
  MCU. The ROM bootloader still talks on UART0 at reset; nothing can be done about
  that and the Mill ignores it.
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
- **`0x5B` is the only frame terminator, and every checksum is verified.** `0x0A`
  looks like a terminator but is an ordinary payload byte: a temperature of exactly
  10 degC. A decoder that breaks on it truncates that frame and then silently serves
  the *previous* frame's readings. Both rules are load-bearing, not taste.

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
