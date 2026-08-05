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
//! # Why four processes and not four threads
//!
//! One daemon serves one link at a time, and finding 204 measured the cost:
//! a real cold build submits eleven links at once, and they queue into a
//! 35.7 → 205.5 ms staircase in which every served link's own timings look
//! perfectly healthy. The queue is invisible from inside the daemon.
//!
//! Threads were the obvious fix and are the wrong one, for a reason that has
//! nothing to do with the session. Relative paths in a request are resolved
//! against the *caller's* directory, and the way that is done is `chdir` —
//! which is a property of the process, not the thread. Two links running
//! concurrently in one process would resolve each other's relative paths, and
//! a linker that reads the wrong object file produces a wrong binary rather
//! than an error. Making that safe means resolving every path in a request by
//! hand — inputs, `-o`, `-L`, `-F`, response files — where one missed flag is
//! silently the wrong file.
//!
//! Separate processes keep `chdir` correct for free, and the pid turns out to
//! be load-bearing twice more: the output is written to `.blinker-{pid}.tmp`
//! beside its destination and the cache to `.tmp{pid}`, both published by
//! rename. Every one of those is safe across processes and would have been a
//! collision across threads.
//!
//! So: [`WORKERS`] resident linkers, each owning one session, each serving one
//! link at a time. A request is routed to one of them by hashing its output
//! path ([`worker_of`]), which keeps a target on the worker that already holds
//! its state and serialises two links to the same output — which they require
//! anyway, since they write the same file. Workers are started on demand, so a
//! build that links one crate still runs one daemon.
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

/// How often a waiting daemon looks up from the socket.
///
/// It is not a poll interval in the sleep-loop sense: `poll` blocks in the
/// kernel and returns the instant a client arrives, so this costs one wakeup a
/// second and nothing on the latency of a link.
const TICK: Duration = Duration::from_secs(1);

/// How many resident linkers serve one build.
///
/// Four because that is the width a cargo build actually presents: finding 204
/// caught eleven links submitted at once, but they arrive in waves as crates
/// finish, and the useful width is bounded by how many crates are ready at the
/// same moment rather than by how many the build contains. Each worker holds a
/// session, so the number is also a memory multiplier — see [`start`], which
/// divides the budget between them rather than handing each the whole thing.
pub const WORKERS: usize = 4;

/// Where one of this linker's daemons listens.
///
/// Per user and per executable: see the module docs on why the executable's
/// identity is in the name, and why the two halves of that identity are
/// separable. The worker index is last so the set is legible in the temporary
/// directory — `…-0.sock` through `…-3.sock` — which matters because these are
/// processes nobody started deliberately and may have to look for.
pub fn socket_path(executable: &Path, worker: usize) -> PathBuf {
    let (_, name) = socket_name(executable, worker);
    std::env::temp_dir().join(name)
}

