//! Structured diagnostics: the machine-readable record of what a link did.
//!
//! The JSON shape defined here is a **stable contract**, established at M0 and
//! extended — never restructured — as later milestones populate more fields.
//! Integration tests assert against these fields directly, which is what makes
//! claims like "reused_inputs > 0 after a no-op rebuild" a one-line test rather
//! than a benchmark-only observation.
//!
//! Fields that a given milestone cannot yet populate are emitted as `null`
//! rather than omitted, so consumers can rely on the key set being stable.

use std::path::{Path, PathBuf};
use std::time::Duration;

mod fingerprint;
pub use fingerprint::{fingerprint_input, InputFingerprint};

pub mod schema {
    /// Bumped whenever the emitted JSON gains or changes a field.
    ///
    /// Consumers should reject a major version they do not understand rather
    /// than silently misreading a renamed field.
    pub const VERSION: u32 = 1;
}

/// How a link was ultimately performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LinkMode {
    /// Arguments recorded, link delegated to the system linker. The only mode
    /// M0 can produce.
    Delegated,
    /// Full internal link with no reuse of prior state (M2+).
    Cold,
    /// Internal link reusing cached parse results (M4+).
    Cached,
    /// Internal link reusing prior link state (M5+).
    Incremental,
    /// Internal path abandoned; the system linker was invoked instead.
    ExternalFallback,
}

/// Why an internal link was not used. Mirrors spec §30; M0 only ever reports
/// `NotImplemented`, but the enum is defined up front so the field is stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReason {
    /// The internal linker does not exist yet (M0/M1).
    NotImplemented,
    NoPreviousState,
    CacheSchemaMismatch,
    CacheCorrupt,
    UnsupportedArgument,
    UnsupportedInputFormat,
    ValidationFailed,
}

/// Per-phase wall-clock timings. Phases not run in a given mode stay `None` and
/// serialize as `null`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct PhaseTimings {
    pub argument_parsing_ms: Option<f64>,
    pub input_fingerprinting_ms: Option<f64>,
    pub fallback_exec_ms: Option<f64>,
    /// Wall time of the internal link, when one was performed.
    ///
    /// Separate from `fallback_exec_ms`: an internal link is not a fallback,
    /// and reporting it in that field made the two indistinguishable in a
    /// record.
    pub internal_link_ms: Option<f64>,
    /// Reading the first bytes of every input to see whether any is a format
    /// blinker does not link, and assembling the `LinkRequest` — which
    /// resolves every `-l` to a file on disk.
    pub input_precheck_ms: Option<f64>,
    pub link_request_ms: Option<f64>,
    /// Stages within the internal link.
    pub link_read_and_parse_ms: Option<f64>,
    pub link_resolve_ms: Option<f64>,
    pub link_layout_ms: Option<f64>,
    /// Reachability analysis, when `-dead_strip` was asked for.
    ///
    /// Reported because it is one of the two largest stages and was invisible:
    /// it was measured inside the link and then dropped on the way out, so a
    /// record showed five stages summing to well under the link's own total
    /// and nothing saying where the rest went.
    pub link_dead_strip_ms: Option<f64>,
    /// The three halves of the strip, all inside `link_dead_strip_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_atoms_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_liveness_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_strip_build_ms: Option<f64>,
    /// Between the stages, and beside them: see `LinkTimings::prepare_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_prepare_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_accounting_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_address_table_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_address_diff_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_placements_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_personality_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_unwind_size_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_commons_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_digest_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_group_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_traverse_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_eh_frame_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_tables_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_unwind_ms: Option<f64>,
    pub link_relocate_ms: Option<f64>,
    pub link_emit_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_write_ms: Option<f64>,
    /// What the cache cost, as against what it saved.
    ///
    /// Absent when no cache was asked for. Reported because reuse counters
    /// alone describe only one side of a trade: an edit relink reusing 78% of
    /// its relocations measured 5.5 ms *slower* than the same link with the
    /// cache switched off, and none of the 5.5 ms appeared under any stage —
    /// decoding and planning were charged to `relocate`, and building and
    /// storing happened after `emit` had stopped.
    /// Parsing the SDK's `.tbd` stubs, which overlaps reading the objects.
    /// Only a cost when it is the longer of the two.
    pub link_stub_parse_ms: Option<f64>,
    /// Inside `emit`: layout, contents, linkedit, assemble, sign.
    pub link_emit_layout_ms: Option<f64>,
    pub link_emit_contents_ms: Option<f64>,
    pub link_emit_linkedit_ms: Option<f64>,
    pub link_emit_assemble_ms: Option<f64>,
    /// Inside `relocate`: address map, contents, synthetic tables, apply.
    pub link_address_map_ms: Option<f64>,
    pub link_contents_ms: Option<f64>,
    pub link_synthetic_ms: Option<f64>,
    pub link_apply_ms: Option<f64>,
    /// The output symbol table and debug map, between `relocate` and `emit`.
    pub link_symbols_ms: Option<f64>,
    /// Surveying relocations for GOT/stub/TLV slots.
    pub link_survey_ms: Option<f64>,
    pub link_emit_uuid_ms: Option<f64>,
    pub link_emit_sign_ms: Option<f64>,
    pub link_cache_load_ms: Option<f64>,
    pub link_cache_plan_ms: Option<f64>,
    pub link_cache_build_ms: Option<f64>,
    pub link_cache_store_ms: Option<f64>,
    /// The five formerly unattributed regions; see `LinkStages::residue`.
    pub link_intern_ids_ms: Option<f64>,
    pub link_strip_stats_ms: Option<f64>,
    pub link_handback_ms: Option<f64>,
    pub link_teardown_ms: Option<f64>,
    pub link_session_stats_ms: Option<f64>,
    pub link_finished_probe_ms: Option<f64>,
    pub total_ms: Option<f64>,
}

