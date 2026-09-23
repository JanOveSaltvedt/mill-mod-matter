//! What the heating element is rated at, as something a user can set rather than
//! something the firmware was built with.
//!
//! There is no metering hardware in the Mill and none was added by the mod, so the
//! element's plate rating *is* the meter: every reading `meter.rs` serves is that
//! number gated on one bit of the Mill's status frame. It used to come only from
//! `heater.element_watts` in `config.toml`, which meant a board flashed for a 600 W
//! panel metered a 1200 W panel at half its draw until somebody rebuilt and
//! reflashed it - through the back of a heater it is sealed inside.
//!
//! So it is exposed twice, on endpoint 2, and the two halves are one value:
//!
//! - **Mode Select** (`0x0050`) offers [`MODES`] - the ratings Mill actually ships -
//!   as a list to pick from. This is the half a controller can render: Home
//!   Assistant turns it into a `select` entity in the *config* category, which is
//!   exactly what it is.
//! - **A manufacturer-specific cluster** ([`ELEMENT_WATTS_CLUSTER`]) takes the exact
//!   figure, for a plate that reads 582 W rather than 600, or a model not on the
//!   list. No controller has a UI for it - it is reached with `chip-tool
//!   write-by-id` or the equivalent - which is fine for something set once per unit.
//!
//! [`ElementRating`] is the single piece of state behind both - `CurrentMode` itself
//! is owned and persisted by `rs-matter`'s `ModeSelectHandler`, and mirrored there -
//! and [`CUSTOM_MODE`] is what keeps them honest: `CurrentMode` is mandatory and
//! must always name a supported mode, so "somebody wrote an exact figure" has to be
//! *sayable* in the mode list. Writing `ElementWatts` therefore switches
//! `CurrentMode` to `Custom`, and the select entity reads "Custom" instead of
//! silently disagreeing with the meter.
//!
//! It runs the other way too: `ElementWatts` reads back the rating *in force*, not
//! the last figure written to it, so picking `1200 W` from the list makes it read
//! 1200. Between them the two clusters can be read in either order and never
//! disagree with each other or with what `meter.rs` is scaling by.
//!
//! # Why an endpoint of its own
//!
//! Mode Select's device type (`0x0027`) is `class="simple"` - an *application*
//! device type, the same class as Thermostat - and core spec 9.2.1 allows a simple
//! endpoint only one of those. That is the same rule `main.rs`'s `NODE` cites to
//! justify the Electrical Sensor sharing endpoint 1; Electrical Sensor only
//! qualifies because it is `class="utility"`. Mode Select does not, so it gets
//! endpoint 2. The manufacturer-specific cluster rides along beside it, which costs
//! nothing: an MS cluster is outside device-type conformance entirely and may sit on
//! any endpoint.

use core::cell::Cell;

use log::{error, info, warn};

use rs_matter_embassy::matter::dm::clusters::mode_select::{
    self, Mode, ModeId, ModeSelectHandler, ModeSelectHooks, SemanticTag,
};
use rs_matter_embassy::matter::dm::devices::test::TEST_DEV_DET;
use rs_matter_embassy::matter::dm::{
    Access, AsyncHandler, AttrId, Attribute, Cluster, ClusterId, Dataver, EndptId, HandlerContext,
    MatchContext, Quality, ReadContext, ReadReply, Reply, WriteContext, ACCEPTED_COMMAND_LIST,
    ATTRIBUTE_LIST, CLUSTER_REVISION, FEATURE_MAP, GENERATED_COMMAND_LIST,
};
use rs_matter_embassy::matter::error::{Error, ErrorCode};
use rs_matter_embassy::matter::tlv::FromTLV;
use rs_matter_embassy::matter::with;

use crate::config::{CIRCUIT_MAX_WATTS, DEFAULT_ELEMENT_WATTS};
use crate::heater::MillHeater;
use crate::vendor_kv::{VendorKv, ELEMENT_RATING_KEY};

/// The vendor whose namespace the semantic tags and the custom cluster belong to.
///
/// The CSA *test* vendor ID, `0xFFF1`, because that is what this device reports in
/// Basic Information and what its test DAC is issued for - see `dev_det` in
/// `main.rs` for why neither is configurable. A certified device would carry its own
/// here and the cluster ID below would move with it.
const VENDOR_ID: u16 = TEST_DEV_DET.vid;

