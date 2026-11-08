//! Real-socket test of the HTTP endpoint on 127.0.0.1.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use gametime::http;
use gametime::store::{SessionRecord, Store};

fn rec(game: &str, start: u64, end: u64, active: u64) -> SessionRecord {
    SessionRecord {
        game: game.into(),
        source: "steam".into(),
        exe: "x.exe".into(),
        start_utc: start,
        end_utc: end,
        active_s: active,
    }
}

fn get(port: u16, request: &str) -> String {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.write_all(request.as_bytes()).unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn serves_viewer_and_live_data() {
    let dir = std::env::temp_dir().join(format!("gametime-http-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    store
        .append(&rec("Closed Game", 1_767_323_045, 1_767_323_345, 300))
        .unwrap();

    let live = http::new_live();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let l2 = live.clone();
    let srv = Store::open(&dir).unwrap();
    std::thread::spawn(move || http::serve(listener, srv, l2));

    let r = get(port, "GET /sessions.json?x=1 HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200 OK\r\n"), "{r}");
    assert!(r.contains("Access-Control-Allow-Origin: *\r\n"));
    assert!(r.contains("Cache-Control: no-store\r\n"));
    assert!(r.contains("Closed Game"));
    assert!(!r.contains("\"open\""));

    // Live state is reflected at request time.
    *live.lock().unwrap() = Some(rec("Open Game", 1_767_330_000, 1_767_330_100, 100));
    let r = get(port, "GET /sessions.json HTTP/1.1\r\n\r\n");
    assert!(
        r.contains("Open Game") && r.contains("\"open\": true"),
        "{r}"
    );

    let r = get(port, "GET /daily.json HTTP/1.1\r\n\r\n");
    assert!(
        r.starts_with("HTTP/1.1 200 OK\r\n") && r.contains("\"days\""),
        "{r}"
    );

    let r = get(port, "GET /gametime.ics HTTP/1.1\r\n\r\n");
    assert!(r.contains("Content-Type: text/calendar") && r.contains("BEGIN:VCALENDAR"));

    let r = get(port, "GET / HTTP/1.1\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(r.contains("Content-Type: text/html; charset=utf-8"));
    assert!(r.contains("Access-Control-Allow-Origin: *\r\n"));
    assert!(r.to_lowercase().contains("<html"));

    let r = get(port, "GET /nope HTTP/1.1\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 404 "));
    assert!(r.contains("Access-Control-Allow-Origin: *\r\n"));
    let r = get(port, "POST /sessions.json HTTP/1.1\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 405 "));

    // A client that connects and goes silent must not wedge later requests
    // beyond the read timeout.
    let _idle = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let r = get(port, "GET / HTTP/1.1\r\n\r\n");
    assert!(r.starts_with("HTTP/1.1 200 OK\r\n"));

    let _ = std::fs::remove_dir_all(&dir);
}