/// How long each stage of an internal link took, in milliseconds.
///
/// A struct rather than six positional arguments: they are all `f64` and all
/// milliseconds, so a caller that swapped two would compile and report a
/// plausible breakdown of the wrong shape.
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkStages {
    pub read_and_parse: f64,
    pub resolve: f64,
    pub layout: f64,
    pub dead_strip: f64,
    pub relocate: f64,
    pub emit: f64,
    pub write: f64,
    /// Parsing `.tbd` stubs, which happens on its own thread inside
    /// `read_and_parse` rather than after it.
    pub stub_parse: f64,
    /// Inside `emit`, in order: layout, contents, linkedit, assemble, sign.
    pub emit_breakdown: [f64; 6],
    /// Inside `relocate`, in order: address map, contents, synthetic, apply.
    pub relocate_breakdown: [f64; 4],
    /// Inside `dead_strip`, in order: atoms, liveness, strip build.
    pub strip_breakdown: [f64; 3],
    /// Work that belonged to no stage: preparation, and the invariant counter.
    pub prepare: f64,
    pub accounting: f64,
    /// The rest of what belonged to no stage, in order: gathering interned
    /// name ids, copying the strip's counters, handing the symbol and string
    /// tables back to the session, freeing everything the link built, and
    /// reading the session's counters. `teardown` is the fourth: every stage
    /// timer stops before its locals drop, so a link's *deallocation* was in
    /// the total and in no stage.
    pub residue: [f64; 5],
    /// Probing whether the finished image can be replayed, before the link.
    pub finished_probe: f64,
    pub address_table: f64,
    pub address_diff: f64,
    /// Inside `synthetic`: eh_frame repair, indirect tables, unwind info.
    pub synthetic_breakdown: [f64; 3],
    /// Inside `liveness`: grouping relocations per object, then traversing.
    pub liveness_breakdown: [f64; 2],
    /// Reachability digests: time, objects moved, objects compared.
    pub digest: f64,
    pub reach_moved: u64,
    pub reach_total: u64,
    /// Objects whose symbol-table entries were kept, of how many there were.
    pub symbols_reused: u64,
    pub symbols_total: u64,
    /// Bytes of per-target state the session holds, and its budget.
    pub held_bytes: u64,
    pub budget_bytes: u64,
    /// Inside `prepare`: placements, eh personalities, unwind sizing, commons.
    pub prepare_breakdown: [f64; 4],
    pub changed_addresses: u64,
    pub total_addresses: u64,
    /// Output symbols and the debug map.
    pub symbols: f64,
    /// Surveying relocations for indirect slots.
    pub survey: f64,
    /// Load, plan, build, store — the cache's four costs, zero without one.
    pub cache: CacheStages,
}

/// What the incremental cache spends, split by what it is doing.
#[derive(Debug, Clone, Copy, Default)]
pub struct CacheStages {
    /// Reading and decoding the previous cache.
    pub load: f64,
    /// Deciding which objects may skip relocation.
    pub plan: f64,
    /// Assembling the cache this link will leave behind.
    pub build: f64,
    /// Encoding and writing it, including the copy of the finished image.
    pub store: f64,
}

impl CacheStages {
    /// Whether a cache ran at all, so absent and free stay distinguishable.
    fn ran(&self) -> bool {
        self.load > 0.0 || self.plan > 0.0 || self.build > 0.0 || self.store > 0.0
    }
}

