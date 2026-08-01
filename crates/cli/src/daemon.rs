//! A resident linker, and the client that talks to it.
//!
//! Finding 104 measured what holding parsed inputs across links is worth: the
//! same link goes from 41.9 ms to 19.2 ms when the process does not exit
//! between them. Nothing in this file makes a link faster. It exists so that a
//! build can reach state that already exists — `rustc` spawns a linker per
//! crate, and a process that exits has no state to reach.
//!
//! # The protocol
//!
//! Length-prefixed frames over a Unix socket, hand-rolled because it carries
//! four things and none of them need a schema:
//!
//! ```text
//!   request    cwd \0 arg \0 arg \0 ...        the argument vector, verbatim
//!   response   u32 exit code, then stderr      what the client prints and exits with
//! ```
//!
//! The argument vector is passed through unchanged, so a link performed by the
//! daemon is the link the client would have performed. That is the property
//! worth protecting: a daemon that interprets arguments differently from the
//! one-shot path is a second linker with a shared name.
//!
//! # Why one link at a time
//!
//! The session is `&mut` and holds every parsed input; serving two links at
//! once would mean either two sessions — which halves the point — or locking
//! around the thing every stage touches. Requests queue. A build system that
//! links twelve crates in parallel gets them one after another, each ~19 ms
//! rather than ~42, which is still ahead; making it concurrent is a later
//! question about sharing a session, not about sockets.
//!
//! # Why the socket name carries the linker's identity
//!
//! A daemon holds parsed state produced by *a particular build of blinker*.
//! Rebuild the linker and the old daemon is still listening, still holding its
//! state, and still perfectly willing to answer — with the previous version's
//! behaviour. That is finding 64's failure mode with a longer lifetime: a stale
//! result that no test can distinguish from a correct one. The socket path
//! includes a hash of the executable's path, size and mtime, so a new linker
//! simply cannot find the old one's socket.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long a daemon waits with no work before exiting.
///
/// A linker daemon that outlives the build it was started for is a background
/// process holding tens of megabytes of parsed objects for a session that
/// ended. Twenty minutes covers a working session's gaps without keeping
/// yesterday's state alive.
const IDLE_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// Frames larger than this are refused rather than allocated.
///
/// A `u32` length read off a socket is untrusted input, and the natural
/// implementation — `vec![0; length]` — turns a wrong four bytes into an
/// allocation of up to 4 GiB. Real argument vectors are kilobytes; a megabyte
/// is already generous.
const MAX_FRAME: u32 = 1 << 20;

/// Where this linker's daemon listens.
///
/// Per user and per executable: see the module docs on why the executable's
/// identity is in the name.
pub fn socket_path(executable: &Path) -> PathBuf {
    let mut hasher = blake3::Hasher::new();
    hasher.update(executable.as_os_str().as_encoded_bytes());
    if let Ok(meta) = std::fs::metadata(executable) {
        hasher.update(&meta.len().to_le_bytes());
        if let Ok(modified) = meta.modified() {
            if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                hasher.update(&since.as_nanos().to_le_bytes());
            }
        }
    }
    let digest = hasher.finalize();
    let name = format!(
        "blinker-{}-{}.sock",
        // The uid keeps two users on one machine from sharing a socket, which
        // the directory permissions would prevent anyway — but a collision
        // there is a confusing permission error rather than a clean miss.
        unsafe { libc::getuid() },
        &digest.to_hex()[..16]
    );
    std::env::temp_dir().join(name)
}

fn write_frame(stream: &mut UnixStream, payload: &[u8]) -> std::io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "frame too large"))?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header);
    if length > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame exceeds the maximum",
        ));
    }
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

/// Encode a working directory and argument vector into a request frame.
fn encode_request(cwd: &Path, argv: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(cwd.as_os_str().as_encoded_bytes());
    for arg in argv {
        out.push(0);
        out.extend_from_slice(arg.as_bytes());
    }
    out
}

/// The inverse. `None` when the frame is not a request this version wrote.
fn decode_request(payload: &[u8]) -> Option<(PathBuf, Vec<String>)> {
    let mut parts = payload.split(|byte| *byte == 0);
    let cwd = PathBuf::from(String::from_utf8(parts.next()?.to_vec()).ok()?);
    let argv = parts
        .map(|part| String::from_utf8(part.to_vec()).ok())
        .collect::<Option<Vec<String>>>()?;
    Some((cwd, argv))
}

