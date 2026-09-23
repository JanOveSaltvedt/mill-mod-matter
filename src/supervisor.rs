//! The chip's own reset machinery: why it last reset, the brownout detector, and a
//! watchdog.
//!
//! `esp_hal::init` turns every watchdog off, and the ESP-IDF second-stage bootloader
//! arms only the C6's *analog* brownout reset ("BOD mode 1"). That one performs a
//! chip reset, which the reset-reason register records as `0x01` - the very code a
//! power-on leaves. So before this module a brownout rebooted the board and was then
//! indistinguishable from somebody cutting its power, and a stalled executor or a
//! panic (which `esp-backtrace` ends in `loop {}`) hung it for good.
//!
//! Now:
//!
//! - [`arm_brownout_reset`] configures the digital brownout detector ("BOD mode 0")
//!   the way ESP-IDF's own `esp_brownout_init` does, with a *system* reset, which is
//!   recorded as [`SocResetReason::SysBrownOut`] and can be told apart.
//! - [`Watchdog`] runs TIMG1's main watchdog, fed from the one executor task. A
//!   stall or a panic halt now ends in a reset recorded as
//!   [`SocResetReason::CoreMwdt1`].
//! - [`boot_reason`] turns what the last reset left behind into the General
//!   Diagnostics `BootReasonEnum`, which `boot_diag.rs` serves over Matter.
//!
//! Like `heater.rs` for the Mill's UART, this is the only module that touches these
//! peripherals.

use core::convert::Infallible;

use esp_hal::peripherals::{I2C_ANA_MST, LP_ANA, MODEM_LPCON, TIMG1};
use esp_hal::rtc_cntl::SocResetReason;
use esp_hal::timer::timg::{MwdtStage, TimerGroup, Wdt};

use rs_matter_embassy::matter::dm::clusters::gen_diag::BootReasonEnum;

/// The brownout threshold, as ESP-IDF's `CONFIG_ESP_BROWNOUT_DET_LVL`.
///
/// 7 is ESP-IDF's default for the C6, and the *lowest* voltage on offer: its
/// Kconfig estimates 7 → 2.51 V, 6 → 2.64 V, 5 → 2.76 V, 4 → 2.92 V, 3 → 3.10 V,
/// 2 → 3.27 V. Lower numbers mean a higher threshold. A 3.3 V rail with any ripple
/// at all would sit on level 2 permanently and the board would never finish booting,
/// so do not raise this without a scope on the rail.
const BROWNOUT_LEVEL: u8 = 7;

/// How long the executor may go without feeding the watchdog before it resets the
/// board.
///
/// Far longer than anything this firmware legitimately blocks for. The longest are
/// flash operations (a KV write is an `embassy_futures::block_on` with interrupts
/// off, see CLAUDE.md, and a factory reset erases the whole 64 KiB `nvs` range:
/// sixteen sector erases of at most a few hundred ms each) and the PASE/CASE crypto
/// during commissioning. A stall is not subtle, so there is no need to cut this
/// fine.
const WATCHDOG_TIMEOUT_SECS: u64 = 20;

/// How often [`Watchdog::run`] feeds it. Anything well under the timeout will do.
const WATCHDOG_FEED: embassy_time::Duration = embassy_time::Duration::from_secs(1);

/// What the last reset left in the reset-reason register, or `None` for a code
/// `esp-hal` does not know.
pub fn reset_cause() -> Option<SocResetReason> {
    esp_hal::system::reset_reason()
}

/// The raw reset-reason code, `0` for none `esp-hal` knows.
///
/// Served beside [`boot_reason`] because the Matter enum is coarser than the chip:
/// it has no way to say "USB-JTAG reset" (which is what `espflash` does after
/// flashing), and it folds the two watchdogs and every software reset together.
pub fn reset_cause_code(cause: Option<SocResetReason>) -> u8 {
    cause.map_or(0, |cause| cause as u8)
}

