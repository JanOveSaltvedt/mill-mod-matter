//! Logs when the BLE controller and the Thread radio come up and go down.
//!
//! Commissioning is non-concurrent (`stack.run`), so the two should never overlap:
//! `rs-matter-stack` runs BLE only while a basic commissioning window is open, then
//! Thread, and a commissioned board that boots with no window open never starts BLE
//! at all. The controller lives only inside `EspThreadDriver`'s `BleDriver::run`,
//! as a local `BleConnector` whose `Drop` runs esp-radio's `ble_deinit`: controller
//! disabled, every BLE stack disabled and deinitialised, HCI detached. That drop has
//! happened by the time `BleDriver::run` returns, so logging after the return is
//! logging after the teardown.
//!
//! A leftover BLE controller would take radio time from 802.15.4, and the build has
//! no esp-radio `coex` feature to schedule the two. [`LoggedRadio`] makes that
//! checkable from the serial log: every Thread start says whether BLE is down, and
//! warns if it is not.
//!
//! Not undone by `ble_deinit`, and not checked here: the BT clock gates
//! (`clk_bt_en` and friends in `MODEM_SYSCON`) stay on. Those only cost power, and
//! the front-end clocks among them are shared with 802.15.4 anyway.

use portable_atomic::{AtomicU64, Ordering};

use log::{info, warn};

use rs_matter_embassy::ble::Controller;
use rs_matter_embassy::matter::error::Error;
use rs_matter_embassy::wireless::{BleDriver, BleDriverTask, ThreadDriver, ThreadDriverTask};

/// Sentinel for [`BLE_UP_SINCE_MS`] while the controller is down.
const BLE_DOWN: u64 = u64::MAX;

/// Uptime (ms) at which the BLE controller came up, or [`BLE_DOWN`].
static BLE_UP_SINCE_MS: AtomicU64 = AtomicU64::new(BLE_DOWN);

/// Uptime (ms) at which the BLE controller was last torn down, or [`BLE_DOWN`] if it
/// has not been started this boot.
static BLE_DOWN_AT_MS: AtomicU64 = AtomicU64::new(BLE_DOWN);

fn uptime_ms() -> u64 {
    embassy_time::Instant::now().as_millis()
}

/// A Thread + BLE driver that logs each radio's lifetime and delegates everything
/// else to the one it wraps.
pub struct LoggedRadio<T>(pub T);

impl<T> ThreadDriver for LoggedRadio<T>
where
    T: ThreadDriver,
{
    async fn run<A>(&mut self, task: A) -> Result<(), Error>
    where
        A: ThreadDriverTask,
    {
        let up_since = BLE_UP_SINCE_MS.load(Ordering::Relaxed);
        let down_at = BLE_DOWN_AT_MS.load(Ordering::Relaxed);

        if up_since != BLE_DOWN {
            warn!(
                "Radio: Thread starting while the BLE controller is still up (since {} ms)",
                up_since
            );
        } else if down_at == BLE_DOWN {
            info!("Radio: Thread starting; BLE controller not started this boot");
        } else {
            info!(
                "Radio: Thread starting; BLE controller deinitialised at {} ms",
                down_at
            );
        }

        let result = self.0.run(task).await;

        info!("Radio: Thread stopped at {} ms ({:?})", uptime_ms(), result);

        result
    }
}

impl<T> BleDriver for LoggedRadio<T>
where
    T: BleDriver,
{
    async fn run<A>(&mut self, task: A) -> Result<(), Error>
    where
        A: BleDriverTask,
    {
        let result = self.0.run(LoggedBleTask(task)).await;

        // The inner driver's `BleConnector` has been dropped by now, and with it the
        // controller deinitialised - `ble_deinit` asserts on failure rather than
        // returning, so reaching this line means it succeeded.
        let now = uptime_ms();
        BLE_UP_SINCE_MS.store(BLE_DOWN, Ordering::Relaxed);
        BLE_DOWN_AT_MS.store(now, Ordering::Relaxed);

        info!(
            "Radio: BLE controller deinitialised at {} ms ({:?})",
            now, result
        );

        result
    }
}

/// Wraps the stack's BLE task so the controller's bring-up is logged once the inner
/// driver has actually initialised it, not merely when it was asked to.
struct LoggedBleTask<A>(A);

impl<A> BleDriverTask for LoggedBleTask<A>
where
    A: BleDriverTask,
{
    async fn run<C>(&mut self, controller: C) -> Result<(), Error>
    where
        C: Controller,
    {
        let now = uptime_ms();
        BLE_UP_SINCE_MS.store(now, Ordering::Relaxed);

        info!("Radio: BLE controller initialised at {} ms", now);

        self.0.run(controller).await
    }
}
