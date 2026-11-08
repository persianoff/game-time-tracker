//! Optional LAN HTTP endpoint (`config.http_port`), off by default.
//!
//! One std `TcpListener` thread, blocking `accept` (no CPU or allocation while
//! idle), one request at a time, GET/HEAD only, `Connection: close`. Only the
//! request line is read, with a small fixed buffer, a per-read timeout and an
//! overall deadline so a slow client cannot block the thread for long.
//!
//! Routes (query strings and fragments are ignored):
//! - `/`, `/index.html`: the viewer (`viewer/index.html`, embedded)
//! - `/sessions.json`: data contract v1, including the open session
//! - `/daily.json`: `{version, generated, days: [{date, game, minutes}]}`
//! - `/gametime.ics`: calendar
//!
//! Data is built from the store at request time. The open session comes from a
//! [`Live`] snapshot that the poll thread refreshes at checkpoint time; the lock
//! is held only to clone it, and the poll thread never waits on this thread.
//!
//! The Windows Firewall is not touched: the user must allow the port.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::store::{SessionRecord, Store};
use crate::{export, log, win32};

const VIEWER_HTML: &str = include_str!("../viewer/index.html");

/// Bytes of the request kept for routing (the request line is in here).
const REQ_BUF: usize = 2048;
/// Total request size accepted (line, headers, anything else); more gets a 431.
const REQ_MAX: usize = 16 * 1024;
/// After responding, how much / how long to drain unread request bytes so the
/// close is a FIN, not a reset that would discard the response.
const DRAIN_MAX: usize = 64 * 1024;
const DRAIN_TIME: Duration = Duration::from_secs(1);
const DRAIN_READ_TIMEOUT: Duration = Duration::from_millis(100);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
/// Overall time allowed to receive the request line.
const REQ_DEADLINE: Duration = Duration::from_secs(5);

/// The open session as of the last checkpoint (`None` when idle).
pub type Live = Arc<Mutex<Option<SessionRecord>>>;

pub fn new_live() -> Live {
    Arc::new(Mutex::new(None))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Index,
    Sessions,
    Daily,
    Ics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    /// Serve the route; `head` omits the body.
    Serve(Route, bool),
    /// Unknown path; the flag is HEAD (no body).
    NotFound(bool),
    MethodNotAllowed,
    BadRequest,
    /// Request larger than [`REQ_MAX`].
    TooLarge,
}

/// Path (query and fragment stripped) to route.
pub fn route_path(target: &str) -> Option<Route> {
    let path = target.split(['?', '#']).next().unwrap_or("");
    match path {
        "/" | "/index.html" => Some(Route::Index),
        "/sessions.json" => Some(Route::Sessions),
        "/daily.json" => Some(Route::Daily),
        "/gametime.ics" => Some(Route::Ics),
        _ => None,
    }
}

/// Route the request line (`METHOD target HTTP/x.y`) found at the start of `req`.
pub fn route_request(req: &[u8]) -> Routed {
    let end = req
        .iter()
        .position(|&b| b == b'\n' || b == b'\r')
        .unwrap_or(req.len());
    let Ok(line) = std::str::from_utf8(&req[..end]) else {
        return Routed::BadRequest;
    };
    let mut parts = line.split(' ').filter(|p| !p.is_empty());
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Routed::BadRequest;
    };
    if !version.starts_with("HTTP/") || !target.starts_with('/') {
        return Routed::BadRequest;
    }
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) {
        return Routed::BadRequest;
    }
    let head = match method {
        "GET" => false,
        "HEAD" => true,
        _ => return Routed::MethodNotAllowed,
    };
    match route_path(target) {
        Some(r) => Routed::Serve(r, head),
        None => Routed::NotFound(head),
    }
}

/// A complete response (status line, headers; body appended unless `head`).
fn response(status: &str, ctype: &str, body: &[u8], head: bool, extra: &str) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\nCache-Control: no-store\r\n{extra}Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    if !head {
        out.extend_from_slice(body);
    }
    out
}

fn text_error(status: &str, extra: &str, head: bool) -> Vec<u8> {
    response(
        status,
        "text/plain; charset=utf-8",
        status.as_bytes(),
        head,
        extra,
    )
}