/// The mode that means "the rating is in [`ELEMENT_WATTS_CLUSTER`], not in this
/// list".
///
/// Last rather than first so that [`MODES`] reads as the ascending list of ratings
/// it is, and so that the fallback below is a rating rather than an indirection.
pub const CUSTOM_MODE: ModeId = 11;

/// The tag value for [`CUSTOM_MODE`]: not a wattage, and deliberately a value no
/// real element has.
const CUSTOM_TAG: u16 = 0;

/// The ratings Mill ships its Gen 2 panels at, plus [`CUSTOM_MODE`].
///
/// **The tag value is the wattage.** That is the whole machine-readable half of an
/// entry: `Label` is for a human to recognise their heater by, and a client that
/// knows this namespace reads the rating out of the tag rather than parsing
/// `"1200 W"`. [`watts_of`] is the only thing that relies on it.
///
/// The list spans Mill's Invisible and Glass panel lines, which is the 250-2000 W
/// range Mill quotes for the platform. A panel whose plate is not on it is what
/// [`CUSTOM_MODE`] is for, so the list being incomplete costs a user one extra step
/// rather than locking them out.
///
/// **Mode values are persisted, so they may never be renumbered.**
/// `ModeSelectHandler` keeps `CurrentMode` under
/// [`ELEMENT_MODE_KEY`](crate::vendor_kv::ELEMENT_MODE_KEY). Appending a rating is
/// free; changing what an existing id means would silently re-rate every
/// device already storing it. Dropping one is survivable - `ModeSelectHandler`
/// repairs a `CurrentMode` that is no longer in the table by switching to the first
/// entry, which is why the first entry is the *lowest* rating: a device that lands
/// there understates its energy rather than inventing energy it never drew.
pub const MODES: &[Mode<'static>] = &[
    Mode::new(0, "250 W", &[SemanticTag::new(VENDOR_ID, 250)]),
    Mode::new(1, "400 W", &[SemanticTag::new(VENDOR_ID, 400)]),
    Mode::new(2, "600 W", &[SemanticTag::new(VENDOR_ID, 600)]),
    Mode::new(3, "700 W", &[SemanticTag::new(VENDOR_ID, 700)]),
    Mode::new(4, "800 W", &[SemanticTag::new(VENDOR_ID, 800)]),
    Mode::new(5, "900 W", &[SemanticTag::new(VENDOR_ID, 900)]),
    Mode::new(6, "1000 W", &[SemanticTag::new(VENDOR_ID, 1000)]),
    Mode::new(7, "1200 W", &[SemanticTag::new(VENDOR_ID, 1200)]),
    Mode::new(8, "1300 W", &[SemanticTag::new(VENDOR_ID, 1300)]),
    Mode::new(9, "1500 W", &[SemanticTag::new(VENDOR_ID, 1500)]),
    Mode::new(10, "2000 W", &[SemanticTag::new(VENDOR_ID, 2000)]),
    Mode::new(
        CUSTOM_MODE,
        "Custom",
        &[SemanticTag::new(VENDOR_ID, CUSTOM_TAG)],
    ),
];

/// Whether every entry in [`MODES`] is well formed: a unique `Mode` value, and a
/// semantic tag to read the rating out of.
///
/// `ModeSelectHandler::validate` checks the uniqueness half too, but it does so at
/// `Startup` and by panicking - which is the right shape for a library that cannot
/// see the table until the device boots, and the wrong place to find out about a
/// typo in a `const` sitting twenty lines up.
const fn modes_are_well_formed() -> bool {
    let mut i = 0;

    while i < MODES.len() {
        // Every entry carries its rating in a tag, which is what `watts_of` reads.
        // An anonymous mode is legal in the cluster and meaningless here.
        if MODES[i].tags.is_empty() {
            return false;
        }

        let mut j = i + 1;

        while j < MODES.len() {
            if MODES[i].id == MODES[j].id {
                return false;
            }

            j += 1;
        }

        i += 1;
    }

    true
}

const _: () = assert!(modes_are_well_formed());

// The fallback `ModeSelectHandler::repair_current_mode` picks for a `CurrentMode`
// that a firmware update has dropped from the table. It must be a real rating, or a
// device would land on `Custom` and adopt a wattage nobody chose for it.
const _: () = assert!(MODES[0].id != CUSTOM_MODE);

// `watts_of` distinguishes the two by `CUSTOM_MODE` alone, so the custom entry must
// not also look like a rating of `CUSTOM_TAG` watts.
const _: () = assert!(CUSTOM_TAG == 0);