fn as_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Counters describing the work a link performed.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Counters {
    pub input_count: u64,
    pub changed_inputs: Option<u64>,
    pub reused_inputs: Option<u64>,
    /// Relocations skipped because their object was reused, and the total.
    ///
    /// The unit that makes a hit rate mean what it looks like it means. Object
    /// sizes in a Rust link span three orders of magnitude, so "46 of 47
    /// objects reused" can describe a link that saved nothing at all — that is
    /// exactly what it described (finding 66).
    pub reused_relocations: Option<u64>,
    pub total_relocations: Option<u64>,
    pub bytes_read: u64,
    pub bytes_written: Option<u64>,
    /// Bytes the incremental cache read and wrote, counted apart from the
    /// inputs and the output.
    ///
    /// The cache currently stores every patched output section *and* a copy of
    /// the finished binary, so what it writes is comparable in size to what the
    /// link produces. Rolled into `bytes_written` that would look like a large
    /// output; named, it looks like what it is.
    /// Contributions that kept their previous address, and those that did not.
    ///
    /// Reported because "relocations reused" cannot distinguish a layout that
    /// held from a cache that got lucky, and the layout is the thing that has
    /// to hold first.
    pub contributions_retained: Option<u64>,
    pub contributions_moved: Option<u64>,
    /// Of those, the ones whose input had not changed. Meant to be zero.
    pub contributions_moved_unchanged: Option<u64>,
    /// What a resident linker reused: inputs served from memory, inputs it had
    /// to read, and whether the two derived answers still held.
    pub inputs_held: Option<u64>,
    /// Addresses this link changed, and how many there are. The size of the
    /// blast radius in the units relocation cares about.
    /// Objects whose reachability projection moved, of how many compared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach_moved: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach_total: Option<u64>,
    /// Objects whose symbol-table entries were kept from the previous link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols_reused: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols_total: Option<u64>,
    /// Bytes of per-target state held, and the budget it is held against.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_addresses: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_addresses: Option<u64>,
    pub inputs_read: Option<u64>,
    pub replayed_extraction: Option<bool>,
    pub held_resolution: Option<bool>,
    /// Inputs whose symbol interface moved, and the first one that did.
    pub interface_changes: Option<u64>,
    pub first_interface_change: Option<String>,
    pub cache_bytes_read: Option<u64>,
    pub cache_bytes_written: Option<u64>,
    /// `__text` bytes dead-stripping removed. `None` when it did not run.
    pub stripped_bytes: Option<u64>,
    /// Atoms the reachability propagation left dead that something live then
    /// referred to.
    ///
    /// Must be zero. It is reported rather than asserted because the
    /// verification pass that produces it *repairs* the mistake — a non-zero
    /// count is a link that is correct and a model that is incomplete, and
    /// the only way to notice is for the number to be visible.
    pub revived_atoms: Option<u64>,
}

/// The complete record of one invocation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LinkRecord {
    pub schema_version: u32,
    pub mode: LinkMode,
    pub fallback_reason: Option<FallbackReason>,
    pub exit_code: i32,
    pub output_path: Option<PathBuf>,
    pub arch: Option<String>,
    pub deployment_target: Option<String>,
    pub timings: PhaseTimings,
    pub counters: Counters,
    pub inputs: Vec<InputFingerprint>,
    /// Arguments the parser could not classify. Empty is the healthy state;
    /// non-empty is the M0 deliverable that tells us what to model next.
    pub unrecognized_arguments: Vec<String>,
    /// Verbatim argument vector, exactly as the linker received it.
    pub argv: Vec<String>,
    /// Argument vector rewritten to point at archived input copies.
    ///
    /// `None` when the invocation was not archived, in which case replay falls
    /// back to `argv` — which only works while the original inputs still exist.
    /// See [`InputFingerprint::archived_path`] for why that is usually not the
    /// case for a recorded corpus.
    pub replay_argv: Option<Vec<String>>,
}

impl LinkRecord {
    /// Start a record for a delegated (M0) link.
    pub fn delegated() -> Self {
        // A seam for `scripts/relink.py`; the constant below is what it edits.
        //
        // It exists because there is no other way to write a *body* edit in
        // Rust from outside the source. Appending an item — even a private one
        // — repartitions the crate's codegen units, and rustc then renames
        // symbols wholesale: an appended private function measured 8498 of 6877
        // addresses changed, against 252 for adding a `pub fn`. An appended
        // dead item measured the opposite failure, 0 of 6842, because
        // dead-strip removed it again.
        //
        // Changing a literal inside a function that already exists adds no
        // item, moves no partition, and renames nothing. That is the edit a
        // developer makes, and this is the only shape of it a script can
        // produce. `delegated` is on every invocation's path, so the code is
        // live and dead-strip keeps it.
        std::hint::black_box(relink_seam(0x0000_0000_0000_0000));
        LinkRecord {
            schema_version: schema::VERSION,
            mode: LinkMode::Delegated,
            fallback_reason: Some(FallbackReason::NotImplemented),
            exit_code: 0,
            output_path: None,
            arch: None,
            deployment_target: None,
            timings: PhaseTimings::default(),
            counters: Counters::default(),
            inputs: Vec::new(),
            unrecognized_arguments: Vec::new(),
            argv: Vec::new(),
            replay_argv: None,
        }
    }

