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

/// Longest accepted request line (plus whatever else arrived in the first reads).
const REQ_BUF: usize = 2048;
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
    NotFound,
    MethodNotAllowed,
    BadRequest,
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
        None => Routed::NotFound,
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
        Routed::NotFound => text_error("404 Not Found", "", false),
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
        Routed::Serve(route, head) => match export::build_payload(store, open, now) {
            Ok(p) => {
                let (ctype, body) = match route {
                    Route::Sessions => ("application/json; charset=utf-8", &p.json),
                    Route::Daily => ("application/json; charset=utf-8", &p.daily_json),
                    _ => ("text/calendar; charset=utf-8", &p.ics),
                };
                response("200 OK", ctype, body, head, "")
            }
            Err(e) => {
                log::log_line(&format!("http: cannot read sessions: {e}"));
                text_error("500 Internal Server Error", "", head)
            }
        },
    }
}

/// Read until the first line is complete, the buffer is full, EOF, a timeout
/// or the overall deadline. Returns the bytes read.
fn read_request(stream: &mut TcpStream, buf: &mut [u8; REQ_BUF]) -> usize {
    let start = Instant::now();
    let mut n = 0;
    while n < buf.len() && start.elapsed() < REQ_DEADLINE {
        match stream.read(&mut buf[n..]) {
            Ok(0) | Err(_) => break,
            Ok(k) => {
                n += k;
                if buf[..n].contains(&b'\n') {
                    break;
                }
            }
        }
    }
    n
}

fn serve_one(mut stream: TcpStream, store: &Store, live: &Live) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut buf = [0u8; REQ_BUF];
    let n = read_request(&mut stream, &mut buf);
    if n == 0 {
        return;
    }
    let routed = route_request(&buf[..n]);
    // Lock only to clone.
    let open = live.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let out = handle(routed, store, open.as_ref(), win32::now_utc_secs());
    let _ = stream.write_all(&out);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
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
        assert_eq!(r("GET /missing HTTP/1.1\r\n"), Routed::NotFound);
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
        let nf = text(&handle(Routed::NotFound, &store, None, 0));
        assert!(nf.starts_with("HTTP/1.1 404 Not Found\r\n"));
        assert!(nf.contains("Access-Control-Allow-Origin: *\r\n"));
        assert!(nf.contains("Cache-Control: no-store\r\n"));
        let m = text(&handle(Routed::MethodNotAllowed, &store, None, 0));
        assert!(m.starts_with("HTTP/1.1 405 "));
        assert!(m.contains("Allow: GET, HEAD\r\n"));
        let b = text(&handle(Routed::BadRequest, &store, None, 0));
        assert!(b.starts_with("HTTP/1.1 400 "));
        let h = text(&handle(Routed::Serve(Route::Index, true), &store, None, 0));
        assert!(h.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(h.ends_with("\r\n\r\n"), "HEAD has no body");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