/// What a device with nothing stored comes up with: whatever `config.toml` was built
/// with, expressed as a listed mode where one matches and as [`CUSTOM_MODE`] where
/// none does - in which case [`ElementRating`]'s custom figure defaults to the same
/// wattage.
///
/// So `heater.element_watts` keeps working exactly as it did for anybody who never
/// touches either cluster - including for a rating that is not on the list, which is
/// the case that would otherwise be a regression.
const fn default_mode() -> ModeId {
    let mut i = 0;

    while i < MODES.len() {
        // `tags[0]` cannot panic: `modes_are_well_formed` has already been asserted.
        if MODES[i].id != CUSTOM_MODE && MODES[i].tags[0].value == DEFAULT_ELEMENT_WATTS {
            return MODES[i].id;
        }

        i += 1;
    }

    CUSTOM_MODE
}

/// What `Description` reports: what this Mode Select instance selects.
///
/// The only thing that tells a client what the instance is *for* - the cluster
/// carries its purpose in this string rather than in its ID, which is the whole
/// difference between Mode Select and a Mode Base derivation.
const DESCRIPTION: &str = "Heating element rating";

/// How often `ElementWatts` is resampled for the benefit of subscribers.
///
/// Slow on purpose. The rating changes only when somebody changes it - once per unit,
/// realistically - so this tick is a comparison of two `u16`s that almost always
/// finds them equal, and there is nothing to be gained by finding out sooner.
const RATING_POLL: embassy_time::Duration = embassy_time::Duration::from_secs(5);

/// The manufacturer-specific cluster carrying the exact rating.
///
/// A manufacturer extensible identifier: [`VENDOR_ID`] in the top sixteen bits and a
/// suffix from the `0xFC00..=0xFFFE` range the Core spec reserves for a vendor's own
/// clusters. `rs-matter` serves this shape already - its `UnitTesting` cluster is
/// `0xFFF1FC05` - so nothing in the Interaction Model needs persuading.
pub const ELEMENT_WATTS_CLUSTER_ID: ClusterId = ((VENDOR_ID as ClusterId) << 16) | 0xFC01;

/// `ElementWatts`: the element's rating in watts, writable.
///
/// A plain short attribute ID rather than a second MEI - inside a
/// manufacturer-specific cluster the vendor prefix is already established, and the
/// standard range is what the spec uses there.
pub const ATTR_ELEMENT_WATTS: AttrId = 0x0000;

/// `RWVM`: readable by anyone on the fabric, writable only with Manage. The rating
/// is a commissioning-time fact about the hardware, not a control.
const ELEMENT_WATTS_ATTRS: &[Attribute] = &[
    Attribute::new(ATTR_ELEMENT_WATTS, Access::RWVM, Quality::N),
    // The global attributes, which `Cluster::read` answers on this cluster's behalf.
    // `EventList` is left out, as the generated clusters leave it out: it is not
    // served, so it does not belong in `AttributeList` either.
    GENERATED_COMMAND_LIST,
    ACCEPTED_COMMAND_LIST,
    ATTRIBUTE_LIST,
    FEATURE_MAP,
    CLUSTER_REVISION,
];

/// One writable attribute, no commands, no events, no features.
pub const ELEMENT_WATTS_CLUSTER: Cluster<'static> = Cluster::new(
    ELEMENT_WATTS_CLUSTER_ID,
    1,
    0,
    ELEMENT_WATTS_ATTRS,
    &[],
    &[],
    with!(all),
    with!(),
    with!(),
);

/// The rating a mode means, in watts, or `None` if `mode` is not in [`MODES`].
///
/// [`CUSTOM_MODE`] has no rating of its own - its rating is whatever
/// [`ElementRating::custom_watts`] holds - so it reads as `None` here too, and every
/// caller handles it before asking.
fn watts_of(mode: ModeId) -> Option<u16> {
    if mode == CUSTOM_MODE {
        return None;
    }

    MODES
        .iter()
        .find(|entry| entry.id == mode)
        .and_then(|entry| entry.tags.first())
        .map(|tag| tag.value)
}

/// What [`ELEMENT_RATING_KEY`] holds: the custom wattage as a little-endian `u16`.
///
/// `CurrentMode` is not in it - `ModeSelectHandler` persists that under a key of its
/// own. Firmware from before that kept both here, as three bytes with the mode
/// first; [`ElementWattsHandler::run`] hands such a mode over to the handler once,
/// and the blob is rewritten in the current form.
const RATING_LEN: usize = 2;