/// Which worker serves this invocation.
///
/// The output path is the key, because it is what "target" means to everything
/// downstream: one binary is one session's worth of retained state, and two
/// links writing the same binary must not run at once whatever else is true.
/// Hashing it keeps a target on one worker for the life of the daemon, so the
/// state a link leaves behind is state the next link to that target will find.
///
/// The key is built lexically — joined to the working directory if relative,
/// then `.` and `..` removed — and never with `canonicalize`. Resolving it on
/// disk reads better and is wrong: the answer would depend on whether the
/// output's directory exists *yet*, so a target would route one way on the
/// link that creates its directory and another way afterwards, losing the
/// session it had just filled. A test caught exactly that. It also spends a
/// syscall per path component on every link, in a path being measured against
/// a 2 ms budget.
///
/// What lexical costs is that two spellings of one file through a symlink —
/// `/tmp/x/prog` and `/private/tmp/x/prog` — route apart. No build system
/// spells its own output two ways within a build, and two concurrent links to
/// one output file are a build error before they are a linker's problem; the
/// property being bought here is that a *given* spelling always lands on the
/// same worker, and that one is exact.
pub fn worker_of(argv: &[String]) -> usize {
    let replayed;
    let output = match argv.iter().position(|arg| arg == "-o") {
        Some(at) => argv.get(at + 1),
        // A replay names its output inside the record, not on the command
        // line, so every replayed link hashed to worker 0 — which is what
        // `build-links.py` drives, so the harness that measures a build's links
        // has been measuring one worker serving all sixteen programs. Routing
        // is not a test concern: two recordings replayed together deserve two
        // workers for the same reason two crates do (finding 214).
        None => {
            replayed = argv
                .iter()
                .filter_map(|arg| arg.strip_prefix("--blinker-replay-invocation="))
                .find_map(|path| {
                    let text = std::fs::read_to_string(path).ok()?;
                    let record: serde_json::Value = serde_json::from_str(&text).ok()?;
                    Some(record.get("output_path")?.as_str()?.to_string())
                });
            replayed.as_ref()
        }
    };
    let Some(output) = output else {
        // No output named: not a link this router can place, and worker 0 is
        // as good as any. It will still be served correctly — routing decides
        // where, never whether.
        return 0;
    };
    let path = Path::new(output);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(lexical(&absolute).as_os_str().as_encoded_bytes());
    let digest = u64::from_le_bytes(hasher.finalize().as_bytes()[..8].try_into().expect("8"));
    (digest % WORKERS as u64) as usize
}

