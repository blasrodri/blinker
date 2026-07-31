//! Link object files named on the command line, for inspection by hand.
//!
//! ```text
//! cargo run -p blinker-link --example link_files -- out a.o b.o
//! ./out; echo $?
//! ```

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // `-dead_strip` anywhere on the line, as `ld` spells it.
    let dead_strip = args.iter().any(|a| a == "-dead_strip");
    args.retain(|a| a != "-dead_strip");
    let Some((output, objects)) = args.split_first() else {
        eprintln!("usage: link_files <output> <object>...");
        std::process::exit(2);
    };

    let request = blinker_link::LinkRequest::new(objects.iter().map(Into::into).collect())
        .identifier(output.rsplit('/').next().unwrap_or("a.out"))
        .dead_stripped(dead_strip);

    match blinker_link::link_timed(&request) {
        Ok((image, timings)) => {
            std::fs::write(output, &image.bytes).expect("writable");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(output, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
            println!("linked {} ({} bytes)", output, image.bytes.len());
            println!("{timings}");
        }
        Err(error) => {
            eprintln!("link failed: {error}");
            std::process::exit(1);
        }
    }
}