/// The length of the older, mode-carrying blob.
const LEGACY_RATING_LEN: usize = 3;

/// What the heating element is rated at: the state behind the Mode Select cluster
/// and the manufacturer-specific one alike.
pub struct ElementRating<'a> {
    /// `CurrentMode` as `ModeSelectHandler` last had it. The handler owns the
    /// attribute; this is kept in step through [`ModeSelectHooks::change_to_mode`]
    /// and, for the value it restores at `Startup` without calling that, through
    /// [`Self::adopt_mode`].
    mode: Cell<ModeId>,
    custom_watts: Cell<u16>,
    /// A `CurrentMode` found in an older firmware's blob, awaiting handover to the
    /// handler - see [`LEGACY_RATING_LEN`].
    legacy_mode: Cell<Option<ModeId>>,
    /// The heater this rating meters. Every change is pushed here, because
    /// `MillHeater::active_power_mw` is what the Electrical Power and Electrical
    /// Energy Measurement clusters actually read.
    heater: &'a MillHeater<'a>,
    kv: &'a dyn VendorKv,
}

impl<'a> ElementRating<'a> {
    /// Restore the custom rating from `kv` and push the default rating onto
    /// `heater`.
    ///
    /// The mode in force is not known until `ModeSelectHandler` has restored it at
    /// `Startup`; [`ElementWattsHandler::run`] adopts it from there. Until then the
    /// heater meters at [`ModeSelectHooks::CURRENT_MODE`] - which is to say at
    /// `config.toml`'s rating - and the Mill has not been heard from yet anyway.
    pub fn new(kv: &'a dyn VendorKv, heater: &'a MillHeater<'a>) -> Self {
        let mut buf = [0u8; LEGACY_RATING_LEN];

        let (legacy_mode, stored) = match kv.load_blob(ELEMENT_RATING_KEY, &mut buf) {
            Ok(Some(RATING_LEN)) => (None, u16::from_le_bytes([buf[0], buf[1]])),
            Ok(Some(LEGACY_RATING_LEN)) => (Some(buf[0]), u16::from_le_bytes([buf[1], buf[2]])),
            _ => (None, DEFAULT_ELEMENT_WATTS),
        };

        // The blob predates the running firmware, so the custom wattage may be above
        // a `circuit_max_watts` that has since been lowered. The configured default
        // is a better answer than a reading nobody can justify.
        //
        // A legacy mode that is merely *unknown* is left alone rather than corrected
        // here - `ModeSelectHandler::apply_mode` refuses it when it is handed over,
        // and the handler's own `CurrentMode` stands.
        let custom_watts = if (1..=CIRCUIT_MAX_WATTS).contains(&stored) {
            stored
        } else {
            warn!(
                "Element: stored custom rating {stored} W is outside 1..={CIRCUIT_MAX_WATTS} W; \
                 falling back to {DEFAULT_ELEMENT_WATTS} W"
            );

            DEFAULT_ELEMENT_WATTS
        };

        let rating = Self {
            mode: Cell::new(Self::CURRENT_MODE),
            custom_watts: Cell::new(custom_watts),
            legacy_mode: Cell::new(legacy_mode),
            heater,
            kv,
        };

        rating.apply();

        rating
    }

    /// The rating in force, in watts: the selected mode's, or the custom figure
    /// while [`CUSTOM_MODE`] is selected.
    ///
    /// A mode that is in neither category cannot survive `Startup`, but until then
    /// it reads as the configured default rather than panicking.
    pub fn effective_watts(&self) -> u16 {
        let mode = self.mode.get();

        if mode == CUSTOM_MODE {
            self.custom_watts.get()
        } else {
            watts_of(mode).unwrap_or(DEFAULT_ELEMENT_WATTS)
        }
    }

    /// Adopt the `CurrentMode` the handler restored, and meter at it.
    fn adopt_mode(&self, mode: ModeId) {
        self.mode.set(mode);
        self.apply();

        info!(
            "Element: rating restored as mode {mode} ({} W)",
            self.effective_watts()
        );
    }

