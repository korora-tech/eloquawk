//! Single instance. The first Eloquawk takes an exclusive lock on
//! `$XDG_RUNTIME_DIR/eloquawk.lock` and listens on the 0600 socket `eloquawk.sock` beside it;
//! a later launch finds the lock taken, recognises the running instance with `hello`, asks it
//! to `show` its window or to `compose` a `mailto:` URL, and exits.
//!
//! The protocol is one JSON object per line, request then reply:
//! `{"method":"hello"}` → `{"app":"eloquawk","pid":N}`, `{"method":"show"}` → `{"ok":true}`,
//! `{"method":"compose","mailto":"mailto:…"}` → `{"ok":true}`. A mailto URL is only ever
//! handed to the whitelisted parser in `mail/mailto.rs`, which opens a compose window and
//! never sends. `show` and `compose` may carry `"token"`, the launching process's
//! `XDG_ACTIVATION_TOKEN`, which the running instance spends on raising its window (on Wayland
//! a window may only take focus with a token the compositor issued for a user action).
//!
//! Without a private `$XDG_RUNTIME_DIR` the whole mechanism is skipped: there is no fallback
//! to `/tmp`, where another user could squat the names. `--demo --instance` uses
//! `eloquawk-demo.lock` and `eloquawk-demo.sock`, so it never meets a real instance.
//! Extracted from antarctical's `dankcal.rs`.

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// What a launch asks of the running instance, and what the running instance is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Bring the main window forward.
    Show,
    /// Open a compose window prefilled from this `mailto:` URL.
    Compose(String),
}

impl Request {
    /// The request for a launch with an optional positional `mailto:` URL.
    pub fn for_launch(mailto: Option<&str>) -> Self {
        match mailto {
            Some(url) => Self::Compose(url.to_string()),
            None => Self::Show,
        }
    }
}

/// A request handed over by a later launch, with the activation token it was started with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handoff {
    pub request: Request,
    pub token: Option<String>,
}

/// The longest activation token accepted; real ones are a few dozen characters.
const MAX_TOKEN: usize = 512;

/// True for a plausible xdg-activation token: printable ASCII without spaces.
pub fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= MAX_TOKEN && token.bytes().all(|b| b.is_ascii_graphic())
}

pub enum Claim {
    /// This launch is the instance; serve the socket with [`Listener::serve`].
    Bound(Listener),
    /// Another Eloquawk is running and has been asked to act; this launch should exit.
    HandedOff,
    /// Running without single-instance support, for the reason given.
    Unavailable(String),
}

/// How long a launch that finds the lock taken waits for the holder to answer.
const HAND_OFF_WAIT: Duration = Duration::from_secs(3);
/// Reads and writes on a socket connection give up after this long.
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest request line accepted (a mailto URL with a body fits easily).
const MAX_LINE: usize = 64 * 1024;
/// The longest `mailto:` URL handed to the composer.
const MAX_MAILTO: usize = 32 * 1024;

/// The lock and socket file names.
fn names(demo: bool) -> (&'static str, &'static str) {
    if demo {
        ("eloquawk-demo.lock", "eloquawk-demo.sock")
    } else {
        ("eloquawk.lock", "eloquawk.sock")
    }
}

/// Finds a running Eloquawk and hands `request` to it, with this launch's
/// `XDG_ACTIVATION_TOKEN`, or becomes the instance.
pub fn claim(demo: bool, request: &Request) -> Claim {
    let handoff = Handoff {
        request: request.clone(),
        token: std::env::var("XDG_ACTIVATION_TOKEN")
            .ok()
            .filter(|t| valid_token(t)),
    };
    match runtime_dir(std::env::var_os("XDG_RUNTIME_DIR")) {
        Some(dir) => claim_in(&dir, demo, &handoff),
        None => Claim::Unavailable("XDG_RUNTIME_DIR is unset or not private".into()),
    }
}

