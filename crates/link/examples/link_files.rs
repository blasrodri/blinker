//! Link object files named on the command line, for inspection by hand.
//!
//! ```text
//! cargo run -p blinker-link --example link_files -- out a.o b.o
//! ./out; echo $?
//! ```

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((output, objects)) = args.split_first() else {
        eprintln!("usage: link_files <output> <object>...");
        std::process::exit(2);
    };

    let request = blinker_link::LinkRequest::new(objects.iter().map(Into::into).collect())
        .identifier(output.rsplit('/').next().unwrap_or("a.out"));

    match blinker_link::link_to_file(&request, std::path::Path::new(output)) {
        Ok(image) => println!("linked {} ({} bytes)", output, image.bytes.len()),
        Err(error) => {
            eprintln!("link failed: {error}");
            std::process::exit(1);
        }
    }
}