    pub fn set_timing_argument_parsing(&mut self, d: Duration) {
        self.timings.argument_parsing_ms = Some(as_ms(d));
    }

    pub fn set_timing_fingerprinting(&mut self, d: Duration) {
        self.timings.input_fingerprinting_ms = Some(as_ms(d));
    }

    pub fn set_timing_fallback_exec(&mut self, d: Duration) {
        self.timings.fallback_exec_ms = Some(as_ms(d));
    }

    /// Deciding whether this link is one blinker can do, and building the
    /// request for it.
    ///
    /// Both sit inside `internal_link_ms`, which is measured from the driver
    /// rather than from the linker, and belonged to no stage — which is where
    /// a quarter of a ripgrep relink was hiding (finding 232).
    pub fn set_timing_setup(&mut self, precheck: Duration, request: Duration) {
        self.timings.input_precheck_ms = Some(as_ms(precheck));
        self.timings.link_request_ms = Some(as_ms(request));
    }

    /// Record the internal link's total and its per-stage breakdown.
    pub fn set_timing_internal_link(&mut self, total: Duration, stages: LinkStages) {
        let LinkStages {
            read_and_parse,
            resolve,
            layout,
            dead_strip,
            relocate,
            emit,
            write: write_ms,
            cache,
            stub_parse,
            emit_breakdown,
            relocate_breakdown,
            symbols,
            survey,
            strip_breakdown,
            prepare,
            accounting,
            residue,
            finished_probe,
            address_table,
            address_diff,
            synthetic_breakdown,
            liveness_breakdown,
            digest,
            reach_moved,
            reach_total,
            symbols_reused,
            symbols_total,
            held_bytes,
            budget_bytes,
            prepare_breakdown,
            changed_addresses,
            total_addresses,
        } = stages;
        self.timings.internal_link_ms = Some(as_ms(total));
        self.timings.link_read_and_parse_ms = Some(read_and_parse);
        self.timings.link_resolve_ms = Some(resolve);
        self.timings.link_layout_ms = Some(layout);
        // Absent rather than zero when nothing was stripped, so "the stage did
        // not run" and "the stage was free" stay distinguishable.
        self.timings.link_dead_strip_ms = (dead_strip > 0.0).then_some(dead_strip);
        self.timings.link_stub_parse_ms = (stub_parse > 0.0).then_some(stub_parse);
        if emit_breakdown.iter().any(|v| *v > 0.0) {
            self.timings.link_emit_layout_ms = Some(emit_breakdown[0]);
            self.timings.link_emit_contents_ms = Some(emit_breakdown[1]);
            self.timings.link_emit_linkedit_ms = Some(emit_breakdown[2]);
            self.timings.link_emit_assemble_ms = Some(emit_breakdown[3]);
            self.timings.link_emit_uuid_ms = Some(emit_breakdown[4]);
            self.timings.link_emit_sign_ms = Some(emit_breakdown[5]);
        }
        self.timings.link_prepare_ms = (prepare > 0.0).then_some(prepare);
        self.timings.link_address_table_ms = (address_table > 0.0).then_some(address_table);
        self.timings.link_address_diff_ms = (address_diff > 0.0).then_some(address_diff);
        self.timings.link_digest_ms = (digest > 0.0).then_some(digest);
        if reach_total > 0 {
            self.counters.reach_moved = Some(reach_moved);
            self.counters.reach_total = Some(reach_total);
        }
        if symbols_total > 0 {
            self.counters.symbols_reused = Some(symbols_reused);
            self.counters.symbols_total = Some(symbols_total);
        }
        if budget_bytes > 0 {
            self.counters.held_bytes = Some(held_bytes);
            self.counters.budget_bytes = Some(budget_bytes);
        }
        if prepare_breakdown.iter().any(|v| *v > 0.0) {
            self.timings.link_placements_ms = Some(prepare_breakdown[0]);
            self.timings.link_personality_ms = Some(prepare_breakdown[1]);
            self.timings.link_unwind_size_ms = Some(prepare_breakdown[2]);
            self.timings.link_commons_ms = Some(prepare_breakdown[3]);
        }
        if liveness_breakdown.iter().any(|v| *v > 0.0) {
            self.timings.link_group_ms = Some(liveness_breakdown[0]);
            self.timings.link_traverse_ms = Some(liveness_breakdown[1]);
        }
        if synthetic_breakdown.iter().any(|v| *v > 0.0) {
            self.timings.link_eh_frame_ms = Some(synthetic_breakdown[0]);
            self.timings.link_tables_ms = Some(synthetic_breakdown[1]);
            self.timings.link_unwind_ms = Some(synthetic_breakdown[2]);
        }
        if total_addresses > 0 {
            self.counters.changed_addresses = Some(changed_addresses);
            self.counters.total_addresses = Some(total_addresses);
        }
        self.timings.link_accounting_ms = (accounting > 0.0).then_some(accounting);
        if strip_breakdown.iter().any(|v| *v > 0.0) {
            self.timings.link_atoms_ms = Some(strip_breakdown[0]);
            self.timings.link_liveness_ms = Some(strip_breakdown[1]);
            self.timings.link_strip_build_ms = Some(strip_breakdown[2]);
        }
        if relocate_breakdown.iter().any(|v| *v > 0.0) {
            self.timings.link_address_map_ms = Some(relocate_breakdown[0]);
            self.timings.link_contents_ms = Some(relocate_breakdown[1]);
            self.timings.link_synthetic_ms = Some(relocate_breakdown[2]);
            self.timings.link_apply_ms = Some(relocate_breakdown[3]);
        }
        for (field, value) in [
            (&mut self.timings.link_intern_ids_ms, residue[0]),
            (&mut self.timings.link_strip_stats_ms, residue[1]),
            (&mut self.timings.link_handback_ms, residue[2]),
            (&mut self.timings.link_teardown_ms, residue[3]),
            (&mut self.timings.link_session_stats_ms, residue[4]),
        ] {
            *field = (value > 0.0).then_some(value);
        }
        self.timings.link_finished_probe_ms = (finished_probe > 0.0).then_some(finished_probe);
        self.timings.link_symbols_ms = (symbols > 0.0).then_some(symbols);
        self.timings.link_survey_ms = (survey > 0.0).then_some(survey);
        self.timings.link_relocate_ms = Some(relocate);
        self.timings.link_emit_ms = Some(emit);
        self.timings.link_write_ms = (write_ms > 0.0).then_some(write_ms);
        if cache.ran() {
            self.timings.link_cache_load_ms = Some(cache.load);
            self.timings.link_cache_plan_ms = Some(cache.plan);
            self.timings.link_cache_build_ms = Some(cache.build);
            self.timings.link_cache_store_ms = Some(cache.store);
        }
    }