/// Ask the daemon at `socket` to perform a link.
///
/// `Ok(None)` means there is no daemon — no socket, or one nothing is
/// listening on. That is not an error: the caller links in-process instead,
/// which is what every invocation did before this existed.
pub fn request(socket: &Path, argv: &[String]) -> std::io::Result<Option<(i32, Vec<u8>)>> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(error) if is_absent(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let cwd = std::env::current_dir()?;

    // The whole exchange, and any failure in it means "no daemon". Connecting
    // is not proof that one is there: on macOS a socket file left by a daemon
    // that was killed accepts a connection and then delivers end-of-file, so a
    // client that only checked `connect` would report an I/O error and refuse
    // to link. Falling back is always safe — the work has not been done, and
    // doing it here is what happens with no daemon at all.
    let mut exchange = || -> std::io::Result<(i32, Vec<u8>)> {
        write_frame(&mut stream, &encode_request(&cwd, argv))?;
        let response = read_frame(&mut stream)?;
        if response.len() < 4 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the daemon sent a truncated response",
            ));
        }
        let code = i32::from_le_bytes(response[..4].try_into().expect("4 bytes"));
        Ok((code, response[4..].to_vec()))
    };
    match exchange() {
        Ok(answer) => Ok(Some(answer)),
        Err(error) if is_absent(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether an error means "there is no daemon" rather than "the daemon failed".
///
/// A stale socket file — left by a daemon that was killed — refuses connections
/// with `ECONNREFUSED`, and is indistinguishable from never having existed as
/// far as the caller's decision goes.
fn is_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound
            | std::io::ErrorKind::ConnectionRefused
            // A daemon that died — before answering, or between binding and
            // being killed. `UnexpectedEof` is the one a stale socket file
            // produces on macOS, and it took a test to find because
            // `connect` succeeding reads as "a daemon is there".
            | std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::BrokenPipe
    )
}

/// Run the daemon, serving links until it has been idle for [`IDLE_TIMEOUT`].
///
/// `handle` performs one link and returns its exit code and captured stderr.
/// It is passed in rather than called directly so this file holds no opinion
/// about what a link is, and so a test can serve something it can check.
pub fn serve<F>(socket: &Path, mut handle: F) -> std::io::Result<()>
where
    F: FnMut(&Path, &[String]) -> (i32, Vec<u8>),
{
    // A socket file left by a dead daemon would make `bind` fail with
    // `EADDRINUSE` forever. Connecting first distinguishes the two cases: if
    // something answers, that daemon is alive and this one is not needed.
    if UnixStream::connect(socket).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "a daemon is already listening",
        ));
    }
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)?;
    // Only this user may connect. The socket carries argument vectors and
    // performs links on their behalf; the directory is shared.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    }
    listener.set_nonblocking(true)?;

    let mut idle_since = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                idle_since = Instant::now();
                stream.set_nonblocking(false)?;
                if let Err(error) = serve_one(&mut stream, &mut handle) {
                    // One client's failure is not the daemon's. The next
                    // request gets a fresh connection, and the client that
                    // failed falls back to linking in-process.
                    let _ = error;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if idle_since.elapsed() >= IDLE_TIMEOUT {
                    let _ = std::fs::remove_file(socket);
                    return Ok(());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = std::fs::remove_file(socket);
                return Err(error);
            }
        }
    }
}

fn serve_one<F>(stream: &mut UnixStream, handle: &mut F) -> std::io::Result<()>
where
    F: FnMut(&Path, &[String]) -> (i32, Vec<u8>),
{
    let payload = read_frame(stream)?;
    let Some((cwd, argv)) = decode_request(&payload) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed request",
        ));
    };
    let (code, stderr) = handle(&cwd, &argv);
    let mut response = code.to_le_bytes().to_vec();
    response.extend_from_slice(&stderr);
    write_frame(stream, &response)
}

/// Serve links from this process until it goes idle.
///
/// The session lives here and nowhere else: one per daemon, handed to every
/// request, which is the entire reason the daemon exists.
pub fn serve_links() -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let socket = socket_path(&executable);
    let mut session = blinker_link::Session::default();

    serve(&socket, move |cwd, argv| {
        // Relative input and output paths are resolved against the caller's
        // directory, not the daemon's. Safe because requests are served one at
        // a time; a concurrent daemon would have to resolve paths itself
        // rather than move the process.
        if std::env::set_current_dir(cwd).is_err() {
            return (
                1,
                format!("blinker: cannot enter {}\n", cwd.display()).into_bytes(),
            );
        }
        // The daemon's own `--blinker-daemon` is stripped: the link is being
        // performed here, and passing it on would ask this process to look for
        // a daemon to hand it to.
        let argv: Vec<String> = argv
            .iter()
            .filter(|arg| *arg != "--blinker-daemon")
            .cloned()
            .collect();
        match crate::run_in(&argv, &mut session) {
            Ok(outcome) => (outcome.exit_code, Vec::new()),
            Err(error) => (1, format!("blinker: {error}\n").into_bytes()),
        }
    })
}