/// The General Diagnostics `BootReasonEnum` for a reset cause.
///
/// **`PowerOnReboot` is not proof of a power cycle.** The bootloader's analog
/// brownout reset (BOD mode 1) leaves the same code as a power-on, so a brownout
/// deep enough, or fast enough, to trip it before [`arm_brownout_reset`]'s detector
/// does still reads as `PowerOnReboot`. So does the first boot after this firmware
/// is flashed over one that did not arm the digital detector.
///
/// Every watchdog the chip has is a hardware timer, so they all map to
/// `HardwareWatchdogReset`; `SoftwareWatchdogReset` is for a watchdog implemented
/// in software, which this firmware does not have.
pub fn boot_reason(cause: Option<SocResetReason>) -> BootReasonEnum {
    match cause {
        Some(SocResetReason::ChipPowerOn) => BootReasonEnum::PowerOnReboot,
        Some(SocResetReason::SysBrownOut) => BootReasonEnum::BrownOutReset,
        Some(
            SocResetReason::CoreMwdt0
            | SocResetReason::CoreMwdt1
            | SocResetReason::Cpu0Mwdt0
            | SocResetReason::Cpu0Mwdt1
            | SocResetReason::CoreRtcWdt
            | SocResetReason::Cpu0RtcWdt
            | SocResetReason::SysRtcWdt
            | SocResetReason::SysSuperWdt,
        ) => BootReasonEnum::HardwareWatchdogReset,
        Some(SocResetReason::CoreSw | SocResetReason::Cpu0Sw) => BootReasonEnum::SoftwareReset,
        // USB-JTAG/USB-UART resets are `espflash` (or a monitor) resetting the board
        // over the console; the rest are rare enough to leave to `ResetCause`.
        _ => BootReasonEnum::Unspecified,
    }
}

/// Arm the digital brownout detector (BOD mode 0) to reset the whole digital system,
/// RTC included, when the supply drops below [`BROWNOUT_LEVEL`].
///
/// A transcription of ESP-IDF v5.5's `brownout_hal_config` for the C6 with the
/// values `esp_brownout_init` passes when `CONFIG_ESP_BROWNOUT_USE_INTR` is off.
/// That includes turning the bootloader's analog reset (BOD mode 1) *off*. ESP-IDF
/// does this because mode 1 "always has the highest priority". Left on, it could
/// win the race and record a brownout as a power-on, which is the problem this
/// module exists to fix. The detector also powers the RF down and suspends flash
/// on detection, as ESP-IDF's does, so a sagging rail is not also driving the
/// radio's PA while it waits `reset_wait` cycles to reset.
///
/// Call once, early, before the radio is started: the threshold is written over the
/// analog `regi2c` bus, which the PHY blob also uses, and nothing else is running
/// yet to contend for it.
pub fn arm_brownout_reset() {
    let ana = LP_ANA::regs();

    // `brownout_ll_ana_reset_enable(false)`: take mode 1 away from the fuse-driven
    // default (bit 1 of `FIB_ENABLE`) and turn its reset off.
    ana.fib_enable()
        .modify(|r, w| unsafe { w.bits(r.bits() & !(1 << 1)) });
    ana.bod_mode1_cntl()
        .modify(|_, w| w.bod_mode1_reset_ena().clear_bit());

    ana.bod_mode0_cntl().modify(|_, w| unsafe {
        w.bod_mode0_intr_wait().bits(2);
        w.bod_mode0_close_flash_ena().set_bit();
        w.bod_mode0_pd_rf_ena().set_bit();
        w.bod_mode0_reset_wait().bits(0x3ff);
        w.bod_mode0_reset_ena().set_bit();
        // `BROWNOUT_RESET_LEVEL_SYSTEM`; clear would be a chip reset, and a chip
        // reset records `0x01` - a power-on - again.
        w.bod_mode0_reset_sel().set_bit()
    });

    ana.bod_mode0_cntl()
        .modify(|_, w| w.bod_mode0_cnt_clr().set_bit());
    ana.bod_mode0_cntl()
        .modify(|_, w| w.bod_mode0_cnt_clr().clear_bit());

    regi2c::set_bod_threshold(BROWNOUT_LEVEL);

    // Named `intr_ena`, but it is the detector's enable: ESP-IDF's
    // `brownout_ll_bod_enable` sets exactly this bit.
    ana.bod_mode0_cntl()
        .modify(|_, w| w.bod_mode0_intr_ena().set_bit());
}