/// Build the response bytes for a routed request. `open` is the live snapshot.
pub fn handle(routed: Routed, store: &Store, open: Option<&SessionRecord>, now: u64) -> Vec<u8> {
    match routed {
        Routed::BadRequest => text_error("400 Bad Request", "", false),
        Routed::NotFound(head) => text_error("404 Not Found", "", head),
        Routed::TooLarge => text_error("431 Request Header Fields Too Large", "", false),
        Routed::MethodNotAllowed => {
            text_error("405 Method Not Allowed", "Allow: GET, HEAD\r\n", false)
        }
        Routed::Serve(Route::Index, head) => response(
            "200 OK",
            "text/html; charset=utf-8",
            VIEWER_HTML.as_bytes(),
            head,
            "",
        ),
        Routed::Serve(route, head) => match export::Loaded::load(store, open) {
            Ok(l) => {
                // Only the requested representation is built.
                let (ctype, body) = match route {
                    Route::Sessions => ("application/json; charset=utf-8", l.sessions_json(now)),
                    Route::Daily => ("application/json; charset=utf-8", l.daily_json(now)),
                    _ => ("text/calendar; charset=utf-8", l.ics(now)),
                };
                response("200 OK", ctype, &body, head, "")
            }
            Err(e) => {
                log::log_line(&format!("http: cannot read sessions: {e}"));
                text_error("500 Internal Server Error", "", head)
            }
        },
    }
}

/// Result of [`read_request`].
struct Request {
    /// Bytes stored in the routing buffer.
    stored: usize,
    too_large: bool,
}

/// Read until the end of the headers (`\r\n\r\n` or `\n\n`), EOF, a timeout
/// or the overall deadline. The first [`REQ_BUF`] bytes are kept for routing;
/// the rest is read and discarded, up to [`REQ_MAX`] in total.
fn read_request(stream: &mut TcpStream, req: &mut [u8; REQ_BUF]) -> Request {
    let start = Instant::now();
    let mut chunk = [0u8; 1024];
    let (mut stored, mut total) = (0usize, 0usize);
    let (mut state, mut prev) = (0usize, 0u8);
    const END: [u8; 4] = *b"\r\n\r\n";
    while let Some(left) = REQ_DEADLINE.checked_sub(start.elapsed()) {
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT.min(left).max(Duration::from_millis(1))));
        let k = match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(k) => k,
        };
        let keep = k.min(REQ_BUF - stored);
        req[stored..stored + keep].copy_from_slice(&chunk[..keep]);
        stored += keep;
        total += k;
        let mut done = false;
        for &b in &chunk[..k] {
            state = if b == END[state] {
                state + 1
            } else if b == b'\r' {
                1
            } else {
                0
            };
            if state == 4 || (b == b'\n' && prev == b'\n') {
                done = true;
                break;
            }
            prev = b;
        }
        if done {
            break;
        }
        if total > REQ_MAX {
            return Request {
                stored,
                too_large: true,
            };
        }
    }
    Request {
        stored,
        too_large: false,
    }
}

/// Send FIN, then read and discard whatever the client still sends (bounded)
/// so closing does not reset the connection.
fn drain(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(DRAIN_READ_TIMEOUT));
    let start = Instant::now();
    let mut sink = [0u8; 1024];
    let mut n = 0;
    while n < DRAIN_MAX && start.elapsed() < DRAIN_TIME {
        match stream.read(&mut sink) {
            Ok(0) | Err(_) => break,
            Ok(k) => n += k,
        }
    }
}

fn serve_one(mut stream: TcpStream, store: &Store, live: &Live) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut buf = [0u8; REQ_BUF];
    let req = read_request(&mut stream, &mut buf);
    if req.stored == 0 && !req.too_large {
        return;
    }
    let routed = if req.too_large {
        Routed::TooLarge
    } else {
        route_request(&buf[..req.stored])
    };
    // Lock only to clone.
    let open = live.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let out = handle(routed, store, open.as_ref(), win32::now_utc_secs());
    let _ = stream.write_all(&out);
    let _ = stream.flush();
    drop(out);
    if matches!(routed, Routed::Serve(r, _) if r != Route::Index) {
        win32::trim_working_set();
    }
    drain(&mut stream);
}

/// Accept loop. Blocks forever; run it on its own thread.
pub fn serve(listener: TcpListener, store: Store, live: Live) {
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => serve_one(stream, &store, &live),
            // e.g. a connection reset before accept, or fd exhaustion: don't spin.
            Err(_) => std::thread::sleep(Duration::from_millis(200)),
        }
    }
}