/// An absolute path with `.` and `..` removed, without touching the disk.
///
/// `..` is popped rather than resolved, which differs from what the filesystem
/// would say when the component above is a symlink. That is the documented
/// limit of [`worker_of`]: this decides which worker, never whether.
fn lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// The socket's file name, and the prefix every daemon for this *path* shares.
///
/// Splitting them is what makes a superseded daemon recognisable: everything
/// under the same prefix was started from the same executable path, and
/// everything under the same prefix with a different suffix was started from
/// different bytes at that path.
fn socket_name(executable: &Path, worker: usize) -> (String, String) {
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
    //
    // The worker suffix spends two more of those bytes. It is worth naming
    // that this is the budget it comes out of: the name is around 40 bytes on
    // top of a temporary directory of about 50, so there is room for a
    // one-digit worker index and not for a second hash.
    let prefix = format!(
        "blinker-{}-{}-",
        // The uid keeps two users on one machine from sharing a socket, which
        // the directory permissions would prevent anyway — but a collision
        // there is a confusing permission error rather than a clean miss.
        unsafe { libc::getuid() },
        &path.finalize().to_hex()[..8]
    );
    let name = format!(
        "{prefix}{}-{worker}.sock",
        &content.finalize().to_hex()[..12]
    );
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
pub fn request(
    socket: &Path,
    argv: &[String],
    worker: usize,
) -> std::io::Result<Option<(i32, Vec<u8>)>> {
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
    // "Was the request delivered?" is the whole question. Before it, a failure
    // means there is no daemon and linking here is what would have happened
    // anyway. After it, the daemon took the work and died — a panic in a link
    // is the ordinary way — and falling back silently turns a linker that
    // crashes on every request into a linker that merely feels slow. That is
    // not hypothetical: it is how a wrong live set behind an experimental flag
    // passed a byte-identity harness, because every delta link panicked the
    // daemon and every client quietly relinked in process and got the right
    // answer (finding 195).
    let mut delivered = false;
    let mut exchange = |delivered: &mut bool| -> std::io::Result<(i32, Vec<u8>)> {
        write_frame(&mut stream, &encode_request(&cwd, argv))?;
        *delivered = true;
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
    match exchange(&mut delivered) {
        Ok(answer) => Ok(Some(answer)),
        Err(error) if is_absent(&error) && !delivered => Ok(None),
        Err(error) if delivered => Err(std::io::Error::other(format!(
            "the resident linker died while performing this link ({error}); \
             linking in process. Run it in the foreground to see why: \
             blinker --blinker-daemon-serve={worker}"
        ))),
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
pub fn serve<F, S>(
    socket: &Path,
    mut handle: F,
    superseded: S,
    idle_timeout: Duration,
) -> std::io::Result<()>
where
    F: FnMut(&Path, &[String]) -> (i32, Vec<u8>),
    S: Fn() -> bool,
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
    // Non-blocking, with `poll` doing the waiting: see `wait_for_client`.
    listener.set_nonblocking(true)?;

    let mut idle_since = Instant::now();
    loop {
        if !wait_for_client(&listener, TICK)? {
            if superseded() || idle_since.elapsed() >= idle_timeout {
                let _ = std::fs::remove_file(socket);
                return Ok(());
            }
            continue;
        }
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
            // `poll` said readable and `accept` disagreed — a client that gave
            // up in between, or a spurious wakeup. Neither is this daemon's
            // problem, and the next iteration is another tick.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                let _ = std::fs::remove_file(socket);
                return Err(error);
            }
        }
    }
}

/// Wait until a client is there, or `timeout` passes. `true` means a client.
///
/// `std` has no accept timeout, and the obvious alternatives are both bad: a
/// non-blocking accept in a sleep loop trades latency for idleness — the first
/// version of this polled every 20 ms and added a measured 10.6 ms to every
/// link, which made the daemon slower than not having one — and a blocking
/// accept never notices it should exit.
///
/// This was `SO_RCVTIMEO` on the listening socket, which reads like the right
/// answer, is accepted by `setsockopt` with a return of 0, and does nothing:
/// on macOS the receive timeout does not apply to `accept`, which goes on
/// blocking forever. So the idle timeout above had never once fired. Every
/// daemon ever started was still running, and the only reason that was not
/// obvious is that starting one used to be a deliberate act.
///
/// `poll` is the thing that actually blocks in the kernel until a client
/// arrives and gives up on schedule, which is what the old comment claimed.
fn wait_for_client(listener: &UnixListener, timeout: Duration) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    let mut fds = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one correctly-initialised `pollfd`, and a count that says so.
    // The listener owns the descriptor and outlives the call.
    let ready = unsafe { libc::poll(&mut fds, 1, timeout.as_millis() as libc::c_int) };
    match ready {
        // Interrupted is not an error to the caller: it is a tick that found
        // nothing, which is what the idle check wants anyway.
        -1 => {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error)
            }
        }
        0 => Ok(false),
        _ => Ok(true),
    }
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
    // Every worker of *this* build is kept, not just the caller's own. They
    // share a path prefix and differ only in the content hash, so a worker
    // that swept on "not my socket name" would stop its three siblings on
    // startup — and each of them would stop it back.
    let keep: Vec<String> = (0..WORKERS)
        .map(|worker| socket_name(executable, worker).1)
        .collect();
    sweep(executable, &keep);
}

/// Stop every daemon under this executable's prefix except the names in `keep`.
fn sweep(executable: &Path, keep: &[String]) {
    let (prefix, _) = socket_name(executable, 0);
    let directory = std::env::temp_dir();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".sock") {
            continue;
        }
        if keep.iter().any(|kept| kept == name) {
            continue;
        }
        stop(&directory.join(name));
    }
}

