//! Request-framing robustness: the client must always receive a complete
//! response (no connection reset from unread request bytes).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use gametime::http;
use gametime::store::{SessionRecord, Store};

fn start_server(tag: &str) -> (u16, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("gametime-http-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    store
        .append(&SessionRecord {
            game: "G".into(),
            source: "steam".into(),
            exe: String::new(),
            start_utc: 1_767_323_045,
            end_utc: 1_767_323_345,
            active_s: 300,
        })
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let live = http::new_live();
    std::thread::spawn(move || http::serve(listener, store, live));
    (port, dir)
}

fn read_all(mut s: TcpStream) -> String {
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

fn get(port: u16, request: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    read_all(s)
}

#[test]
fn request_split_across_writes() {
    let (port, dir) = start_server("split");
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(b"GET /sessions.json HTTP/1.1\r\n").unwrap();
    s.flush().unwrap();
    std::thread::sleep(Duration::from_millis(150));
    s.write_all(b"Host: t\r\nAccept: */*\r\n\r\n").unwrap();
    let r = read_all(s);
    assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
    assert!(r.contains("\"sessions\""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn large_headers_still_get_a_response() {
    let (port, dir) = start_server("bigheader");
    let req = format!(
        "GET /daily.json HTTP/1.1\r\nHost: t\r\nX-Pad: {}\r\n\r\n",
        "a".repeat(3000)
    );
    let r = get(port, &req);
    assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
    assert!(r.contains("\"days\""));

    // Beyond the 16 KB total cap: 431.
    let req = format!("GET / HTTP/1.1\r\nX-Pad: {}\r\n\r\n", "a".repeat(20_000));
    let r = get(port, &req);
    assert!(r.starts_with("HTTP/1.1 431 "), "{r}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn post_with_body_gets_405() {
    let (port, dir) = start_server("post");
    let body = "x".repeat(5000);
    let req = format!(
        "POST /sessions.json HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let r = get(port, &req);
    assert!(r.starts_with("HTTP/1.1 405 "), "{r}");
    assert!(r.contains("Allow: GET, HEAD"));
    let _ = std::fs::remove_dir_all(&dir);
}
