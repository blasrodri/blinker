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
//!
//! The name carries those two hashes *separately* — path, then content — so a
//! daemon can recognise its own predecessors: same path, different content is
//! exactly "the executable I was started from has been replaced". Those are
//! told to exit, which is the difference between rebuilding blinker twenty
//! times and having twenty daemons.
//!
//! # Engaging without being asked
//!
//! [`engage`] is the default path: every link looks for a resident linker, and
//! starts one for the *next* link if there is none. Finding 189 is why. A
//! build's links took 484 ms without a daemon and 294 ms with one, and the
//! daemon was opt-in behind a flag that nothing set — so the mechanism this
//! project is built around was off for everyone who followed the setup
//! instructions. A linker that starts a background process is a real
//! imposition, so it is bounded on every side: the daemon exits after
//! [`IDLE_TIMEOUT`], it is replaced rather than duplicated when blinker is
//! rebuilt, and `BLINKER_NO_DAEMON` or `--blinker-no-daemon` turns the whole
//! thing off.

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

/// How long a start is presumed to be in progress.
///
/// A daemon takes milliseconds to bind. This is the window in which a second
/// link that finds no daemon assumes one is already on its way rather than
/// starting another.
const STARTUP_WINDOW: Duration = Duration::from_secs(10);

/// Where this linker's daemon listens.
///
/// Per user and per executable: see the module docs on why the executable's
/// identity is in the name, and why the two halves of that identity are
/// separable.
pub fn socket_path(executable: &Path) -> PathBuf {
    let (_, name) = socket_name(executable);
    std::env::temp_dir().join(name)
}

/// The socket's file name, and the prefix every daemon for this *path* shares.
///
/// Splitting them is what makes a superseded daemon recognisable: everything
/// under the same prefix was started from the same executable path, and
/// everything under the same prefix with a different suffix was started from
/// different bytes at that path.
fn socket_name(executable: &Path) -> (String, String) {
    let mut path = blake3::Hasher::new();
    path.update(executable.as_os_str().as_encoded_bytes());
    let mut content = blake3::Hasher::new();
    if let Ok(meta) = std::fs::metadata(executable) {
        content.update(&meta.len().to_le_bytes());
        if let Ok(modified) = meta.modified() {
            if let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH) {
                content.update(&since.as_nanos().to_le_bytes());
            }
        }
    }
    // Short on purpose. A Unix socket path has to fit in `sockaddr_un`, which
    // is 104 bytes on macOS, and the temporary directory it sits in is already
    // around fifty of them — `/var/folders/xx/<24 characters>/T/`. Two sixteen-
    // character hashes fit, but only just, and "only just" is a linker that
    // works for me and not for the next person. Thirty-two bits of path is a
    // grouping key, and a collision costs one unnecessary retirement; forty-
    // eight bits of content is the identity that must not collide, and its
    // input includes an mtime in nanoseconds.
    let prefix = format!(
        "blinker-{}-{}-",
        // The uid keeps two users on one machine from sharing a socket, which
        // the directory permissions would prevent anyway — but a collision
        // there is a confusing permission error rather than a clean miss.
        unsafe { libc::getuid() },
        &path.finalize().to_hex()[..8]
    );
    let name = format!("{prefix}{}.sock", &content.finalize().to_hex()[..12]);
    (prefix, name)
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

/// A request with no working directory and no arguments: "are you alive?".
///
/// A real exchange is the only way to tell a live daemon from a socket file
/// left by a dead one — `connect` succeeding proves nothing on macOS, which is
/// what made the first version of `serve` refuse to start after a crash, for
/// as long as the stale file existed.
const PING: &[u8] = b"";

/// A request asking the daemon to stop after answering.
///
/// Sent by a newly started daemon to the ones it supersedes. It cannot collide
/// with a real request: the first field of one is a working directory, and a
/// working directory is an absolute path.
const QUIT: &[u8] = b"\x1bquit";

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