/// Serve links from this process until it goes idle.
///
/// The session lives here and nowhere else: one per daemon, handed to every
/// request, which is the entire reason the daemon exists.
pub fn serve_links(worker: usize) -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let socket = socket_path(&executable, worker);
    // Before binding, not after: if this process loses the race to bind, the
    // daemon that won is this same executable and the sweep was still the
    // right thing to have done.
    retire_superseded(&executable);
    let mut session = blinker_link::Session::default();
    // The one thing that distinguishes this from a one-shot link: there will be
    // another one. See `Session::resident`.
    session.set_resident(true);

    // "The file I was started from is no longer the file I am." Computed the
    // same way a client computes the path it looks for, so the daemon retires
    // exactly when it stops being findable.
    let identity = socket.clone();
    let superseded = move || {
        std::env::current_exe().is_ok_and(|executable| socket_path(&executable, worker) != identity)
    };

    serve(
        &socket,
        move |cwd, argv| {
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
            let served = match crate::run_in(&argv, &mut session) {
                Ok(outcome) => (outcome.exit_code, Vec::new()),
                Err(error) => (1, format!("blinker: {error}\n").into_bytes()),
            };
            // A link too large to cache leaves behind a gigabyte of interned
            // names, digests and memos held for a reuse this session has
            // already ruled out — and then the allocator holds the pages after
            // they are freed. Both are the right default for a process that
            // will link the same program again; this one has just said it will
            // not. Emptying and handing the pages back costs milliseconds and
            // stops four idle workers sitting on 3.6 GB of a machine whose page
            // cache is where the *inputs* live.
            if session.declined_to_retain() {
                session.forget();
                blinker_link::release_free_memory();
            }
            served
        },
        superseded,
        IDLE_TIMEOUT,
    )
}

