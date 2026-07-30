//! The `ld64` option table: every option and, critically, its **arity**.
//!
//! # Why this is a table rather than discovered case by case
//!
//! Knowing an option *exists* is the easy half. The half that causes silent
//! corruption is knowing how many arguments it **consumes**. Get that wrong and
//! the linker's own arguments get misread as input files — `-L /some/dir`
//! parsed as a flag plus an object file, `-sectcreate seg sect file` parsed as
//! a flag plus three object files. Nothing errors; the link is just wrong.
//!
//! Discovering arity empirically, one project at a time, means each of those
//! bugs waits for a project that happens to trigger it. So the table is seeded
//! from authoritative sources instead:
//!
//! - **Apple's `man ld`** on the host toolchain, which documents each option
//!   with its argument names (`-alias symbol_name alternate_symbol_name`),
//!   making arity mechanically extractable.
//! - **LLD's `lld/MachO/Options.td`**, whose `Separate` / `Joined` / `MultiArg`
//!   declarations give the same information in machine-readable form and cover
//!   options Apple's page omits.
//!
//! The recorded corpus then serves its proper purpose: telling us which subset
//! of this table real Rust builds actually use, and how often — rather than
//! being the mechanism by which we discover the table exists.
//!
//! Regenerate after a toolchain update with `scripts/extract-ld-options.sh`.

/// Number of following arguments an option consumes.
pub type Arity = u8;