/// TIMG1's main watchdog, resetting the system if it goes [`WATCHDOG_TIMEOUT_SECS`]
/// without being fed.
///
/// TIMG1 rather than TIMG0 because `esp-rtos` owns TIMG0's timer, and leaving the
/// whole of TIMG0 to it keeps the two from sharing a peripheral.
pub struct Watchdog {
    wdt: Wdt<TIMG1<'static>>,
}

impl Watchdog {
    /// Start the watchdog. From here on the board resets unless [`Self::feed`] or
    /// [`Self::run`] keeps it fed.
    pub fn start(timg1: TIMG1<'static>) -> Self {
        let mut wdt = TimerGroup::new(timg1).wdt;

        wdt.set_timeout(
            MwdtStage::Stage0,
            esp_hal::time::Duration::from_secs(WATCHDOG_TIMEOUT_SECS),
        );
        // Stage 0 resets the system; stages 1-3 are off.
        wdt.enable();

        Self { wdt }
    }

    pub fn feed(&mut self) {
        self.wdt.feed();
    }

    /// Feed the watchdog forever.
    ///
    /// Run it in the same `select` as the Matter stack, on the one executor task.
    /// That is what makes it a *stall* detector: a future that blocks the CPU, or a
    /// panic that halts it, stops this being polled too.
    ///
    /// It does not notice the stack being *live but stuck*, for example awaiting a
    /// radio event that never comes. Only something that knows what progress looks
    /// like could do that.
    pub async fn run(&mut self) -> Infallible {
        loop {
            self.feed();

            embassy_time::Timer::after(WATCHDOG_FEED).await;
        }
    }
}

/// Just enough of the C6's internal analog I2C bus to write the brownout threshold.
///
/// `esp-hal` has this as `soc::regi2c`, but `pub(crate)`. This is its
/// `regi2c_read`/`regi2c_write` at the pinned revision, cut down to the one block
/// used. The threshold shares its register with another field, hence
/// read-modify-write.
mod regi2c {
    use super::{I2C_ANA_MST, MODEM_LPCON};

    /// `REGI2C_ULP_CAL`, the block `I2C_BOD` lives in (ESP-IDF `regi2c_brownout.h`).
    const BLOCK: u8 = 0x61;
    /// `I2C_ULP_IR_FORCE` / `I2C_BOD_THRESHOLD`.
    const REG: u8 = 0x05;
    /// `I2C_BOD_THRESHOLD`, bits 2..0.
    const THRESHOLD_MASK: u8 = 0b111;

    pub fn set_bod_threshold(level: u8) {
        let master = select_block();
        let value = (read(master) & !THRESHOLD_MASK) | (level & THRESHOLD_MASK);

        write(master, value);
    }

    /// Clock the analog I2C master and point it at [`BLOCK`], returning which of
    /// its two controllers serves it.
    fn select_block() -> usize {
        MODEM_LPCON::regs()
            .clk_conf()
            .modify(|_, w| w.clk_i2c_mst_en().set_bit());
        MODEM_LPCON::regs()
            .i2c_mst_clk_conf()
            .modify(|_, w| w.clk_i2c_mst_sel_160m().set_bit());

        let selected = I2C_ANA_MST::regs()
            .ana_conf2()
            .read()
            .ulp_cal_mst_sel()
            .bit_is_set();

        I2C_ANA_MST::regs().ana_conf1().write(|w| unsafe {
            w.bits(0x00FF_FFFF);
            w.ulp_cal_rd().clear_bit()
        });

        if selected {
            0
        } else {
            1
        }
    }

    fn read(master: usize) -> u8 {
        let ctrl = I2C_ANA_MST::regs().i2c_ctrl(master);

        while ctrl.read().busy().bit() {}

        ctrl.write(|w| unsafe {
            w.slave_addr().bits(BLOCK);
            w.slave_reg_addr().bits(REG)
        });

        while ctrl.read().busy().bit() {}

        ctrl.read().data().bits()
    }

    fn write(master: usize, value: u8) {
        let ctrl = I2C_ANA_MST::regs().i2c_ctrl(master);

        ctrl.write(|w| unsafe {
            w.slave_addr().bits(BLOCK);
            w.slave_reg_addr().bits(REG);
            w.read_write().set_bit();
            w.data().bits(value)
        });

        while ctrl.read().busy().bit() {}
    }
}