    /// Set the exact rating, returning whether `CurrentMode` now has to move to
    /// [`CUSTOM_MODE`].
    ///
    /// The caller does that move through `ModeSelectHandler::apply_mode`, which is
    /// what notifies subscribers of the new `CurrentMode` - and which lands back in
    /// [`ModeSelectHooks::change_to_mode`] below, where the push happens.
    pub fn set_custom_watts(&self, watts: u16) -> Result<bool, Error> {
        // Checked before anything is mutated, so a rejected write leaves no trace.
        if !(1..=CIRCUIT_MAX_WATTS).contains(&watts) {
            error!("Element: refusing a rating of {watts} W, outside 1..={CIRCUIT_MAX_WATTS} W");

            return Err(ErrorCode::ConstraintError.into());
        }

        self.custom_watts.set(watts);

        if self.mode.get() == CUSTOM_MODE {
            // In this order: the heater has to be metering at the new rating before
            // the rating is committed to flash, or an unclean power-off in between
            // would leave the two disagreeing.
            self.apply();
            self.save_state();

            return Ok(false);
        }

        self.save_state();

        Ok(true)
    }

    /// Push the rating in force onto the heater.
    fn apply(&self) {
        self.heater.set_element_watts(self.effective_watts());
    }

    fn save_state(&self) {
        let bytes = self.custom_watts.get().to_le_bytes();

        if let Err(e) = self.kv.store_blob(ELEMENT_RATING_KEY, &bytes) {
            error!("Element: could not persist the element rating: {e}");
        }
    }
}

impl ModeSelectHooks for ElementRating<'_> {
    /// The mandatory four attributes and `ChangeToMode`, and nothing else.
    ///
    /// `StartUpMode` and `OnMode` are deliberately left out. Both exist to *re-pick*
    /// a mode, at power-up or when an OnOff cluster on the same endpoint turns on,
    /// and this instance does not select behaviour that can be re-picked: it records
    /// a fact about the hardware the board is bolted to, which is the same fact after
    /// a reboot as before it.
    const CLUSTER: Cluster<'static> = mode_select::CLUSTER;

    const CURRENT_MODE: ModeId = default_mode();

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn supported_modes(&self) -> &[Mode<'_>] {
        MODES
    }

    /// Adopt `mode` and meter the element at what it says. The handler persists it.
    ///
    /// The handler has already checked that `mode` is in [`MODES`] and differs from
    /// the current one, so the only thing left to refuse is a listed rating above
    /// `heater.circuit_max_watts` - which cannot happen with the shipped
    /// `config.toml`, but can on a board configured for a 6 A circuit.
    fn change_to_mode(&self, mode: ModeId) -> Result<(), Error> {
        if let Some(watts) = watts_of(mode) {
            if watts > CIRCUIT_MAX_WATTS {
                error!(
                    "Element: refusing mode {mode} ({watts} W), above the \
                     {CIRCUIT_MAX_WATTS} W circuit maximum"
                );

                return Err(ErrorCode::ConstraintError.into());
            }
        }

        self.mode.set(mode);
        self.apply();

        Ok(())
    }
}

/// Serves [`ELEMENT_WATTS_CLUSTER`]: one writable attribute, hand-rolled.
///
/// Hand-rolled because there is nothing to generate from - `rs-matter`'s codegen
/// reads the CSA IDL, which by definition has nothing to say about a vendor's own
/// cluster. The shape below is the generated `HandlerAsyncAdaptor`'s, minus the
/// dispatch over an attribute enum that a single attribute does not need.
pub struct ElementWattsHandler<'a> {
    dataver: Dataver,
    /// Where this cluster lives, for the one report that is raised outside an
    /// operation and so carries no path of its own - see [`AsyncHandler::run`].
    endpoint: EndptId,
    rating: &'a ElementRating<'a>,
    /// The Mode Select handler on the same endpoint.
    ///
    /// Held so that a write here can move `CurrentMode` to [`CUSTOM_MODE`] *through*
    /// the cluster that owns it, which is what gets subscribers told. Reaching into
    /// [`ElementRating`] directly would change the mode without reporting it, and a
    /// controller's select entity would sit on a stale option indefinitely.
    mode: &'a ModeSelectHandler<&'a ElementRating<'a>>,
}

impl<'a> ElementWattsHandler<'a> {
    pub const fn new(
        dataver: Dataver,
        endpoint: EndptId,
        rating: &'a ElementRating<'a>,
        mode: &'a ModeSelectHandler<&'a ElementRating<'a>>,
    ) -> Self {
        Self {
            dataver,
            endpoint,
            rating,
            mode,
        }
    }
}

