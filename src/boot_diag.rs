//! Why the board last reset, over Matter.
//!
//! The standard home for this is General Diagnostics' `BootReason` attribute
//! (`0x0004`, optional), with the same enum. It is out of reach, though:
//! `rs-matter-stack` chains the General Diagnostics handler itself, on top of ours,
//! and hands it `&()` as its `GenDiag` source, so there is no way to feed it a value
//! short of forking the stack as well as `rs-matter`. So the value is served here,
//! from a manufacturer-specific cluster on the root endpoint beside General
//! Diagnostics, using the standard enum's values. Moving it over later changes
//! nothing a reader has to relearn.
//!
//! Two attributes, both fixed for the life of a boot:
//!
//! - `BootReason` (`0x0000`, enum8): the General Diagnostics `BootReasonEnum`.
//! - `ResetCause` (`0x0001`, uint8): the raw ESP32-C6 reset-reason code the enum was
//!   derived from, `0` if `esp-hal` does not know it. The enum is coarse (it cannot
//!   say "USB-JTAG reset" and folds all watchdogs together), and this is what tells
//!   them apart. `0x01` power-on, `0x03` software, `0x08` TIMG1 watchdog, `0x0F`
//!   brownout, `0x15`/`0x16` USB-UART/USB-JTAG. See `supervisor.rs`.
//!
//! `GeneralDiagnostics::RebootCount` and `UpTime` already count and time the boots;
//! this adds only the *why*. Nothing here remembers the boots before this one.

use log::{info, warn};

use rs_matter_embassy::matter::dm::clusters::gen_diag::BootReasonEnum;
use rs_matter_embassy::matter::dm::devices::test::TEST_DEV_DET;
use rs_matter_embassy::matter::dm::{
    Access, AsyncHandler, AttrId, Attribute, Cluster, ClusterId, Dataver, MatchContext, Quality,
    ReadContext, ReadReply, Reply, WriteContext, ACCEPTED_COMMAND_LIST, ATTRIBUTE_LIST,
    CLUSTER_REVISION, FEATURE_MAP, GENERATED_COMMAND_LIST,
};
use rs_matter_embassy::matter::error::{Error, ErrorCode};
use rs_matter_embassy::matter::with;

use crate::supervisor;

/// The CSA test vendor ID, as for `element.rs`'s cluster and for the same reason.
const VENDOR_ID: u16 = TEST_DEV_DET.vid;

/// `0xFFF1_FC02`: the next suffix after `element.rs`'s `ELEMENT_WATTS_CLUSTER_ID`.
pub const BOOT_DIAG_CLUSTER_ID: ClusterId = ((VENDOR_ID as ClusterId) << 16) | 0xFC02;

/// `BootReason`: the General Diagnostics `BootReasonEnum` value.
pub const ATTR_BOOT_REASON: AttrId = 0x0000;

/// `ResetCause`: the raw ESP32-C6 reset-reason code.
pub const ATTR_RESET_CAUSE: AttrId = 0x0001;

const BOOT_DIAG_ATTRS: &[Attribute] = &[
    // Not `Quality::F`: fixed for a boot, not for the life of the device.
    Attribute::new(ATTR_BOOT_REASON, Access::RV, Quality::NONE),
    Attribute::new(ATTR_RESET_CAUSE, Access::RV, Quality::NONE),
    GENERATED_COMMAND_LIST,
    ACCEPTED_COMMAND_LIST,
    ATTRIBUTE_LIST,
    FEATURE_MAP,
    CLUSTER_REVISION,
];

/// Two read-only attributes, no commands, no events, no features.
pub const BOOT_DIAG_CLUSTER: Cluster<'static> = Cluster::new(
    BOOT_DIAG_CLUSTER_ID,
    1,
    0,
    BOOT_DIAG_ATTRS,
    &[],
    &[],
    with!(all),
    with!(),
    with!(),
);

/// Serves [`BOOT_DIAG_CLUSTER`], hand-rolled for the same reason as
/// `element.rs`'s `ElementWattsHandler`.
pub struct BootDiagHandler {
    dataver: Dataver,
    boot_reason: BootReasonEnum,
    reset_cause: u8,
}

impl BootDiagHandler {
    /// Read the reset reason once, log it, and hold it for the life of the boot.
    pub fn new(dataver: Dataver) -> Self {
        let cause = supervisor::reset_cause();
        let boot_reason = supervisor::boot_reason(cause);
        let reset_cause = supervisor::reset_cause_code(cause);

        match boot_reason {
            BootReasonEnum::BrownOutReset | BootReasonEnum::HardwareWatchdogReset => {
                warn!("Boot reason: {boot_reason:?} (reset cause {reset_cause:#04x}, {cause:?})")
            }
            _ => info!("Boot reason: {boot_reason:?} (reset cause {reset_cause:#04x}, {cause:?})"),
        }

        Self {
            dataver,
            boot_reason,
            reset_cause,
        }
    }
}

impl AsyncHandler for BootDiagHandler {
    fn read_awaits(&self, _ctx: impl ReadContext) -> bool {
        false
    }

    fn write_awaits(&self, _ctx: impl WriteContext) -> bool {
        false
    }

    async fn read(&self, ctx: impl ReadContext, reply: impl ReadReply) -> Result<(), Error> {
        let Some(writer) = reply.with_dataver(self.dataver.get())? else {
            return Ok(());
        };

        if ctx.attr().is_system() {
            return ctx
                .attr()
                .cluster(ctx.metadata(), |cluster| cluster.read(ctx.attr(), writer));
        }

        match ctx.attr().attr_id {
            ATTR_BOOT_REASON => Reply::set(writer, self.boot_reason),
            ATTR_RESET_CAUSE => Reply::set(writer, self.reset_cause),
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    fn bump_dataver(&self, ctx: impl MatchContext) {
        if ctx
            .cluster()
            .map(|c| c == BOOT_DIAG_CLUSTER_ID)
            .unwrap_or(true)
        {
            self.dataver.changed();
        }
    }
}