/// [`claim`] in `dir`. Launches serialise on the lock: its holder replaces a stale socket and
/// binds; a launch that finds it taken only probes the socket, retrying briefly while the
/// holder binds.
fn claim_in(dir: &Path, demo: bool, request: &Handoff) -> Claim {
    let (lock_name, sock_name) = names(demo);
    let lock_path = dir.join(lock_name);
    let sock_path = dir.join(sock_name);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(&lock_path);
    let lock = match lock {
        Ok(f) => f,
        Err(e) => {
            return Claim::Unavailable(format!("cannot open {}: {e}", lock_path.display()));
        }
    };
    let deadline = Instant::now() + HAND_OFF_WAIT;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Claim::Unavailable(format!("cannot lock {}: {e}", lock_path.display()));
            }
        }
        if let Some(claim) = hand_off(&sock_path, request) {
            return claim;
        }
        // The holder may still be binding, or be on its way out (its socket already gone);
        // the lock is tried again each round, so a launch takes over once the holder exits.
        if Instant::now() >= deadline {
            return Claim::Unavailable(
                "another Eloquawk holds the lock but does not answer".into(),
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // Holding the lock: a socket nobody answers on is stale, and `bind` replaces it.
    match bind(&sock_path, lock) {
        Ok(l) => Claim::Bound(l),
        Err(e) => Claim::Unavailable(format!("cannot listen on {}: {e}", sock_path.display())),
    }
}

/// Hands `request` to the Eloquawk listening on `path`. `None` when nothing answers as one.
fn hand_off(path: &Path, handoff: &Handoff) -> Option<Claim> {
    let Probe::Eloquawk(mut client, pid) = probe(path) else {
        return None;
    };
    eprintln!("eloquawk: already running (pid {pid}); handing over");
    let mut line = match &handoff.request {
        Request::Show => json!({"method": "show"}),
        Request::Compose(url) => json!({"method": "compose", "mailto": url}),
    };
    if let Some(token) = &handoff.token {
        line["token"] = json!(token);
    }
    match client.call(&line) {
        Ok(v) if v["ok"] == true => {}
        Ok(v) => eprintln!(
            "eloquawk: the running instance refused the request: {}",
            v["error"].as_str().unwrap_or("no reason given")
        ),
        Err(e) => eprintln!("eloquawk: the running instance did not answer: {e}"),
    }
    Some(Claim::HandedOff)
}

/// `$XDG_RUNTIME_DIR` when it is set, absolute, ours and private (0700).
fn runtime_dir(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let dir = PathBuf::from(value.filter(|v| !v.is_empty())?);
    if !dir.is_absolute() {
        return None;
    }
    let meta = std::fs::metadata(&dir).ok()?;
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    (meta.is_dir() && meta.uid() == uid && meta.mode() & 0o077 == 0).then_some(dir)
}

/// The bound socket and the lock that guards it, ready for [`Listener::serve`].
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    /// (device, inode) of the socket file, so shutting down never removes someone else's.
    file: (u64, u64),
    lock: File,
}

/// Binds `path` (0600), replacing a stale socket nobody listens on.
fn bind(path: &Path, lock: File) -> io::Result<Listener> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a file that is not a socket is in the way",
            ));
        }
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "something already listens there",
            ));
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let meta = std::fs::metadata(path)?;
    Ok(Listener {
        listener,
        path: path.to_path_buf(),
        file: (meta.dev(), meta.ino()),
        lock,
    })
}