/// Whether a daemon is listening at `socket` and answering.
///
/// Sends the ping and requires an answer. Anything else — no file, a refused
/// connection, a connection that accepts and then ends — is not a daemon.
pub fn is_alive(socket: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        return false;
    };
    // A daemon mid-link will not answer promptly; this is asking whether one
    // exists, and one that is busy still exists.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    write_frame(&mut stream, PING).is_ok() && read_frame(&mut stream).is_ok()
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
    if is_alive(socket) {
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
    // A *blocking* accept with a timeout, not a non-blocking one in a sleep
    // loop. The first version polled every 20 ms, which is invisible in a test
    // and adds an average of 10 ms of latency to every link — measured at
    // +10.6 ms, which made the daemon slower than not having one and wiped out
    // everything residency had bought. A linker's whole job here is measured in
    // tens of milliseconds; a sleep is not a small thing to put in front of it.
    accept_timeout(&listener, Duration::from_secs(1))?;

    let mut idle_since = Instant::now();
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                idle_since = Instant::now();
                stream.set_nonblocking(false)?;
                match serve_one(&mut stream, &mut handle) {
                    Ok(Served::Continue) => {}
                    Ok(Served::Stop) => {
                        let _ = std::fs::remove_file(socket);
                        return Ok(());
                    }
                    // One client's failure is not the daemon's. The next
                    // request gets a fresh connection, and the client that
                    // failed falls back to linking in-process.
                    Err(error) => {
                        let _ = error;
                    }
                }
            }
            // The timeout expiring, which is how idleness is noticed.
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if idle_since.elapsed() >= IDLE_TIMEOUT {
                    let _ = std::fs::remove_file(socket);
                    return Ok(());
                }
            }
            Err(error) => {
                let _ = std::fs::remove_file(socket);
                return Err(error);
            }
        }
    }
}

/// Make `accept` block, but not forever.
///
/// `std` has no accept timeout, and the alternatives are both worse: a
/// non-blocking accept in a sleep loop trades latency for idleness, and a
/// blocking one never notices it should exit. `SO_RCVTIMEO` on the listening
/// socket gives an `accept` that returns as soon as a client arrives and gives
/// up after `timeout`.
fn accept_timeout(listener: &UnixListener, timeout: Duration) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let value = libc::timeval {
        tv_sec: timeout.as_secs() as libc::time_t,
        tv_usec: timeout.subsec_micros() as libc::suseconds_t,
    };
    // SAFETY: `fd` is the listener's, open for the call; `value` is a
    // correctly-sized `timeval` and its length is passed as such.
    let result = unsafe {
        libc::setsockopt(
            listener.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            std::ptr::addr_of!(value).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the daemon should keep listening after answering a request.
enum Served {
    Continue,
    Stop,
}

fn serve_one<F>(stream: &mut UnixStream, handle: &mut F) -> std::io::Result<Served>
where
    F: FnMut(&Path, &[String]) -> (i32, Vec<u8>),
{
    let payload = read_frame(stream)?;
    if payload == PING {
        write_frame(stream, &0i32.to_le_bytes())?;
        return Ok(Served::Continue);
    }
    if payload == QUIT {
        // Answered before stopping, so the sender knows this daemon is gone
        // rather than merely unreachable.
        write_frame(stream, &0i32.to_le_bytes())?;
        return Ok(Served::Stop);
    }
    let Some((cwd, argv)) = decode_request(&payload) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed request",
        ));
    };
    let (code, stderr) = handle(&cwd, &argv);
    let mut response = code.to_le_bytes().to_vec();
    response.extend_from_slice(&stderr);
    write_frame(stream, &response)?;
    Ok(Served::Continue)
}

/// Tell the daemon at `socket` to stop, and wait for it to acknowledge.
///
/// Every failure is ignored on purpose: the only reason to send this is that
/// the daemon is superseded, and a superseded daemon that has already died is
/// the outcome being asked for.
pub fn stop(socket: &Path) {
    let Ok(mut stream) = UnixStream::connect(socket) else {
        // Nothing listening, and the file is this daemon's to clean up.
        let _ = std::fs::remove_file(socket);
        return;
    };
    // A daemon mid-link answers when that link is done. Waiting is what makes
    // this an orderly handover rather than a race with a process still writing
    // an output file.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    if write_frame(&mut stream, QUIT).is_ok() {
        let _ = read_frame(&mut stream);
    }
}

