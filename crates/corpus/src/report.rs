//! Rendering the inventory and baseline as human-readable reports.

use crate::inventory::Inventory;

/// One fixture's timing comparison.
pub struct BaselineRow {
    pub tag: String,
    pub system_ms: f64,
    pub blinker_ms: f64,
}

/// Median of a sample, sorting in place.
///
/// Median rather than mean: build timings have a long right tail (scheduler
/// noise, filesystem cache misses) that drags a mean around without saying
/// anything about typical behaviour.
pub fn median(samples: &mut [f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("timings are never NaN"));
    let mid = samples.len() / 2;
    if samples.len().is_multiple_of(2) {
        (samples[mid - 1] + samples[mid]) / 2.0
    } else {
        samples[mid]
    }
}

pub fn print_inventory(inventory: &Inventory) {
    let mut link_ms = inventory.link_ms.clone();

    println!("=== Argument inventory ===\n");
    println!("Records analysed:  {}", inventory.record_count);
    println!("Inputs seen:       {}", inventory.total_inputs);
    println!(
        "Bytes read:        {:.1} MB",
        inventory.total_bytes as f64 / 1_048_576.0
    );
    if !link_ms.is_empty() {
        let link = median(&mut link_ms);
        println!("Median link time:  {link:.0} ms (delegated to the system linker)");
        let mut overhead = inventory.overhead_ms.clone();
        if !overhead.is_empty() {
            let own = median(&mut overhead);
            println!(
                "blinker overhead:  {own:.2} ms per link ({:.1}% of the link step)",
                own / link * 100.0
            );
        }
    }

    println!("\n--- Categories ---\n");
    println!(
        "{:<24} {:>8} {:>10}   SPELLINGS",
        "CATEGORY", "SEEN", "DISTINCT"
    );
    for (category, spellings) in &inventory.by_category {
        let total: usize = spellings.values().map(|o| o.count).sum();
        // Path-valued categories collapse to one placeholder spelling, so
        // listing them adds nothing.
        let sample = if spellings.len() == 1 && spellings.keys().next().unwrap().starts_with('<') {
            String::new()
        } else {
            let mut names: Vec<&str> = spellings.keys().map(String::as_str).collect();
            names.sort();
            let shown = names.iter().take(6).cloned().collect::<Vec<_>>().join(" ");
            if names.len() > 6 {
                format!("{shown} … (+{} more)", names.len() - 6)
            } else {
                shown
            }
        };
        println!(
            "{category:<24} {total:>8} {:>10}   {sample}",
            spellings.len()
        );
    }

    print_table_coverage(inventory);

    println!("\n--- Unmodelled arguments ---\n");
    if inventory.unrecognized.is_empty() {
        println!("None. Every argument in the corpus classified.");
    } else {
        println!("{:<40} {:>6}   FIXTURES", "ARGUMENT", "SEEN");
        for (arg, observation) in &inventory.unrecognized {
            let sources: Vec<&str> = observation.sources.iter().map(String::as_str).collect();
            println!(
                "{arg:<40} {:>6}   {}",
                observation.count,
                sources.join(", ")
            );
        }
        println!(
            "\n{} spelling(s) need modelling in the `arguments` crate.",
            inventory.unrecognized.len()
        );
    }
    println!();
}

/// How much of the known `ld64` option table this corpus actually exercises.
///
/// The gap between "known" and "observed" is the point: arity for all 238
/// options comes from the reference table, so an option appearing for the first
/// time in some future project is already parsed correctly. The corpus tells us
/// which ones matter in practice, not which ones exist.
fn print_table_coverage(inventory: &Inventory) {
    use blinker_arguments::reference::LD64_OPTIONS;

    let observed: std::collections::BTreeSet<&str> = inventory
        .by_category
        .values()
        .flat_map(|spellings| spellings.keys())
        .map(|s| s.split_whitespace().next().unwrap_or(s))
        .collect();

    let known_observed: Vec<&str> = LD64_OPTIONS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| observed.contains(name))
        .collect();

    let multi_arg = LD64_OPTIONS.iter().filter(|(_, arity)| *arity > 1).count();

    println!("\n--- Option table coverage ---\n");
    println!(
        "Known ld64 options:   {} ({multi_arg} take more than one argument)",
        LD64_OPTIONS.len()
    );
    println!(
        "Observed in corpus:   {} — {}",
        known_observed.len(),
        known_observed.join(" ")
    );
    println!(
        "\nArity for all {} is known from the reference table, so an option\n\
         appearing for the first time in a future project is already parsed\n\
         correctly. The corpus shows which are used, not which exist.",
        LD64_OPTIONS.len()
    );
}

pub fn print_baseline(rows: &[BaselineRow]) {
    if rows.is_empty() {
        println!("No fixtures were timed.");
        return;
    }

    println!("=== Baseline timing ===\n");
    println!(
        "Full clean `cargo build` wall time, median of the runs. blinker is in\n\
         delegating mode, so the difference is the cost of recording and\n\
         forwarding — not of linking.\n"
    );
    println!(
        "{:<14} {:>12} {:>12} {:>12}",
        "FIXTURE", "SYSTEM (ms)", "BLINKER (ms)", "OVERHEAD"
    );
    for row in rows {
        let overhead = (row.blinker_ms / row.system_ms - 1.0) * 100.0;
        println!(
            "{:<14} {:>12.0} {:>12.0} {:>11.1}%",
            row.tag, row.system_ms, row.blinker_ms, overhead
        );
    }

    let mut system: Vec<f64> = rows.iter().map(|r| r.system_ms).collect();
    let mut blinker: Vec<f64> = rows.iter().map(|r| r.blinker_ms).collect();
    println!(
        "\n{:<14} {:>12.0} {:>12.0}",
        "median",
        median(&mut system),
        median(&mut blinker)
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_an_odd_sample_is_the_middle_value() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), 2.0);
    }

    #[test]
    fn median_of_an_even_sample_averages_the_middle_pair() {
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
    }

    #[test]
    fn median_of_a_single_sample_is_that_sample() {
        assert_eq!(median(&mut [7.5]), 7.5);
    }

    #[test]
    fn median_of_an_empty_sample_is_zero_rather_than_a_panic() {
        assert_eq!(median(&mut []), 0.0);
    }

    /// The median must not be swayed by a single slow run — which is the whole
    /// reason for preferring it over the mean here.
    #[test]
    fn median_resists_an_outlier() {
        assert_eq!(median(&mut [10.0, 11.0, 12.0, 13.0, 5000.0]), 12.0);
    }
}
