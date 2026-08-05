//! A C++ program's global constructors have to actually run.
//!
//! This is an *execution* test rather than a byte or section comparison, and
//! deliberately so. The bug it guards against produced a program that linked
//! without a warning, loaded, ran, and returned the right exit code — with none
//! of its globals constructed. `__mod_init_func` was laid out correctly and
//! held the right pointers; only its section *type* was wrong, `S_REGULAR`
//! instead of `S_MOD_INIT_FUNC_POINTERS`, and dyld finds initialisers by type.
//! Nothing short of running the program would have caught it.

use std::process::Command;

use blinker_test_support::{blinker, no_daemon, scratch::Scratch};

/// Compile `source` to an object, or report why the machine cannot.
fn compile(scratch: &Scratch, name: &str, source: &str) -> Option<std::path::PathBuf> {
    let file = scratch.join(format!("{name}.cc"));
    let object = scratch.join(format!("{name}.o"));
    std::fs::write(&file, source).expect("write the source");
    let done = Command::new("cc")
        .arg("-c")
        .arg(&file)
        .arg("-o")
        .arg(&object)
        .output()
        .ok()?;
    done.status.success().then_some(object)
}

const A_GLOBAL_WITH_A_CONSTRUCTOR: &str = r#"
#include <cstdio>
struct Global {
    Global() { printf("constructed\n"); }
    ~Global() { printf("destroyed\n"); }
};
static Global the_global;
int main(void) { printf("ran\n"); return 0; }
"#;

#[test]
fn a_cpp_global_is_constructed_and_destroyed() {
    let scratch = Scratch::dir("static-init").expect("scratch");
    let Some(object) = compile(&scratch, "global", A_GLOBAL_WITH_A_CONSTRUCTOR) else {
        return; // No C++ compiler here; the gate's platform check covers this.
    };
    let output = scratch.join("program");
    let sdk = blinker_link::sdk_root().expect("an SDK to link against");

    let linked = no_daemon(&mut blinker())
        .args(["-arch", "arm64"])
        .args(["-platform_version", "macos", "26.0.0", "26.5"])
        .arg("-syslibroot")
        .arg(&sdk)
        .arg("-o")
        .arg(&output)
        .arg(&object)
        .args(["-lc++", "-lSystem"])
        .output()
        .expect("blinker should run");
    assert!(
        linked.status.success(),
        "the link failed: {}",
        String::from_utf8_lossy(&linked.stderr)
    );

    let ran = Command::new(&output)
        .output()
        .expect("the program should run");
    let said = String::from_utf8_lossy(&ran.stdout);
    assert!(
        said.contains("constructed"),
        "the global's constructor never ran — output was {said:?}"
    );
    assert!(
        said.contains("destroyed"),
        "the global's destructor never ran — output was {said:?}"
    );
    // Order matters as much as presence: a constructor that runs after `main`
    // is as wrong as one that does not run.
    let constructed = said.find("constructed").expect("checked above");
    let ran_main = said.find("ran").expect("main should have printed");
    assert!(
        constructed < ran_main,
        "the constructor should run before main — output was {said:?}"
    );
}

/// The same program through the system linker, so the test is comparing
/// against the platform rather than against its own expectations.
#[test]
fn the_system_linker_agrees_about_what_should_happen() {
    let scratch = Scratch::dir("static-init-ld64").expect("scratch");
    let Some(object) = compile(&scratch, "global", A_GLOBAL_WITH_A_CONSTRUCTOR) else {
        return;
    };
    let output = scratch.join("program");
    let built = Command::new("cc")
        .arg(&object)
        .arg("-o")
        .arg(&output)
        .arg("-lc++")
        .output()
        .expect("cc should run");
    assert!(built.status.success());
    let ran = Command::new(&output)
        .output()
        .expect("the program should run");
    let said = String::from_utf8_lossy(&ran.stdout);
    assert!(said.contains("constructed") && said.contains("destroyed"));
}

/// `___dso_handle` is the linker's to define. Nothing else does, and every C++
/// program with a static destructor references it through `__cxa_atexit`.
#[test]
fn a_linker_defined_dso_handle_needs_no_library() {
    let scratch = Scratch::dir("dso-handle").expect("scratch");
    let Some(object) = compile(&scratch, "handle", A_GLOBAL_WITH_A_CONSTRUCTOR) else {
        return;
    };
    let listed = Command::new("nm")
        .arg(&object)
        .output()
        .expect("nm should run");
    assert!(
        String::from_utf8_lossy(&listed.stdout).contains("U ___dso_handle"),
        "the fixture must actually reference it, or it proves nothing"
    );

    let output = scratch.join("program");
    let sdk = blinker_link::sdk_root().expect("an SDK to link against");
    let linked = no_daemon(&mut blinker())
        .args(["-arch", "arm64"])
        .args(["-platform_version", "macos", "26.0.0", "26.5"])
        .arg("-syslibroot")
        .arg(&sdk)
        .arg("-o")
        .arg(&output)
        .arg(&object)
        .args(["-lc++", "-lSystem"])
        .output()
        .expect("blinker should run");
    assert!(
        linked.status.success(),
        "___dso_handle should be linker-defined, not reported undefined: {}",
        String::from_utf8_lossy(&linked.stderr)
    );
}