/// Ask a resident linker to perform this link.
///
/// `Ok(None)` means there is none, and the caller should link in-process.
pub fn link_via_daemon(argv: &[String]) -> std::io::Result<Option<i32>> {
    let executable = std::env::current_exe()?;
    let socket = socket_path(&executable);
    let Some((code, stderr)) = request(&socket, argv)? else {
        return Ok(None);
    };
    if !stderr.is_empty() {
        use std::io::Write;
        let _ = std::io::stderr().write_all(&stderr);
    }
    Ok(Some(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blinker_test_support::Scratch;

    /// The round trip: what the client sends is what the server is asked to do.
    #[test]
    fn a_request_arrives_as_it_was_sent() {
        let scratch = Scratch::dir("daemon-roundtrip").expect("scratch");
        let socket = scratch.join("s.sock");

        let server = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                let listener = UnixListener::bind(&socket).expect("bind");
                let (mut stream, _) = listener.accept().expect("accept");
                let payload = read_frame(&mut stream).expect("frame");
                let (cwd, argv) = decode_request(&payload).expect("decoded");
                let mut response = 0i32.to_le_bytes().to_vec();
                response
                    .extend_from_slice(format!("{}|{}", cwd.display(), argv.join(",")).as_bytes());
                write_frame(&mut stream, &response).expect("responded");
            })
        };
        // The listener is bound by the thread, so the first connect may race it.
        let argv = vec!["-o".to_string(), "out".to_string(), "a.o".to_string()];
        let mut answer = None;
        for _ in 0..200 {
            match request(&socket, &argv) {
                Ok(Some(response)) => {
                    answer = Some(response);
                    break;
                }
                _ => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        server.join().expect("the server thread finished");

        let (code, stderr) = answer.expect("the daemon answered");
        assert_eq!(code, 0);
        let text = String::from_utf8(stderr).expect("utf8");
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(text, format!("{}|-o,out,a.o", cwd.display()));
    }

    /// No daemon is not an error. The caller links in-process.
    #[test]
    fn no_socket_is_not_a_failure() {
        let scratch = Scratch::dir("daemon-absent").expect("scratch");
        let answer = request(&scratch.join("nothing.sock"), &[]).expect("not an error");
        assert!(answer.is_none());
    }

    /// A socket file whose daemon is gone behaves the same way. Otherwise every
    /// client would fail for as long as the stale file existed.
    #[test]
    fn a_stale_socket_file_is_not_a_failure() {
        let scratch = Scratch::dir("daemon-stale").expect("scratch");
        let socket = scratch.join("stale.sock");
        drop(UnixListener::bind(&socket).expect("bind"));
        assert!(
            socket.exists(),
            "the socket file should outlive the listener"
        );
        assert!(request(&socket, &[]).expect("not an error").is_none());
    }

    /// And `serve` takes that stale file over rather than refusing to start.
    #[test]
    fn serve_replaces_a_stale_socket() {
        let scratch = Scratch::dir("daemon-replace").expect("scratch");
        let socket = scratch.join("replace.sock");
        drop(UnixListener::bind(&socket).expect("bind"));

        let served = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                serve(&socket, |_, argv| (argv.len() as i32, b"served".to_vec()))
            })
        };
        let mut answer = None;
        for _ in 0..200 {
            if let Ok(Some(response)) = request(&socket, &["a".to_string(), "b".to_string()]) {
                answer = Some(response);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let (code, stderr) = answer.expect("the daemon answered");
        assert_eq!(code, 2, "the argument vector did not arrive intact");
        assert_eq!(stderr, b"served");

        // Stop the daemon by removing its socket and letting accept fail, so
        // the test does not wait out the idle timeout.
        let _ = std::fs::remove_file(&socket);
        drop(served);
    }

    /// A length field larger than the cap is refused rather than allocated.
    #[test]
    fn an_absurd_frame_length_is_refused() {
        let scratch = Scratch::dir("daemon-frame").expect("scratch");
        let socket = scratch.join("frame.sock");
        let listener = UnixListener::bind(&socket).expect("bind");

        let client = std::thread::spawn(move || {
            let mut stream = UnixStream::connect(&socket).expect("connect");
            stream.write_all(&u32::MAX.to_le_bytes()).expect("wrote");
            // Deliberately no payload: a reader that trusted the length would
            // be waiting for four gigabytes of it.
        });
        let (mut stream, _) = listener.accept().expect("accept");
        let error = read_frame(&mut stream).expect_err("it should refuse");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        client.join().expect("client finished");
    }
}