    /// Record how much of the previous layout survived.
    pub fn set_placement(&mut self, retained: u64, moved: u64, moved_unchanged: u64) {
        self.counters.contributions_retained = Some(retained);
        self.counters.contributions_moved = Some(moved);
        self.counters.contributions_moved_unchanged = Some(moved_unchanged);
    }

    /// Record what a resident session reused.
    pub fn set_session(
        &mut self,
        held: u64,
        read: u64,
        extraction: bool,
        resolution: bool,
        interface_changes: u64,
        first_change: Option<&std::path::Path>,
    ) {
        if held == 0 && read == 0 {
            return;
        }
        self.counters.inputs_held = Some(held);
        self.counters.inputs_read = Some(read);
        self.counters.replayed_extraction = Some(extraction);
        self.counters.held_resolution = Some(resolution);
        self.counters.interface_changes = Some(interface_changes);
        self.counters.first_interface_change = first_change.map(|p| p.display().to_string());
    }

    /// Record what the cache moved, as bytes.
    pub fn set_cache_bytes(&mut self, read: u64, written: u64) {
        self.counters.cache_bytes_read = Some(read);
        self.counters.cache_bytes_written = Some(written);
    }

    /// Record how much of the link came from the cache.
    ///
    /// Surfaced by default rather than on request. A cache that silently stops
    /// working produces a *correct* binary and a slower link, which no
    /// correctness test can distinguish from a cache that works — blinker's
    /// reused nothing at all on a real Rust link while every test passed, and
    /// the only evidence was a counter nothing printed (finding 64).
    ///
    /// The mode follows from the counters rather than being set separately, so
    /// a record cannot claim to be incremental while reporting no reuse.
    pub fn set_reuse(&mut self, reused: u64, total: u64, work: (u64, u64)) {
        self.counters.reused_inputs = Some(reused);
        self.counters.changed_inputs = Some(total.saturating_sub(reused));
        self.counters.reused_relocations = Some(work.0);
        self.counters.total_relocations = Some(work.1);
        self.mode = if reused == 0 {
            LinkMode::Cold
        } else {
            LinkMode::Incremental
        };
    }

    pub fn set_timing_total(&mut self, d: Duration) {
        self.timings.total_ms = Some(as_ms(d));
    }