/// Option name → arity, sorted by name.
///
/// Sorted so [`arity_of`] can binary-search it.
// Options below marked (dual) accept their value either attached (`-L/usr/lib`)
// or as the next argument (`-L /usr/lib`). `man ld` documents only the attached
// spelling, so the separate arity comes from LLD's Options.td, which declares
// them as both Separate and Joined. rustc emits both: a build script's
// `cargo:rustc-link-search=` arrives separate, other paths arrive attached.
pub static LD64_OPTIONS: &[(&str, Arity)] = &[
    ("-A", 1),
    ("-F", 1),
    ("-L", 1),
    ("-ObjC", 0),
    ("-S", 0),
    ("-U", 1),
    ("-Y", 1),
    ("-add_ast_path", 1),
    ("-add_empty_section", 2),
    ("-add_mergeable_debug_hook", 0),
    ("-adhoc_codesign", 0),
    ("-alias", 2),
    ("-alias_list", 1),
    ("-all_load", 0),
    ("-allow_heap_execute", 0),
    ("-allow_stack_execute", 0),
    ("-allow_sub_type_mismatches", 0),
    ("-allowable_client", 1),
    ("-application_extension", 0),
    ("-arch", 1),
    ("-arch_errors_fatal", 0),
    ("-arch_multiple", 0),
    ("-assert-weak-lx", 0),
    ("-assert_weak_library", 1),
    ("-bind_at_load", 0),
    ("-bitcode_bundle", 0),
    ("-bitcode_hide_symbols", 0),
    ("-bitcode_symbol_map", 1),
    ("-bundle", 0),
    ("-bundle_loader", 1),
    ("-cache_path_lto", 1),
    ("-client_name", 1),
    ("-commons", 1),
    ("-compatibility_version", 1),
    ("-const_selrefs", 0),
    ("-current_version", 1),
    ("-data_const", 0),
    ("-dead_strip", 0),
    ("-dead_strip_dylibs", 0),
    ("-debug_variant", 0),
    ("-delay-lx", 0),
    ("-delay_library", 1),
    ("-dependency_info", 1),
    ("-deployment_target_mismatches", 1),
    ("-dirty_data_list", 1),
    ("-dot", 1),
    ("-dtrace", 1),
    ("-dyld_env", 1),
    ("-dylinker", 0),
    ("-dylinker_install_name", 1),
    ("-dynamic", 0),
    ("-e", 1),
    ("-execute", 0),
    ("-export_dynamic", 0),
    ("-exported_symbol", 1),
    ("-exported_symbols_list", 1),
    ("-fatal_warnings", 0),
    ("-filelist", 1),
    ("-final_output", 1),
    ("-fixup_chains_section", 0),
    ("-fixup_chains_section_vm", 0),
    ("-flat_namespace", 0),
    ("-force_cpusubtype_ALL", 0),
    ("-force_flat_namespace", 0),
    ("-force_load", 1),
    ("-framework", 1),
    ("-fvmlib", 0),
    ("-headerpad", 1),
    ("-headerpad_max_install_names", 0),
    ("-hidden-lx", 0),
    ("-image_base", 1),
    ("-image_suffix", 1),
    ("-init", 1),
    ("-install_name", 1),
    ("-interposable", 0),
    ("-interposable_list", 1),
    ("-ios_version_min", 1),
    ("-keep_private_externs", 0),
    ("-keep_relocs", 0),
    ("-l", 1),
    ("-lazy_framework", 1),
    ("-lazy_library", 1),
    ("-ld_classic", 0),
    ("-ld_new", 0),
    ("-load_hidden", 1),
    ("-lto_library", 1),
    ("-macos_version_min", 1),
    ("-macosx_version_min", 1),
    ("-make_mergeable", 0),
    ("-map", 1),
    ("-max_default_common_align", 1),
    ("-max_relative_cache_size_lto", 1),
    ("-mcpu", 1),
    ("-merge-lx", 0),
    ("-merge_library", 1),
    ("-merge_zero_fill_sections", 0),
    ("-mllvm", 1),
    ("-move_to_ro_segment", 2),
    ("-move_to_rw_segment", 2),
    ("-multi_module", 0),
    ("-multiply_defined", 1),
    ("-multiply_defined_unused", 1),
    ("-needed-lx", 0),
    ("-needed_framework", 1),
    ("-needed_library", 1),
    ("-no_adhoc_codesign", 0),
    ("-no_application_extension", 0),
    ("-no_arch_warnings", 0),
    ("-no_branch_islands", 0),
    ("-no_const_selrefs", 0),
    ("-no_data_const", 0),
    ("-no_dead_strip_inits_and_terms", 0),
    ("-no_deduplicate", 0),
    ("-no_dynamic_access", 0),
    ("-no_eh_labels", 0),
    ("-no_exported_symbols", 0),
    ("-no_function_starts", 0),
    ("-no_implicit_dylibs", 0),
    ("-no_inits", 0),
    ("-no_merged_libraries_hook", 0),
    ("-no_objc_category_merging", 0),
    ("-no_objc_relative_method_lists", 0),
    ("-no_order_inits", 0),
    ("-no_pie", 0),
    ("-no_uuid", 0),
    ("-no_warn_duplicate_libraries", 0),
    ("-no_warn_inits", 0),
    ("-no_warn_reduced_section_align", 0),
    ("-no_warn_unused_dylibs", 0),
    ("-no_weak_exports", 0),
    ("-no_weak_imports", 0),
    ("-no_zero_fill_sections", 0),
    ("-noall_load", 0),
    ("-nofixprebinding", 0),
    ("-nomultidefs", 0),
    ("-non_global_symbols_no_strip_list", 1),
    ("-non_global_symbols_strip_list", 1),
    ("-noprebind", 0),
    ("-noprebind_all_twolevel_modules", 0),
    ("-noseglinkedit", 0),
    ("-not_for_dyld_shared_cache", 0),
    ("-o", 1),
    ("-objc_relative_method_lists", 0),
    ("-object_path_lto", 1),
    ("-order_file", 1),
    ("-order_file_statistics", 0),
    ("-page_align_data_atoms", 0),
    ("-pagezero_size", 1),
    ("-pie", 0),
    ("-platform_version", 3),
    ("-prebind", 0),
    ("-prebind_all_twolevel_modules", 0),
    ("-prebind_allow_overlap", 0),
    ("-preload", 0),
    ("-print_statistics", 0),
    ("-private_bundle", 0),
    ("-prune_after_lto", 1),
    ("-prune_interval_lto", 1),
    ("-random_uuid", 0),
    ("-read_only_relocs", 1),
    ("-read_only_stubs", 0),
    ("-reexport-lx", 0),
    ("-reexport_library", 1),
    ("-reexported_symbols_list", 1),
    ("-rename_section", 4),
    ("-rename_segment", 2),
    ("-reproducible", 0),
    ("-root_safe", 0),
    ("-rpath", 1),
    ("-run_init_lazily", 0),
    ("-sdk_version", 1),
    ("-search_dylibs_first", 0),
    ("-search_in_sparse_frameworks", 0),
    ("-search_paths_first", 0),
    ("-sect_diff_relocs", 1),
    ("-sectalign", 3),
    ("-sectcreate", 3),
    ("-section_order", 2),
    ("-sectobjectsymbols", 2),
    ("-sectorder", 3),
    ("-sectorder_detail", 0),
    ("-seg1addr", 1),
    ("-seg_addr_table", 1),
    ("-seg_addr_table_filename", 1),
    ("-seg_page_size", 2),
    ("-segaddr", 2),
    ("-segalign", 1),
    ("-segcreate", 3),
    ("-seglinkedit", 0),
    ("-segment_order", 1),
    ("-segprot", 3),
    ("-segs_read_only_addr", 1),
    ("-segs_read_write_addr", 1),
    ("-setuid_safe", 0),
    ("-single_module", 0),
    ("-slow_stubs", 0),
    ("-stack_addr", 1),
    ("-stack_size", 1),
    ("-static", 0),
    ("-sub_library", 1),
    ("-sub_umbrella", 1),
    ("-syslibroot", 1),
    ("-trace_implicit_libraries", 0),
    ("-trace_implicit_library", 1),
    ("-trace_symbol_layout", 0),
    ("-trace_symbol_layout_file", 1),
    ("-tvos_version_min", 1),
    ("-twolevel_namespace", 0),
    ("-twolevel_namespace_hints", 0),
    ("-u", 1),
    ("-umbrella", 1),
    ("-unaligned_pointers", 1),
    ("-undefined", 1),
    ("-unexported_symbol", 1),
    ("-unexported_symbols_list", 1),
    ("-upward-lx", 0),
    ("-upward_framework", 1),
    ("-upward_library", 1),
    ("-v", 0),
    ("-verbose_branch_islands", 0),
    ("-verbose_deduplicate", 0),
    ("-version_details", 0),
    ("-w", 0),
    ("-warn_commons", 0),
    ("-warn_compact_unwind", 0),
    ("-warn_duplicate_libraries", 0),
    ("-warn_stabs", 0),
    ("-warn_unused_dylibs", 0),
    ("-warn_weak_exports", 0),
    ("-watchos_version_min", 1),
    ("-weak-lx", 0),
    ("-weak_framework", 1),
    ("-weak_library", 1),
    ("-weak_reference_mismatches", 1),
    ("-why_live", 1),
    ("-why_load", 0),
    ("-x", 0),
    ("-ysymbol", 0),
];

