//! Report how much `__text` a link can reach. See `reachability`.
fn main() {
    let objects: Vec<std::path::PathBuf> = std::env::args()
        .skip(1)
        .filter(|a| a.ends_with(".o") || a.ends_with(".rlib") || a.ends_with(".a"))
        .map(Into::into)
        .collect();
    let report = blinker_link::reachability_report(&blinker_link::LinkRequest::new(objects))
        .expect("the inputs parse");
    println!(
        "  atoms {}/{} live   __text {:.0}K live of {:.0}K   would strip {:.0}K",
        report.live_atoms,
        report.total_atoms,
        report.live_bytes as f64 / 1024.0,
        report.total_bytes as f64 / 1024.0,
        report.dead_bytes() as f64 / 1024.0,
    );
    println!(
        "  of which whole dead sections: {:.0}K  (droppable without atom layout)",
        report.fully_dead_section_bytes as f64 / 1024.0,
    );
}