    /// Record the fingerprinted inputs, deriving the derived counters from them
    /// so the two can never disagree.
    pub fn set_inputs(&mut self, inputs: Vec<InputFingerprint>) {
        self.counters.input_count = inputs.len() as u64;
        self.counters.bytes_read = inputs.iter().map(|i| i.file_size).sum();
        self.inputs = inputs;
    }

    pub fn to_json(&self) -> String {
        // Serialization of this struct cannot fail: every field is a plain data
        // type with an infallible Serialize impl.
        serde_json::to_string_pretty(self).expect("LinkRecord is always serializable")
    }

    /// Write the record to `path`, creating parent directories as needed.
    pub fn write_json(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, self.to_json())
    }

    /// Per-stage breakdown of the internal link, when there was one.
    ///
    /// Printed alongside the summary because the totals alone cannot say where
    /// a cache would help — and choosing what to cache from a total is how a
    /// parse cache came to be planned for a link parsing is not dominating.
    fn link_breakdown(&self) -> Option<String> {
        let total = self.timings.internal_link_ms?;
        let stage = |label: &str, value: Option<f64>| {
            value.map(|v| {
                let pct = if total > 0.0 { v / total * 100.0 } else { 0.0 };
                format!("\n    {label:<12}{v:6.1} ms {pct:5.1}%")
            })
        };
        let mut out = format!("\n  link: {total:.1} ms");
        for (label, value) in [
            ("read+parse", self.timings.link_read_and_parse_ms),
            ("resolve", self.timings.link_resolve_ms),
            ("dead-strip", self.timings.link_dead_strip_ms),
            ("  atoms", self.timings.link_atoms_ms),
            ("  liveness", self.timings.link_liveness_ms),
            ("  strip-build", self.timings.link_strip_build_ms),
            ("  digest", self.timings.link_digest_ms),
            ("    group", self.timings.link_group_ms),
            ("    traverse", self.timings.link_traverse_ms),
            ("prepare", self.timings.link_prepare_ms),
            ("  placements", self.timings.link_placements_ms),
            ("  personality", self.timings.link_personality_ms),
            ("  unwind-size", self.timings.link_unwind_size_ms),
            ("  commons", self.timings.link_commons_ms),
            ("accounting", self.timings.link_accounting_ms),
            ("address-table", self.timings.link_address_table_ms),
            ("address-diff", self.timings.link_address_diff_ms),
            ("  eh-frame", self.timings.link_eh_frame_ms),
            ("  tables", self.timings.link_tables_ms),
            ("  unwind-info", self.timings.link_unwind_ms),
            ("layout", self.timings.link_layout_ms),
            ("relocate", self.timings.link_relocate_ms),
            ("emit+sign", self.timings.link_emit_ms),
            ("write", self.timings.link_write_ms),
            // Last, and named for what they are rather than folded into the
            // stage they happen to run inside: these are the cache's price,
            // and a reader comparing them against the reuse counters above is
            // reading the trade the cache actually made.
            ("cache load", self.timings.link_cache_load_ms),
            ("cache plan", self.timings.link_cache_plan_ms),
            ("cache build", self.timings.link_cache_build_ms),
            ("cache store", self.timings.link_cache_store_ms),
        ] {
            if let Some(text) = stage(label, value) {
                out.push_str(&text);
            }
        }
        Some(out)
    }

    /// Record what dead-stripping removed, and whether its model held.
    pub fn set_dead_strip(&mut self, stripped_bytes: u64, revived: u64) {
        self.counters.stripped_bytes = Some(stripped_bytes);
        self.counters.revived_atoms = Some(revived);
    }

    /// The concise human-readable summary shown on a normal successful link.
    pub fn to_summary(&self) -> String {
        let mode = match self.mode {
            LinkMode::Delegated => "delegated",
            LinkMode::Cold => "cold",
            LinkMode::Cached => "cached",
            LinkMode::Incremental => "incremental",
            LinkMode::ExternalFallback => "external-fallback",
        };
        let elapsed = self.timings.total_ms.unwrap_or(0.0);
        let mut s = format!(
            "blinker:\n  mode: {mode}\n  elapsed: {elapsed:.0} ms\n  inputs: {}\n  bytes_read: {}",
            self.counters.input_count, self.counters.bytes_read
        );
        if let Some(reused) = self.counters.reused_inputs {
            let total = reused + self.counters.changed_inputs.unwrap_or(0);
            let share = match (
                self.counters.reused_relocations,
                self.counters.total_relocations,
            ) {
                (Some(done), Some(all)) if all > 0 => {
                    format!(", {:.0}% of relocations", done as f64 / all as f64 * 100.0)
                }
                _ => String::new(),
            };
            s.push_str(&format!("\n  reused: {reused}/{total} objects{share}"));
        }
        // Printed next to the reuse line, and deliberately: a hit rate averages
        // a failure away, and this is the failure. A contribution of a file
        // that did not change has no business moving, and every one that does
        // invalidates every relocation pointing at it.
        if let (Some(kept), Some(moved)) = (
            self.counters.contributions_retained,
            self.counters.contributions_moved,
        ) {
            let placed = kept + moved;
            if placed > 0 {
                s.push_str(&format!(
                    "\n  placement: {kept}/{placed} contributions kept their address"
                ));
                match self.counters.contributions_moved_unchanged {
                    Some(0) => s.push_str(", none of the movers were unchanged"),
                    Some(stale) => {
                        s.push_str(&format!(", {stale} of the movers were UNCHANGED inputs"))
                    }
                    None => {}
                }
            }
        }
        if let Some(stripped) = self.counters.stripped_bytes {
            s.push_str(&format!(
                "\n  dead-stripped: {} KB of __text",
                stripped / 1024
            ));
            if self.counters.revived_atoms.is_some_and(|n| n > 0) {
                s.push_str(&format!(
                    " ({} atoms revived)",
                    self.counters.revived_atoms.unwrap_or(0)
                ));
            }
        }
        if !self.unrecognized_arguments.is_empty() {
            s.push_str(&format!(
                "\n  unrecognized_arguments: {}",
                self.unrecognized_arguments.len()
            ));
        }
        if let Some(breakdown) = self.link_breakdown() {
            s.push_str(&breakdown);
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(record: &LinkRecord) -> serde_json::Value {
        serde_json::from_str(&record.to_json()).unwrap()
    }

    #[test]
    fn delegated_record_declares_its_mode_and_reason() {
        let json = parse(&LinkRecord::delegated());
        assert_eq!(json["mode"], "delegated");
        assert_eq!(json["fallback_reason"], "not_implemented");
        assert_eq!(json["schema_version"], schema::VERSION);
    }

    /// The key set is a contract. Consumers (tests, benchmarks, tooling) index
    /// these directly, so a field disappearing must fail loudly here.
    #[test]
    fn json_contains_every_contract_field() {
        let json = parse(&LinkRecord::delegated());
        for key in [
            "schema_version",
            "mode",
            "fallback_reason",
            "exit_code",
            "output_path",
            "arch",
            "deployment_target",
            "timings",
            "counters",
            "inputs",
            "unrecognized_arguments",
            "argv",
            "replay_argv",
        ] {
            assert!(json.get(key).is_some(), "missing contract field: {key}");
        }
    }

    #[test]
    fn unpopulated_fields_serialize_as_null_not_omitted() {
        // Stability of the key set is what lets consumers index without guards.
        let json = parse(&LinkRecord::delegated());
        assert!(json["output_path"].is_null());
        assert!(json["timings"]["total_ms"].is_null());
        assert!(json["counters"]["reused_inputs"].is_null());
    }

    #[test]
    fn counters_are_derived_from_inputs_so_they_cannot_disagree() {
        let mut record = LinkRecord::delegated();
        record.set_inputs(vec![
            InputFingerprint::for_test("a.o", 100),
            InputFingerprint::for_test("b.o", 250),
        ]);
        assert_eq!(record.counters.input_count, 2);
        assert_eq!(record.counters.bytes_read, 350);
    }

    #[test]
    fn timings_are_recorded_in_milliseconds() {
        let mut record = LinkRecord::delegated();
        record.set_timing_total(Duration::from_millis(1500));
        assert_eq!(record.timings.total_ms, Some(1500.0));
    }

    #[test]
    fn summary_reports_unrecognized_arguments_when_present() {
        let mut record = LinkRecord::delegated();
        assert!(!record.to_summary().contains("unrecognized"));
        record.unrecognized_arguments = vec!["-bogus".into()];
        assert!(record.to_summary().contains("unrecognized_arguments: 1"));
    }

    #[test]
    fn write_json_creates_missing_parent_directories() {
        let dir = blinker_test_support::Scratch::dir("diag").unwrap();
        let path = dir.join("nested/deeper/record.json");
        LinkRecord::delegated().write_json(&path).unwrap();
        assert!(path.exists());
    }
}

#[cfg(test)]
mod reuse_tests {
    use super::*;

    fn parse(record: &LinkRecord) -> serde_json::Value {
        serde_json::from_str(&record.to_json()).unwrap()
    }

    #[test]
    fn reuse_is_reported_in_the_counters_and_the_summary() {
        let mut record = LinkRecord::delegated();
        record.set_inputs(
            (0..47)
                .map(|n| InputFingerprint::for_test("o", n))
                .collect(),
        );
        record.set_reuse(47, 47, (900, 1000));
        assert_eq!(record.counters.reused_inputs, Some(47));
        assert_eq!(record.counters.changed_inputs, Some(0));
        assert!(record
            .to_summary()
            .contains("reused: 47/47 objects, 90% of relocations"));
    }

    /// The failure this exists to make visible: every input rebuilt, and a
    /// correct binary produced, so nothing else in the record differs.
    #[test]
    fn a_cache_that_reused_nothing_says_so_rather_than_staying_silent() {
        let mut record = LinkRecord::delegated();
        record.set_inputs(
            (0..47)
                .map(|n| InputFingerprint::for_test("o", n))
                .collect(),
        );
        record.set_reuse(0, 47, (0, 1000));
        assert!(record.to_summary().contains("reused: 0/47 objects"));
        assert_eq!(parse(&record)["counters"]["reused_inputs"], 0);
    }

    /// The mode is derived, so a record cannot report `incremental` while
    /// having reused nothing.
    #[test]
    fn the_mode_follows_the_counters() {
        let mut record = LinkRecord::delegated();
        record.set_reuse(0, 10, (0, 100));
        assert_eq!(parse(&record)["mode"], "cold");
        record.set_reuse(3, 10, (30, 100));
        assert_eq!(parse(&record)["mode"], "incremental");
    }

    /// A link with no cache leaves the field null, which is different from a
    /// cache that reused nothing.
    #[test]
    fn a_link_without_a_cache_reports_null_rather_than_zero() {
        let record = LinkRecord::delegated();
        assert!(parse(&record)["counters"]["reused_inputs"].is_null());
        assert!(!record.to_summary().contains("reused"));
    }
}

/// The body `scripts/relink.py` edits; see `LinkRecord::delegated`.
///
/// Deliberately arithmetic with no purpose: it must compile to real
/// instructions, be reachable, and mean nothing, so that perturbing it changes
/// the program's bytes and not its behaviour.
#[doc(hidden)]
#[inline(never)]
pub fn relink_seam(value: u64) -> u64 {
    let mut x = value ^ 0x9e37_79b9_7f4a_7c15;
    for i in 0..3u64 {
        x = x.wrapping_mul(0x2545_f491_4f6c_dd1d).rotate_left(17) ^ i;
    }
    x
}


// ---- injected by capture.py for the S0 frontend spike ----
//
// Every item carries `#[allow(missing_docs)]`: a real crate may `deny` lints
// that an injected fixture knows nothing about, and `grep-matcher` denies
// exactly this one. A fixture that cannot be injected into a strict crate can
// only ever measure lax ones.

/// A type whose layout the hot root depends on, so edit class 5 has something
/// to change.
#[allow(missing_docs, dead_code)]
#[derive(Clone, Copy)]
pub struct SpikeReading {
    pub value: u64,
    pub scale: u32,
}

#[allow(missing_docs, dead_code)]
impl SpikeReading {
    pub fn total(&self) -> u64 {
        self.value.wrapping_mul(self.scale as u64)
    }
}

/// Generic, instantiated at `u64` below; edit class 3 asks for a second one.
#[allow(missing_docs, dead_code)]
pub fn spike_convert<T: Into<u64> + Copy>(x: T) -> u64 {
    x.into().wrapping_add(1)
}

/// An existing callee, for edit class 2.
#[allow(missing_docs, dead_code)]
#[inline(never)]
pub fn spike_helper(x: u64) -> u64 {
    x.wrapping_mul(31).wrapping_add(7)
}

/// The hot root. `#[inline(never)]` because a replaceable boundary the
/// optimizer may erase is not one (V2 §10.3).
#[allow(missing_docs, dead_code)]
#[inline(never)]
pub fn spike_hot_root(reading: SpikeReading) -> u64 {
    reading.total().wrapping_mul(23).wrapping_add(4)
}

/// The same function with `#[inline]`, which rustc encodes into this crate's
/// metadata so a downstream crate may compile its own copy. The oracle's
/// control: editing *this* body must change a dependent's machine code, and
/// if it does not, the oracle cannot detect a downstream change at all and
/// nothing it says about the hot root means anything.
#[allow(missing_docs, dead_code)]
#[inline]
pub fn spike_inline_twin(reading: SpikeReading) -> u64 {
    reading.total().wrapping_mul(3).wrapping_add(1)
}

/// Reachable from the crate's roots, so whole-crate collection sees it.
#[allow(missing_docs, dead_code)]
#[unsafe(no_mangle)]
pub extern "C" fn spike_entry(value: u64) -> u64 {
    let reading = SpikeReading { value, scale: 3 };
    spike_hot_root(reading).wrapping_add(spike_helper(value)).wrapping_add(spike_convert(value))
}
