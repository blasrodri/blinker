//! Aggregating recorded invocations into an argument inventory.
//!
//! The inventory answers the question M0 exists to answer: *what does rustc
//! actually pass a linker, across the range of project shapes people build?*
//! Its most important output is the list of arguments the classifier could not
//! model — that list is the input to M1's scope.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use blinker_arguments::ParsedInvocation;

/// One argument spelling and how often it was seen.
#[derive(Debug, Default)]
pub struct Observation {
    pub count: usize,
    /// Which recorded links contained it.
    pub sources: BTreeSet<String>,
}

/// Aggregated view of a directory of records.
#[derive(Debug, Default)]
pub struct Inventory {
    pub record_count: usize,
    /// Category → spelling → observation.
    ///
    /// Path-valued categories collapse to a placeholder spelling: recording
    /// every distinct `.o` path would bury the flags we actually care about
    /// under thousands of unique temp filenames.
    pub by_category: BTreeMap<String, BTreeMap<String, Observation>>,
    /// Arguments that did not classify. The reason this tool exists.
    pub unrecognized: BTreeMap<String, Observation>,
    /// Total inputs and bytes across all records, for scale context.
    pub total_inputs: u64,
    pub total_bytes: u64,
    /// Per-record link timings reported by blinker itself.
    pub link_ms: Vec<f64>,
    /// blinker's own cost per link: total time minus the delegated link.
    ///
    /// This is the number that matters. Whole-build wall time cannot resolve
    /// it — compile time is an order of magnitude larger and its variance
    /// alone exceeds the entire linker step.
    pub overhead_ms: Vec<f64>,
}

/// Categories whose values are file paths, and so are not worth enumerating.
const PATH_CATEGORIES: &[&str] = &[
    "object_file",
    "archive",
    "rlib",
    "dynamic_library",
    "output",
    "library_search_path",
    "framework_search_path",
];

impl Inventory {
    /// Build an inventory from every `*.json` record in `dir`.
    pub fn from_records_dir(dir: &Path) -> Result<Self, String> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| format!("cannot read records dir {}: {e}", dir.display()))?;

        let mut inventory = Inventory::default();
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let label = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let json: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
            inventory.absorb(&label, &json);
        }

        if inventory.record_count == 0 {
            return Err(format!("no records found in {}", dir.display()));
        }
        Ok(inventory)
    }

    fn absorb(&mut self, label: &str, json: &serde_json::Value) {
        self.record_count += 1;

        if let Some(inputs) = json["counters"]["input_count"].as_u64() {
            self.total_inputs += inputs;
        }
        if let Some(bytes) = json["counters"]["bytes_read"].as_u64() {
            self.total_bytes += bytes;
        }
        if let Some(ms) = json["timings"]["fallback_exec_ms"].as_f64() {
            self.link_ms.push(ms);
            if let Some(total) = json["timings"]["total_ms"].as_f64() {
                self.overhead_ms.push(total - ms);
            }
        }

        let Some(argv) = json["argv"].as_array() else {
            return;
        };
        let argv: Vec<String> = argv
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();

        // Re-classify from the recorded argv rather than trusting a summary
        // field, so the inventory always reflects the current classifier.
        let parsed = ParsedInvocation::parse(argv);
        for (index, arg) in &parsed.args {
            let category = arg.category().to_string();
            let spelling = if PATH_CATEGORIES.contains(&category.as_str()) {
                format!("<{category}>")
            } else {
                match arg {
                    // Report the individual ld64 option rather than the whole
                    // `-Wl,a,b` argument it arrived in: several distinct
                    // options share one argv element, and the point of the
                    // inventory is to enumerate the options.
                    blinker_arguments::LinkerArg::LinkerFlag(flag) => flag.clone(),
                    blinker_arguments::LinkerArg::KnownUnmodelled(text) => text.clone(),
                    _ => parsed.argv[*index].clone(),
                }
            };

            let observation = self
                .by_category
                .entry(category)
                .or_default()
                .entry(spelling)
                .or_default();
            observation.count += 1;
            observation.sources.insert(label.to_string());
        }

        for arg in parsed.unrecognized() {
            let observation = self.unrecognized.entry(arg.to_string()).or_default();
            observation.count += 1;
            observation.sources.insert(label.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(argv: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "argv": argv,
            "counters": { "input_count": 2, "bytes_read": 100 },
            "timings": { "fallback_exec_ms": 12.5 },
        })
    }

    #[test]
    fn counts_spellings_and_tracks_their_sources() {
        let mut inv = Inventory::default();
        inv.absorb("alpha", &record(&["-lSystem", "-nodefaultlibs"]));
        inv.absorb("beta", &record(&["-lSystem"]));

        let libs = &inv.by_category["library"];
        assert_eq!(libs["-lSystem"].count, 2);
        assert_eq!(
            libs["-lSystem"].sources,
            ["alpha".to_string(), "beta".to_string()].into()
        );
        assert_eq!(inv.record_count, 2);
    }

    /// Enumerating every distinct object-file path would bury the flags that
    /// actually matter under thousands of unique temp names.
    #[test]
    fn collapses_path_valued_categories_to_a_placeholder() {
        let mut inv = Inventory::default();
        inv.absorb("alpha", &record(&["/tmp/a.o", "/tmp/b.o", "/tmp/c.rlib"]));

        assert_eq!(inv.by_category["object_file"].len(), 1);
        assert_eq!(inv.by_category["object_file"]["<object_file>"].count, 2);
        assert_eq!(inv.by_category["rlib"]["<rlib>"].count, 1);
    }

    #[test]
    fn accumulates_scale_and_timing_context() {
        let mut inv = Inventory::default();
        inv.absorb("alpha", &record(&["-lSystem"]));
        inv.absorb("beta", &record(&["-lSystem"]));

        assert_eq!(inv.total_inputs, 4);
        assert_eq!(inv.total_bytes, 200);
        assert_eq!(inv.link_ms, vec![12.5, 12.5]);
    }

    #[test]
    fn surfaces_unrecognized_arguments_with_their_sources() {
        let mut inv = Inventory::default();
        inv.absorb("alpha", &record(&["-bogus-flag"]));
        assert_eq!(inv.unrecognized["-bogus-flag"].count, 1);
        assert!(inv.unrecognized["-bogus-flag"].sources.contains("alpha"));
    }

    #[test]
    fn splits_wl_payloads_so_each_ld64_option_is_inventoried_separately() {
        // A `-Wl,-a,-b` argument represents two distinct ld64 options; counting
        // it as one spelling would understate what we must support.
        let mut inv = Inventory::default();
        inv.absorb("alpha", &record(&["-Wl,-dead_strip,-no_pie"]));
        assert_eq!(inv.by_category["linker_flag"].len(), 2);
    }

    #[test]
    fn a_record_without_argv_is_skipped_rather_than_failing() {
        let mut inv = Inventory::default();
        inv.absorb(
            "alpha",
            &serde_json::json!({ "counters": {}, "timings": {} }),
        );
        assert_eq!(inv.record_count, 1);
        assert!(inv.by_category.is_empty());
    }

    #[test]
    fn an_empty_directory_is_an_error_not_an_empty_report() {
        // Silently reporting "0 arguments observed" would look like a clean
        // result rather than a missing input.
        let dir = blinker_test_support::Scratch::dir("inv").unwrap();
        assert!(Inventory::from_records_dir(dir.path()).is_err());
    }
}
