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
    pub total_ms: Option<f64>,
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
    pub bytes_read: u64,
    pub bytes_written: Option<u64>,
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
        if !self.unrecognized_arguments.is_empty() {
            s.push_str(&format!(
                "\n  unrecognized_arguments: {}",
                self.unrecognized_arguments.len()
            ));
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
        let dir = std::env::temp_dir().join(format!("blinker-diag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested/deeper/record.json");
        LinkRecord::delegated().write_json(&path).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