/// Ask a resident linker to perform this link.
///
/// `Ok(None)` means there is none, and the caller should link in-process.
pub fn link_via_daemon(argv: &[String], worker: usize) -> std::io::Result<Option<i32>> {
    let executable = std::env::current_exe()?;
    let socket = socket_path(&executable, worker);
    let asked = std::time::Instant::now();
    let Some((code, stderr)) = request(&socket, argv, worker)? else {
        return Ok(None);
    };
    // What the client waited, from the client's side. The daemon serves one
    // connection at a time, so a link that arrives while another is being
    // served waits in the listen backlog — where nothing inside the daemon can
    // see it, and where the served link's own timings look perfectly healthy.
    //
    // Appended to the file `BLINKER_TRACE_WAIT` names, not printed. Written to
    // stderr, every line came back through `rustc` as a linker warning, and
    // cargo *replays cached warnings* for units it did not rebuild — so a
    // second build reprinted the first build's timings, to the tenth of a
    // millisecond, for links that had not happened. Two runs agreeing exactly
    // is what gave it away.
    //
    // A file with pids and wall-clock stamps also answers the question stderr
    // could not: whether two links overlapped.
    if let Some(path) = std::env::var_os("BLINKER_TRACE_WAIT") {
        let finished = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|since| since.as_secs_f64())
            .unwrap_or(0.0);
        let waited = asked.elapsed().as_secs_f64() * 1000.0;
        let output = argv
            .iter()
            .position(|arg| arg == "-o")
            .and_then(|at| argv.get(at + 1))
            .map(String::as_str)
            .unwrap_or("?");
        // Formatted whole, then written once. `writeln!` on a `File` issues a
        // write per fragment, and now that links genuinely overlap, four
        // clients appending fragments to one file interleave them: the third
        // round of the first measurement produced a line with two timestamps
        // spliced together and a span of 6.5e17 ms. `O_APPEND` makes a single
        // write atomic, so the fix is to only make one.
        let line = format!(
            "{} {:.6} {:.6} {waited:.1} {worker} {output}\n",
            std::process::id(),
            finished - waited / 1000.0,
            finished
        );
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
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
    for worker in 0..WORKERS {
        stop(&socket_path(&executable, worker));
    }
    // The current build's daemons are some of possibly many: every previous
    // build of blinker at this path may have left a set, and "stop the linker"
    // should not mean "stop the newest of them". Nothing is kept — this is the
    // one sweep that is allowed to take the current build's workers too, and
    // they have already been asked to stop above.
    sweep(&executable, &[]);
    // A marker left behind would suppress the next start for its whole window.
    for worker in 0..WORKERS {
        let _ = std::fs::remove_file(starting(&socket_path(&executable, worker)));
    }
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
    // Routed before anything is asked of any daemon: which worker owns this
    // target decides which socket is even looked for, so a target that has a
    // resident linker never falls back because a *different* worker is the one
    // that has not started yet.
    let worker = worker_of(argv);
    let socket = socket_path(&executable, worker);
    match link_via_daemon(argv, worker) {
        Ok(Some(code)) => Some(code),
        Ok(None) => {
            // The first link of a build is the one that pays for the rest: it
            // links here, and by the time the second arrives there is a daemon
            // to answer it. Per worker, so a build reaches its full width only
            // as it presents targets that hash to each of them — which is the
            // right shape: a one-crate build never starts four processes.
            start(&executable, &socket, worker);
            None
        }
        // Quiet unless the daemon was asked for by name. It is the default
        // path now, and something that happens on every link has to be silent
        // when it does not work: the link still happened, correctly, and a
        // warning printed into a build's output for a socket that could not be
        // reached is noise the user cannot act on. `--blinker-daemon` is a
        // request, and a request that fails is worth saying so.
        // Printed whether or not the daemon was asked for by name. The quiet
        // path above is for "there was none"; this is for "there was one and
        // it fell over", which the user has to be able to see.
        Err(error) => {
            eprintln!("blinker: {error}");
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
fn start(executable: &Path, socket: &Path, worker: usize) {
    if !claim(&starting(socket)) {
        return;
    }
    use std::process::Stdio;
    // Detached from this process's streams. A daemon inheriting them would
    // write into whatever `rustc` is reading, and would keep a pipe open long
    // after the build that started it finished.
    let mut command = std::process::Command::new(executable);
    command
        .arg(format!("--blinker-daemon-serve={worker}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // `BLINKER_MEMORY_BUDGET` is what a user means by "do not hold more than
    // this much", and they mean it about the linker rather than about one of
    // four processes they did not know existed. Finding 201 gave the session a
    // byte budget precisely so retained state would be bounded; four workers
    // each honouring the whole number would quietly quadruple it.
    let share = (blinker_link::memory_budget() / WORKERS / (1024 * 1024)).max(1);
    command.env("BLINKER_MEMORY_BUDGET", share.to_string());
    let _ = command.spawn();
}

/// The worker index in `--blinker-daemon-serve[=N]`, if this is that request.
///
/// The bare spelling is worker 0 and is what a person types: the error a client
/// prints when a daemon dies tells them to run one in the foreground, and it
/// names the worker, but the common case of looking at any of them should not
/// require knowing the routing.
pub fn serving(argv: &[String]) -> Option<usize> {
    argv.iter().find_map(|arg| {
        let rest = arg.strip_prefix("--blinker-daemon-serve")?;
        match rest.strip_prefix('=') {
            Some(index) => index.parse().ok(),
            None if rest.is_empty() => Some(0),
            None => None,
        }
    })
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
            match request(&socket, &argv, 0) {
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
        let answer = request(&scratch.join("nothing.sock"), &[], 0).expect("not an error");
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
        assert!(request(&socket, &[], 0).expect("not an error").is_none());
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
                serve(
                    &socket,
                    |_, argv| (argv.len() as i32, b"served".to_vec()),
                    || false,
                    IDLE_TIMEOUT,
                )
            })
        };
        let mut answer = None;
        for _ in 0..200 {
            if let Ok(Some(response)) = request(&socket, &["a".to_string(), "b".to_string()], 0) {
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
            std::thread::spawn(move || {
                serve(&socket, |_, _| (0, Vec::new()), || false, IDLE_TIMEOUT)
            })
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

    /// A daemon whose executable was replaced stops on its own.
    ///
    /// It is already unreachable — the socket name carries the executable's
    /// content, so no client will compute this path again — and it holds a
    /// session and an open file for nothing. The file matters: on macOS an
    /// executable overwritten in place while a process runs it has its inode
    /// invalidated, and every later `exec` of that path dies with SIGKILL
    /// before any of its code runs.
    #[test]
    fn a_superseded_daemon_retires_itself() {
        let scratch = Scratch::dir("daemon-superseded").expect("scratch");
        let socket = scratch.join("old.sock");

        let replaced = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let served = {
            let (socket, replaced) = (socket.clone(), std::sync::Arc::clone(&replaced));
            std::thread::spawn(move || {
                serve(
                    &socket,
                    |_, _| (0, Vec::new()),
                    move || replaced.load(std::sync::atomic::Ordering::Relaxed),
                    IDLE_TIMEOUT,
                )
            })
        };
        let mut alive = false;
        for _ in 0..400 {
            if is_alive(&socket) {
                alive = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(alive, "the daemon never came up");

        replaced.store(true, std::sync::atomic::Ordering::Relaxed);
        served
            .join()
            .expect("the daemon thread finished")
            .expect("it stopped cleanly");
        assert!(!socket.exists(), "it left its socket behind");
    }

    /// And a daemon nobody uses goes away on its own.
    ///
    /// This is the assertion that had no way to fail before: the idle check sat
    /// in a branch of `accept` that `SO_RCVTIMEO` promised to reach and never
    /// did, so every daemon ever started outlived the build it was for, and the
    /// only reason nobody noticed is that starting one used to be deliberate.
    #[test]
    fn an_idle_daemon_goes_away() {
        let scratch = Scratch::dir("daemon-idle").expect("scratch");
        let socket = scratch.join("idle.sock");

        let served = {
            let socket = socket.clone();
            std::thread::spawn(move || {
                serve(
                    &socket,
                    |_, _| (0, Vec::new()),
                    || false,
                    Duration::from_millis(300),
                )
            })
        };
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
        let (prefix, first) = socket_name(&executable, 0);

        // Rebuilt in place: a different size, so a different content hash.
        std::fs::write(&executable, b"a longer build").expect("rewritten");
        let (again, second) = socket_name(&executable, 0);

        assert_eq!(prefix, again, "the path half moved");
        assert_ne!(first, second, "the content half did not");
        assert!(second.starts_with(&prefix) && second.ends_with(".sock"));

        let (elsewhere, _) = socket_name(&scratch.join("other/blinker"), 0);
        assert_ne!(prefix, elsewhere, "two paths shared a prefix");
    }

    /// Workers of one build must be distinguishable and must not be mistaken
    /// for predecessors: they share every hash and differ only in the suffix,
    /// which is what `retire_superseded` has to keep rather than sweep.
    #[test]
    fn every_worker_of_one_build_gets_its_own_name_under_one_prefix() {
        let scratch = Scratch::dir("daemon-workers").expect("scratch");
        let executable = scratch.join("blinker");
        std::fs::write(&executable, b"one").expect("written");
        let names: Vec<(String, String)> = (0..WORKERS)
            .map(|worker| socket_name(&executable, worker))
            .collect();
        let prefix = &names[0].0;
        assert!(names.iter().all(|(p, _)| p == prefix), "prefixes diverged");
        let distinct: std::collections::BTreeSet<&String> =
            names.iter().map(|(_, name)| name).collect();
        assert_eq!(distinct.len(), WORKERS, "two workers share a socket");
        // The budget from `socket_name`'s comment, checked rather than
        // asserted in prose: `sockaddr_un` is 104 bytes on macOS and the
        // temporary directory is most of it.
        let longest = std::env::temp_dir().join(&names[WORKERS - 1].1);
        assert!(
            longest.as_os_str().len() < 104,
            "the socket path no longer fits: {}",
            longest.display()
        );
    }

    /// Routing decides *where*, and has to give the same answer every time for
    /// one output — including when the spelling changes but the file does not.
    #[test]
    fn one_output_always_routes_to_one_worker() {
        let argv = |output: &str| vec!["-o".to_string(), output.to_string()];
        let direct = worker_of(&argv("/a/b/prog"));
        assert_eq!(direct, worker_of(&argv("/a/./b/prog")));
        assert_eq!(direct, worker_of(&argv("/a/x/../b/prog")));
        assert!(direct < WORKERS);
        // And an invocation with no output still routes somewhere rather than
        // failing: routing never decides whether a link happens.
        assert!(worker_of(&["a.o".to_string()]) < WORKERS);
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