impl AsyncHandler for ElementWattsHandler<'_> {
    /// Neither path touches flash asynchronously or awaits anything, which lets the
    /// Interaction Model skip buffering the request.
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
            // The rating actually in force, which is not always the last figure
            // written here: picking a listed mode supersedes it. Reporting the
            // superseded figure would make this attribute disagree with the meter it
            // scales, which is the one thing it must never do.
            ATTR_ELEMENT_WATTS => Reply::set(writer, self.rating.effective_watts()),
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    async fn write(&self, ctx: impl WriteContext) -> Result<(), Error> {
        ctx.attr().check_dataver(self.dataver.get())?;

        if ctx.attr().is_system() {
            return Err(ErrorCode::InvalidAction.into());
        }

        match ctx.attr().attr_id {
            ATTR_ELEMENT_WATTS => {
                let watts: u16 = FromTLV::from_tlv(ctx.data())?;

                let switching = self.rating.set_custom_watts(watts)?;

                self.dataver.changed();
                ctx.notify_changed();

                if switching {
                    // Lands in `change_to_mode` above, which adopts the figure just
                    // stored and pushes it onto the heater; the handler persists the
                    // mode.
                    // `apply_mode` cannot fail here: `CUSTOM_MODE` is in `MODES` and
                    // carries no rating of its own to be out of range.
                    self.mode
                        .apply_mode(CUSTOM_MODE, ctx.attr().endpoint_id, &ctx)?;
                }

                info!("Element: rating set to an exact {watts} W");

                Ok(())
            }
            _ => Err(ErrorCode::AttributeNotFound.into()),
        }
    }

    fn bump_dataver(&self, ctx: impl MatchContext) {
        if ctx
            .cluster()
            .map(|c| c == ELEMENT_WATTS_CLUSTER_ID)
            .unwrap_or(true)
        {
            self.dataver.changed();
        }
    }

    /// Report `ElementWatts` when a `ChangeToMode` on the Mode Select cluster beside
    /// this one moves it.
    ///
    /// The write path above reports its own change, but a mode change reaches this
    /// attribute without passing through this handler at all, and the two clusters
    /// cannot notify each other: `ModeSelectHooks::change_to_mode` is handed no
    /// context to notify *with*, and giving [`ElementRating`] a back-reference to
    /// this handler would close a cycle around the handler that borrows it.
    ///
    /// So it is sampled, for the same reason and in the same shape as
    /// `ElecPwrMeasHooks::run` in `meter.rs`: the value it watches changes only when
    /// a human changes it, so the tick can be slow and is nearly always a no-op.
    ///
    /// It is also where the rating learns which mode is in force at all. The
    /// `Startup` lifecycle op is where `ModeSelectHandler` restores `CurrentMode`,
    /// and it does so without calling the hooks; the Interaction Model has finished
    /// `Startup` before it polls any `run`, whatever order the chain is in.
    async fn run(&self, ctx: impl HandlerContext) -> Result<(), Error> {
        self.rating.adopt_mode(self.mode.current_mode());

        // An older firmware's mode, handed over once - see `LEGACY_RATING_LEN`.
        //
        // Rewriting the blob in the current form drops that mode, so it waits for
        // the first tick: `RATING_POLL` is longer than the handler's persist delay,
        // and a power cut before then just migrates again on the next boot.
        let mut migrating = if let Some(mode) = self.rating.legacy_mode.take() {
            info!("Element: moving mode {mode} into the Mode Select handler's store");

            if let Err(e) = self.mode.apply_mode(mode, self.endpoint, &ctx) {
                warn!("Element: could not restore the pre-upstream mode {mode}: {e}");
            }

            true
        } else {
            false
        };

        let mut reported = self.rating.effective_watts();

        // Never returns. `ChainedHandler::run` selects over every handler's `run`, so
        // one that finishes would end the whole chain.
        loop {
            embassy_time::Timer::after(RATING_POLL).await;

            if core::mem::take(&mut migrating) {
                self.rating.save_state();
            }

            let watts = self.rating.effective_watts();

            if watts != reported {
                reported = watts;

                self.dataver.changed();
                ctx.notify_attr_changed(
                    self.endpoint,
                    ELEMENT_WATTS_CLUSTER_ID,
                    ATTR_ELEMENT_WATTS,
                );
            }
        }
    }
}