/// Prefixes whose value may be attached directly to the flag (`-L/usr/lib`).
///
/// Each also has a separate-argument spelling (`-L /usr/lib`); rustc emits both
/// depending on whether the path came from a build script. Longest match wins,
/// so `-weak-l` is tried before `-l`.
pub static JOINED_PREFIXES: &[&str] = &[
    "-weak-l",
    "-needed-l",
    "-reexport-l",
    "-hidden-l",
    "-upward-l",
    "-l",
    "-L",
    "-F",
    "-O",
];

/// Arity of `name`, or `None` if it is not a known `ld64` option.
pub fn arity_of(name: &str) -> Option<Arity> {
    LD64_OPTIONS
        .binary_search_by(|(candidate, _)| (*candidate).cmp(name))
        .ok()
        .map(|index| LD64_OPTIONS[index].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_so_binary_search_is_valid() {
        let mut sorted = LD64_OPTIONS.to_vec();
        sorted.sort_by_key(|(name, _)| *name);
        assert_eq!(
            sorted,
            LD64_OPTIONS.to_vec(),
            "LD64_OPTIONS must stay sorted"
        );
    }

    #[test]
    fn table_has_no_duplicate_entries() {
        let mut names: Vec<&str> = LD64_OPTIONS.iter().map(|(n, _)| *n).collect();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate option in LD64_OPTIONS");
    }

    #[test]
    fn looks_up_flag_options() {
        assert_eq!(arity_of("-dead_strip"), Some(0));
        assert_eq!(arity_of("-all_load"), Some(0));
    }

    #[test]
    fn looks_up_single_value_options() {
        assert_eq!(arity_of("-arch"), Some(1));
        assert_eq!(arity_of("-syslibroot"), Some(1));
        assert_eq!(arity_of("-rpath"), Some(1));
    }

    /// The options this table exists for: getting these wrong silently eats
    /// the following arguments as if they were input files.
    #[test]
    fn looks_up_multi_argument_options() {
        assert_eq!(arity_of("-sectcreate"), Some(3));
        assert_eq!(arity_of("-platform_version"), Some(3));
        assert_eq!(arity_of("-segprot"), Some(3));
        assert_eq!(arity_of("-alias"), Some(2));
        assert_eq!(arity_of("-rename_section"), Some(4));
    }

    #[test]
    fn unknown_options_are_not_invented() {
        assert_eq!(arity_of("-not_a_real_ld_option"), None);
        assert_eq!(arity_of(""), None);
    }

    #[test]
    fn table_covers_the_options_a_rust_link_actually_uses() {
        for option in [
            "-arch",
            "-o",
            "-dead_strip",
            "-syslibroot",
            "-platform_version",
        ] {
            assert!(arity_of(option).is_some(), "{option} missing from table");
        }
    }
}