/// Stop every daemon started from this executable's path but not its bytes.
///
/// Rebuilding blinker orphans its daemon: the socket name changes, so nothing
/// will ever connect to the old one again, and it sits holding a session's
/// worth of parsed objects until its idle timeout. Under active development
/// that is one abandoned process per rebuild.
pub fn retire_superseded(executable: &Path) {
    let (prefix, current) = socket_name(executable);
    let directory = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".sock") || name == current {
            continue;
        }
        stop(&directory.join(name));
    }
}

/// Serve links from this process until it goes idle.
///
/// The session lives here and nowhere else: one per daemon, handed to every
/// request, which is the entire reason the daemon exists.
pub fn serve_links() -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let socket = socket_path(&executable);
    // Before binding, not after: if this process loses the race to bind, the
    // daemon that won is this same executable and the sweep was still the
    // right thing to have done.
    retire_superseded(&executable);
    let mut session = blinker_link::Session::default();
    // The one thing that distinguishes this from a one-shot link: there will be
    // another one. See `Session::resident`.
    session.set_resident(true);

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

/// Stop every resident linker started from this executable's path.
///
/// Nobody starts a daemon deliberately any more, so nobody has a process to
/// kill: it has no terminal, and its name is the linker's. This is the way
/// back out — of a build that is behaving strangely, of a machine that should
/// be idle, of an experiment.
pub fn stop_resident() -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    stop(&socket_path(&executable));
    // The current build's daemon is one of possibly several: every previous
    // build of blinker at this path may have left one, and "stop the linker"
    // should not mean "stop the newest of them".
    retire_superseded(&executable);
    // A marker left behind would suppress the next start for its whole window.
    let _ = std::fs::remove_file(starting(&socket_path(&executable)));
    Ok(())
}