/// Bind `0.0.0.0:port` and serve on a background thread. A bind failure is
/// returned (the caller logs it; tracking continues). The thread is detached
/// and does not keep the process alive.
pub fn spawn(port: u16, store: Store, live: Live) -> io::Result<()> {
    if port == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "http_port 0 is not allowed",
        ));
    }
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    std::thread::Builder::new()
        .name("http".into())
        .stack_size(256 * 1024)
        .spawn(move || serve(listener, store, live))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_route() {
        assert_eq!(route_path("/"), Some(Route::Index));
        assert_eq!(route_path("/index.html"), Some(Route::Index));
        assert_eq!(route_path("/sessions.json?x=1"), Some(Route::Sessions));
        assert_eq!(route_path("/daily.json#a"), Some(Route::Daily));
        assert_eq!(route_path("/gametime.ics"), Some(Route::Ics));
        assert_eq!(route_path("/?src=x"), Some(Route::Index));
        assert_eq!(route_path("/nope"), None);
        assert_eq!(route_path("/sessions.json/"), None);
    }

    #[test]
    fn request_lines() {
        let r = |s: &str| route_request(s.as_bytes());
        assert_eq!(
            r("GET /sessions.json HTTP/1.1\r\nHost: x\r\n\r\n"),
            Routed::Serve(Route::Sessions, false)
        );
        assert_eq!(r("GET / HTTP/1.0\n"), Routed::Serve(Route::Index, false));
        assert_eq!(
            r("HEAD /daily.json?z HTTP/1.1\r\n"),
            Routed::Serve(Route::Daily, true)
        );
        assert_eq!(r("GET /missing HTTP/1.1\r\n"), Routed::NotFound(false));
        assert_eq!(r("HEAD /missing HTTP/1.1\r\n"), Routed::NotFound(true));
        assert_eq!(
            r("POST /sessions.json HTTP/1.1\r\n"),
            Routed::MethodNotAllowed
        );
        assert_eq!(r("DELETE / HTTP/1.1\r\n"), Routed::MethodNotAllowed);
        // Method check precedes path lookup.
        assert_eq!(r("PUT /missing HTTP/1.1\r\n"), Routed::MethodNotAllowed);
        assert_eq!(r("GET /\r\n"), Routed::BadRequest);
        assert_eq!(r("garbage"), Routed::BadRequest);
        assert_eq!(r(""), Routed::BadRequest);
        assert_eq!(r("get / HTTP/1.1\r\n"), Routed::BadRequest);
        assert_eq!(r("GET index HTTP/1.1\r\n"), Routed::BadRequest);
        assert_eq!(r("GET / FTP/1.1\r\n"), Routed::BadRequest);
        assert_eq!(r("GET / HTTP/1.1 extra\r\n"), Routed::BadRequest);
        assert_eq!(route_request(&[0xff, 0xfe, b'\n']), Routed::BadRequest);
    }

    fn text(v: &[u8]) -> String {
        String::from_utf8_lossy(v).into_owned()
    }

    #[test]
    fn error_responses() {
        let dir = std::env::temp_dir().join(format!("gametime-http-unit-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let nf = text(&handle(Routed::NotFound(false), &store, None, 0));
        assert!(nf.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(nf.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(nf.contains("Cache-Control: no-store\r\n"));
        let m = text(&handle(Routed::MethodNotAllowed, &store, None, 0));
        assert!(m.starts_with("HTTP/1.1 405 "));
        assert!(m.contains("Allow: GET, HEAD\r\n"));
        let hn = text(&handle(Routed::NotFound(true), &store, None, 0));
        assert!(hn.starts_with("HTTP/1.1 404 ") && hn.ends_with("\r\n\r\n"));
        let t = text(&handle(Routed::TooLarge, &store, None, 0));
        assert!(t.starts_with("HTTP/1.1 431 "));
        let b = text(&handle(Routed::BadRequest, &store, None, 0));
        assert!(b.starts_with("HTTP/1.1 400 "));
        let h = text(&handle(Routed::Serve(Route::Index, true), &store, None, 0));
        assert!(h.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(h.ends_with("\r\n\r\n"), "HEAD has no body");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