impl Listener {
    /// Serves the socket on its own threads (`eloquawk-instance`, one per connection),
    /// sending each request to `actions`. The lock is held until the returned [`Server`]
    /// is dropped.
    pub fn serve(self, actions: async_channel::Sender<Handoff>) -> Server {
        let stopped = Arc::new(AtomicBool::new(false));
        let server = Server {
            path: self.path,
            file: self.file,
            stopped: stopped.clone(),
            lock: self.lock,
        };
        let listener = self.listener;
        let spawned = std::thread::Builder::new()
            .name("eloquawk-instance".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let actions = actions.clone();
                    let spawned = std::thread::Builder::new()
                        .name("eloquawk-instance".into())
                        .spawn(move || {
                            if let Err(e) = serve(stream, &actions) {
                                eprintln!("eloquawk: instance connection failed: {}", e.kind());
                            }
                        });
                    if let Err(e) = spawned {
                        eprintln!("eloquawk: could not serve an instance connection: {e}");
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("eloquawk: could not start the instance socket: {e}");
        }
        server
    }
}

/// The serving socket. [`Server::shutdown`] removes it and releases the lock (as does drop).
pub struct Server {
    path: PathBuf,
    file: (u64, u64),
    stopped: Arc<AtomicBool>,
    lock: File,
}

impl Server {
    /// Stops accepting and removes the socket file, if it is still ours.
    pub fn shutdown(&self) {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return;
        }
        // Wakes the accept loop so it sees `stopped`.
        let _ = UnixStream::connect(&self.path);
        if std::fs::metadata(&self.path).is_ok_and(|m| (m.dev(), m.ino()) == self.file) {
            let _ = std::fs::remove_file(&self.path);
        }
        // With the socket gone nothing answers here any more, so let the next launch become
        // the instance at once rather than wait for this process to exit.
        let _ = self.lock.unlock();
    }
}

enum Line {
    Text,
    Eof,
    TooLong,
}

/// Reads one line (without its newline) into `buf`, refusing lines over [`MAX_LINE`].
fn read_line(reader: &mut impl BufRead, buf: &mut Vec<u8>) -> io::Result<Line> {
    buf.clear();
    let n = reader
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', buf)?;
    if n == 0 {
        return Ok(Line::Eof);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    } else if buf.len() > MAX_LINE {
        return Ok(Line::TooLong);
    }
    Ok(Line::Text)
}

fn write_line(writer: &mut UnixStream, v: &Value) -> io::Result<()> {
    let mut line = serde_json::to_vec(v).map_err(io::Error::other)?;
    line.push(b'\n');
    writer.write_all(&line)
}

fn serve(stream: UnixStream, actions: &async_channel::Sender<Handoff>) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    let mut buf = vec![];
    loop {
        match read_line(&mut reader, &mut buf)? {
            Line::Text => {}
            Line::Eof => return Ok(()),
            Line::TooLong => {
                return write_line(&mut writer, &json!({"error": "request too long"}));
            }
        }
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let reply = match serde_json::from_slice::<Value>(&buf) {
            Ok(req) => respond(&req, actions),
            Err(_) => json!({"error": "invalid JSON"}),
        };
        write_line(&mut writer, &reply)?;
    }
}

/// True for a `mailto:` URL of a sensible length; anything else is refused.
fn is_mailto(url: &str) -> bool {
    url.len() <= MAX_MAILTO
        && url
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("mailto:"))
}

fn respond(req: &Value, actions: &async_channel::Sender<Handoff>) -> Value {
    let request = match req["method"].as_str().unwrap_or_default() {
        "hello" => return json!({"app": "eloquawk", "pid": std::process::id()}),
        "show" => Request::Show,
        "compose" => match req["mailto"].as_str() {
            Some(url) if is_mailto(url) => Request::Compose(url.to_string()),
            _ => return json!({"error": "not a mailto URL"}),
        },
        other => return json!({"error": format!("unknown method: {other}")}),
    };
    // A malformed token is dropped, not refused: the request still stands without focus.
    let token = req["token"]
        .as_str()
        .filter(|t| valid_token(t))
        .map(str::to_string);
    match actions.send_blocking(Handoff { request, token }) {
        Ok(()) => json!({"ok": true}),
        Err(_) => json!({"error": "shutting down"}),
    }
}

enum Probe {
    /// A running Eloquawk, and its pid.
    Eloquawk(Client, u64),
    /// Nothing, or something that is not Eloquawk.
    Other,
}

