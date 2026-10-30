//! Test-only scripted HTTP/1.1 server on `127.0.0.1:0` (SPEC 12.2), modelled on antarctical's
//! loopback test in `google/api.rs`. It answers connections in order with the scripted
//! replies (`Connection: close`, so ureq opens a new connection per request) and records each
//! request. A reply may be chosen by path with [`Reply::for_path`]; unmatched requests get a
//! 599 so a test fails loudly instead of hanging. A [`Reply::sticky`] reply answers its path
//! any number of times, after the path's one-off replies are used up; it serves requests whose
//! count depends on timing (a poll), and the server stops once every one-off reply is used.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// One recorded request.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    /// Path and query, as sent.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    pub fn form(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(&self.body)
            .into_owned()
            .collect()
    }
}

/// One scripted reply.
#[derive(Clone, Debug)]
pub struct Reply {
    /// Only answer a request to this path (the first unused matching reply wins).
    path: Option<String>,
    /// Never used up.
    sticky: bool,
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            path: None,
            sticky: false,
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
        }
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn for_path(mut self, path: &str) -> Self {
        self.path = Some(path.into());
        self
    }

    /// Answers its path every time it has no one-off reply left.
    pub fn sticky(mut self) -> Self {
        self.sticky = true;
        self
    }
}

pub struct FakeServer {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<Recorded>>>,
    handle: Option<JoinHandle<()>>,
}

impl FakeServer {
    /// Serves `replies` (each once) until all are used or 10 s pass without a request.
    pub fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let thread_log = log.clone();
        let handle = std::thread::spawn(move || serve(listener, replies, thread_log));
        Self {
            addr,
            log,
            handle: Some(handle),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    /// The requests so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.log.lock().unwrap().clone()
    }

    /// Waits until the script is used up (or the server gave up) and returns every request.
    pub fn finish(mut self) -> Vec<Recorded> {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.requests()
    }
}

fn serve(listener: TcpListener, mut replies: Vec<Reply>, log: Arc<Mutex<Vec<Recorded>>>) {
    let mut idle_since = Instant::now();
    while replies.iter().any(|r| !r.sticky) && idle_since.elapsed() < Duration::from_secs(10) {
        let mut stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(_) => return,
        };
        idle_since = Instant::now();
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let Some(req) = receive(&mut stream) else {
            continue;
        };
        let for_path =
            |r: &Reply, sticky: bool| r.sticky == sticky && r.path.as_deref() == Some(req.path());
        let pick = replies
            .iter()
            .position(|r| for_path(r, false))
            .or_else(|| replies.iter().position(|r| for_path(r, true)))
            .or_else(|| replies.iter().position(|r| r.path.is_none() && !r.sticky));
        let reply = match pick {
            Some(i) if replies[i].sticky => replies[i].clone(),
            Some(i) => replies.remove(i),
            None => Reply::json(599, r#"{"error":{"message":"unscripted request"}}"#),
        };
        log.lock().unwrap().push(req);
        respond(&mut stream, &reply);
    }
}

fn receive(stream: &mut TcpStream) -> Option<Recorded> {
    let mut head = Vec::new();
    let mut byte = [0u8];
    while !head.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).ok()?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let len = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    stream.read_exact(&mut body).ok()?;
    Some(Recorded {
        method,
        target,
        headers,
        body,
    })
}

fn respond(stream: &mut TcpStream, reply: &Reply) {
    let mut out = format!("HTTP/1.1 {} X\r\n", reply.status);
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.body.len(),
        reply.body
    ));
    let _ = stream.write_all(out.as_bytes());
    let _ = stream.flush();
}
