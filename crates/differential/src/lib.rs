//! Differential testing against the system linker.
//!
//! # The problem this solves
//!
//! Every test blinker had before this one checked the output against *my
//! understanding* of the Mach-O format. That is worth something — it catches
//! arithmetic slips — but it cannot catch a misunderstanding, and a
//! misunderstanding is the likelier failure. The `system_tools` tests improved
//! on this by asking `otool` and `nm` to read the output back, but those tools
//! are lenient: they will happily describe an image dyld would refuse.
//!
//! The strongest check available short of executing the program is: link the
//! same inputs with `ld64` and with blinker, and compare what came out.
//! Anywhere they disagree is either a blinker bug or a deliberate, documented
//! difference. There is no third category.
//!
//! # Shape
//!
//! - [`reference`] compiles a case with `cc` and links it with the system
//!   toolchain, capturing both the resulting image and the exact argument
//!   vector the driver handed to `ld`.
//! - [`summary`] reduces a linked image to comparable facts.
//! - [`compare`] diffs two summaries, grading each disagreement by
//!   [`Property`] so a test can assert on what blinker claims to match *today*
//!   without failing on what it does not yet implement.
//!
//! # Using it
//!
//! ```no_run
//! use blinker_differential::{compare, reference, summary, Property};
//!
//! let case = reference::LinkCase::new("trivial")
//!     .source("main.c", "int main(void) { return 0; }");
//! let built = reference::build(&case).unwrap();
//! let ld64 = summary::summarize_file(&built.image).unwrap();
//!
//! // ... link the same `built.link_argv` with blinker, then:
//! # let blinker = ld64.clone();
//! let report = compare::compare(&ld64, &blinker);
//! report.require(&[Property::Identity, Property::SegmentSet]);
//! ```

pub mod compare;
pub mod reference;
pub mod summary;

pub use compare::{compare, Difference, Property, Report};
pub use reference::{build, LinkCase, ReferenceLink};
pub use summary::{summarize, summarize_file, ImageSummary};