/// A blocking request connection, for probing and handing over.
struct Client {
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(Self {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        })
    }

    fn call(&mut self, req: &Value) -> io::Result<Value> {
        write_line(&mut self.writer, req)?;
        let mut buf = vec![];
        match read_line(&mut self.reader, &mut buf)? {
            Line::Text => serde_json::from_slice(&buf).map_err(io::Error::other),
            Line::Eof | Line::TooLong => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
}

fn probe(path: &Path) -> Probe {
    let Ok(mut client) = Client::connect(path) else {
        return Probe::Other;
    };
    match client.call(&json!({"method": "hello"})) {
        Ok(v) if v["app"] == "eloquawk" => {
            let pid = v["pid"].as_u64().unwrap_or_default();
            Probe::Eloquawk(client, pid)
        }
        _ => Probe::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private (0700) temporary directory, removed on drop.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("eqk-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ask(request: Request) -> Handoff {
        Handoff {
            request,
            token: None,
        }
    }

    fn bound(claim: Claim) -> Listener {
        match claim {
            Claim::Bound(l) => l,
            Claim::HandedOff => panic!("expected Bound, got HandedOff"),
            Claim::Unavailable(why) => panic!("expected Bound, got Unavailable({why})"),
        }
    }

    #[test]
    fn a_second_launch_hands_over_to_the_first() {
        let dir = TempDir::new("handoff");
        let listener = bound(claim_in(&dir.0, false, &ask(Request::Show)));
        let sock = dir.0.join("eloquawk.sock");
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let (tx, actions) = async_channel::unbounded();
        let server = listener.serve(tx);

        assert!(matches!(
            claim_in(&dir.0, false, &ask(Request::Show)),
            Claim::HandedOff
        ));
        assert_eq!(actions.recv_blocking().unwrap(), ask(Request::Show));

        let url = "mailto:x@y?subject=Hi&attach=/etc/passwd";
        let compose = Handoff {
            request: Request::for_launch(Some(url)),
            token: Some("enemSFrvx80JIi7oSs5qPp9L9veGmkFI".into()),
        };
        assert!(matches!(
            claim_in(&dir.0, false, &compose),
            Claim::HandedOff
        ));
        assert_eq!(actions.recv_blocking().unwrap(), compose);

        // The demo names are separate: a demo launch does not see the real instance.
        let demo = bound(claim_in(&dir.0, true, &ask(Request::Show)));
        assert!(dir.0.join("eloquawk-demo.sock").exists());
        drop(demo);

        server.shutdown();
        assert!(!sock.exists(), "shutdown removes the socket");
        // Shutdown releases the lock, so a launch while this process is still exiting
        // becomes the instance instead of waiting on a holder that no longer answers.
        drop(bound(claim_in(&dir.0, false, &ask(Request::Show))));
        drop(server);
    }

    #[test]
    fn a_launch_takes_over_when_the_silent_holder_exits() {
        let dir = TempDir::new("takeover");
        let held = File::create(dir.0.join("eloquawk.lock")).unwrap();
        held.try_lock().unwrap();
        // The holder answers nothing (its socket is gone) and exits a moment later.
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        drop(bound(claim_in(&dir.0, false, &ask(Request::Show))));
        releaser.join().unwrap();
    }

    #[test]
    fn a_stale_socket_is_replaced() {
        let dir = TempDir::new("stale");
        let sock = dir.0.join("eloquawk.sock");
        drop(UnixListener::bind(&sock).unwrap());
        assert!(sock.exists());
        assert!(matches!(probe(&sock), Probe::Other));
        let listener = bound(claim_in(&dir.0, false, &ask(Request::Show)));
        let (tx, _actions) = async_channel::unbounded();
        let server = listener.serve(tx);
        assert!(
            matches!(probe(&sock), Probe::Eloquawk(_, pid) if pid == u64::from(std::process::id()))
        );
        server.shutdown();
    }

    #[test]
    fn a_file_in_the_way_is_not_removed() {
        let dir = TempDir::new("file");
        let sock = dir.0.join("eloquawk.sock");
        std::fs::write(&sock, b"not a socket").unwrap();
        assert!(matches!(
            claim_in(&dir.0, false, &ask(Request::Show)),
            Claim::Unavailable(_)
        ));
        assert_eq!(std::fs::read(&sock).unwrap(), b"not a socket");
    }

    #[test]
    fn the_server_refuses_what_is_not_a_mailto_url() {
        let dir = TempDir::new("refuse");
        let listener = bound(claim_in(&dir.0, false, &ask(Request::Show)));
        let (tx, actions) = async_channel::unbounded();
        let server = listener.serve(tx);
        let mut c = Client::connect(&dir.0.join("eloquawk.sock")).unwrap();
        for url in ["file:///etc/passwd", "https://example.com", "mailto"] {
            let v = c
                .call(&json!({"method": "compose", "mailto": url}))
                .unwrap();
            assert_eq!(v["error"], "not a mailto URL", "{url}");
        }
        let long = format!("mailto:a@b?body={}", "x".repeat(MAX_MAILTO));
        let v = c
            .call(&json!({"method": "compose", "mailto": long}))
            .unwrap();
        assert_eq!(v["error"], "not a mailto URL");
        let v = c.call(&json!({"method": "compose"})).unwrap();
        assert_eq!(v["error"], "not a mailto URL");
        let v = c.call(&json!({"method": "nope"})).unwrap();
        assert_eq!(v["error"], "unknown method: nope");
        let v = c
            .call(&json!({"method": "compose", "mailto": "MAILTO:a@b", "token": "bad token"}))
            .unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(
            actions.recv_blocking().unwrap(),
            ask(Request::Compose("MAILTO:a@b".into())),
            "a malformed token is dropped"
        );
        assert!(actions.try_recv().is_err());
        server.shutdown();
    }

    #[test]
    fn overlong_lines_close_the_connection() {
        let dir = TempDir::new("long");
        let listener = bound(claim_in(&dir.0, false, &ask(Request::Show)));
        let (tx, _actions) = async_channel::unbounded();
        let server = listener.serve(tx);
        let mut c = Client::connect(&dir.0.join("eloquawk.sock")).unwrap();
        c.writer.write_all(&vec![b'x'; MAX_LINE + 10]).unwrap();
        let mut buf = vec![];
        assert!(matches!(
            read_line(&mut c.reader, &mut buf).unwrap(),
            Line::Text
        ));
        let v: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(v["error"], "request too long");
        assert!(matches!(
            read_line(&mut c.reader, &mut buf).unwrap(),
            Line::Eof
        ));
        server.shutdown();
    }

    #[test]
    fn the_runtime_dir_must_be_set_absolute_and_private() {
        assert_eq!(runtime_dir(None), None);
        assert_eq!(runtime_dir(Some("".into())), None);
        assert_eq!(runtime_dir(Some("relative/dir".into())), None);
        let dir = TempDir::new("runtime");
        assert_eq!(runtime_dir(Some(dir.0.clone().into())), Some(dir.0.clone()));
        std::fs::set_permissions(&dir.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(runtime_dir(Some(dir.0.clone().into())), None);
    }

    #[test]
    fn tokens_are_printable_ascii_of_sensible_length() {
        assert!(valid_token("enemSFrvx80JIi7oSs5qPp9L9veGmkFI"));
        assert!(valid_token("kwin-1234_abc.def"));
        assert!(!valid_token(""));
        assert!(!valid_token("has space"));
        assert!(!valid_token("line\nbreak"));
        assert!(!valid_token("café"));
        assert!(!valid_token(&"x".repeat(MAX_TOKEN + 1)));
    }

    #[test]
    fn launch_requests() {
        assert_eq!(Request::for_launch(None), Request::Show);
        assert_eq!(
            Request::for_launch(Some("mailto:a@b")),
            Request::Compose("mailto:a@b".into())
        );
    }
}