/// Use a resident linker if there is one, and arrange for there to be one.
///
/// `Some(code)` means the link is done and this process should exit with that
/// code. `None` means link here — either because a daemon was not wanted, or
/// because none was running, in which case one has just been started for the
/// links that follow.
///
/// Nothing here can make a link fail. Every failure path returns `None`, which
/// is the behaviour blinker had before any of this existed.
pub fn engage(argv: &[String]) -> Option<i32> {
    if !wanted(argv) {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let socket = socket_path(&executable);
    match link_via_daemon(argv) {
        Ok(Some(code)) => Some(code),
        Ok(None) => {
            // The first link of a build is the one that pays for the rest: it
            // links here, and by the time the second arrives there is a daemon
            // to answer it.
            start(&executable, &socket);
            None
        }
        // Quiet unless the daemon was asked for by name. It is the default
        // path now, and something that happens on every link has to be silent
        // when it does not work: the link still happened, correctly, and a
        // warning printed into a build's output for a socket that could not be
        // reached is noise the user cannot act on. `--blinker-daemon` is a
        // request, and a request that fails is worth saying so.
        Err(error) => {
            if argv.iter().any(|arg| arg == "--blinker-daemon") {
                eprintln!("blinker: daemon unavailable ({error}); linking in process");
            }
            None
        }
    }
}

/// Whether this invocation should look for a resident linker at all.
fn wanted(argv: &[String]) -> bool {
    // The environment variable exists because the linker is not invoked by
    // hand: it is spawned by `rustc`, and adding a flag to that argument vector
    // means editing a cargo config. Turning the daemon off has to be possible
    // from the shell that runs the build.
    if std::env::var_os("BLINKER_NO_DAEMON").is_some() {
        return false;
    }
    !argv.iter().any(|arg| arg == "--blinker-no-daemon")
}

/// Start a daemon for subsequent links, without waiting for it.
///
/// Waiting would hand this link to it, which sounds like a saving and is not:
/// the daemon's value is entirely in the state it accumulates, and it has none
/// yet. Starting it costs a fork; linking here costs what it always did.
fn start(executable: &Path, socket: &Path) {
    if !claim(&starting(socket)) {
        return;
    }
    use std::process::Stdio;
    // Detached from this process's streams. A daemon inheriting them would
    // write into whatever `rustc` is reading, and would keep a pipe open long
    // after the build that started it finished.
    let _ = std::process::Command::new(executable)
        .arg("--blinker-daemon-serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// The marker saying a start is in progress.
fn starting(socket: &Path) -> PathBuf {
    socket.with_extension("starting")
}

/// Whether this process is the one that should start the daemon.
///
/// `cargo` links several crates at once, and none of them finds a daemon in the
/// window before the first one binds. Without this, every link in that window
/// starts a server, and all but one of them exits immediately on `AddrInUse` —
/// harmless, but a build that spawns a dozen doomed processes is a build that
/// looks like it is doing something wrong.
fn claim(marker: &Path) -> bool {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(marker)
    {
        Ok(_) => true,
        // A marker from a start that never completed would suppress every
        // future one, and the daemon would never come back. It is only
        // evidence of a start in progress for as long as a start takes.
        Err(_) => match std::fs::metadata(marker).and_then(|meta| meta.modified()) {
            Ok(at) if at.elapsed().is_ok_and(|since| since > STARTUP_WINDOW) => {
                let _ = std::fs::remove_file(marker);
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(marker)
                    .is_ok()
            }
            _ => false,
        },
    }
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

    /// A superseded daemon stops when told to, and takes its socket with it.
    #[test]
    fn a_daemon_stops_when_asked() {
        let scratch = Scratch::dir("daemon-quit").expect("scratch");
        let socket = scratch.join("quit.sock");

        let served = {
            let socket = socket.clone();
            std::thread::spawn(move || serve(&socket, |_, _| (0, Vec::new())))
        };
        // Bound, not merely spawned: `stop` on a path nothing is listening to
        // removes the file and returns, which would pass this test without a
        // daemon ever having existed.
        let mut alive = false;
        for _ in 0..400 {
            if is_alive(&socket) {
                alive = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(alive, "the daemon never came up");

        stop(&socket);
        served
            .join()
            .expect("the daemon thread finished")
            .expect("it stopped cleanly");
        assert!(!socket.exists(), "it left its socket behind");
    }

    /// Only one of several links starts a daemon.
    #[test]
    fn a_start_is_claimed_once() {
        let scratch = Scratch::dir("daemon-claim").expect("scratch");
        let marker = scratch.join("start.marker");
        assert!(claim(&marker), "the first link should start one");
        assert!(!claim(&marker), "the second link should not");
    }

    /// But a claim that never produced a daemon expires, or the daemon could
    /// never come back.
    #[test]
    fn a_stale_claim_expires() {
        let scratch = Scratch::dir("daemon-claim-stale").expect("scratch");
        let marker = scratch.join("start.marker");
        assert!(claim(&marker));

        // Backdated past the window rather than waiting it out.
        let long_ago = std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - STARTUP_WINDOW * 2);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&marker)
            .expect("marker")
            .set_times(long_ago)
            .expect("backdated");
        assert!(claim(&marker), "an abandoned start should not be permanent");
    }

    /// The two halves of the socket name are independent: same path and
    /// different bytes is what "superseded" means, and it has to be visible in
    /// the name for a daemon to recognise its predecessors.
    #[test]
    fn the_socket_name_separates_path_from_content() {
        let scratch = Scratch::dir("daemon-name").expect("scratch");
        let executable = scratch.join("blinker");
        std::fs::write(&executable, b"one").expect("written");
        let (prefix, first) = socket_name(&executable);

        // Rebuilt in place: a different size, so a different content hash.
        std::fs::write(&executable, b"a longer build").expect("rewritten");
        let (again, second) = socket_name(&executable);

        assert_eq!(prefix, again, "the path half moved");
        assert_ne!(first, second, "the content half did not");
        assert!(second.starts_with(&prefix) && second.ends_with(".sock"));

        let (elsewhere, _) = socket_name(&scratch.join("other/blinker"));
        assert_ne!(prefix, elsewhere, "two paths shared a prefix");
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
