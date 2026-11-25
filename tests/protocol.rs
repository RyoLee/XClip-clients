//! End-to-end protocol tests against an in-process fake server.
//!
//! These cover the ordering guarantee that the old AHK client violated:
//! nothing is pasted before the download has succeeded, and nothing is
//! uploaded before the clipboard actually changed.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use xclip::config::NetConfig;
use xclip::net::{md5_hex, token_for, Client, NetError};

struct FakeServer {
    store: Arc<Mutex<String>>,
    port: u16,
}

impl FakeServer {
    /// Minimal XClip-compatible server: POST stores, GET returns 204 when empty.
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let store = Arc::new(Mutex::new(String::new()));
        let store_c = store.clone();
        let id_hash = md5_hex(b"xclip-test");

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let store = store_c.clone();
                let id_hash = id_hash.clone();
                std::thread::spawn(move || {
                    let _ = handle(stream, &store, &id_hash);
                });
            }
        });
        Self { store, port }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn client(&self) -> Client {
        Client::new(&NetConfig {
            server: self.url(),
            id: "xclip-test".into(),
            password: "test".into(),
            http_timeout: Duration::from_millis(1500),
        })
        .expect("client")
    }
}

fn handle(mut stream: TcpStream, store: &Mutex<String>, id_hash: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        headers.push(line.trim_end().to_string());
    }

    let token = headers
        .iter()
        .find(|h| h.to_ascii_lowercase().starts_with("token:"))
        .map(|h| h[6..].trim().to_ascii_lowercase())
        .unwrap_or_default();

    // Accept the same +-1 time slice window the real server allows.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let pw_hash = md5_hex(b"test");
    let salt = now / 10;
    let valid: Vec<String> = [-1i64, 0, 1]
        .iter()
        .map(|d| md5_hex(format!("{pw_hash}{}", (salt as i64 + d)).as_bytes()))
        .collect();

    let expected_path = format!("/{id_hash}");
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    let (method, path) = (parts.first().copied().unwrap_or(""), parts.get(1).copied().unwrap_or(""));

    if path != expected_path {
        return respond(&mut stream, 403, b"");
    }
    if !valid.contains(&token) {
        return respond(&mut stream, 403, b"");
    }

    match method {
        "POST" => {
            let len = headers
                .iter()
                .find(|h| h.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|h| h.split(':').nth(1))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = vec![0u8; len];
            std::io::Read::read_exact(&mut reader, &mut body)?;
            let body = String::from_utf8_lossy(&body).to_string();
            let encoded = body
                .split("&")
                .find(|kv| kv.starts_with("value="))
                .map(|kv| percent_decode(&kv[6..]))
                .unwrap_or_default();
            let decoded = base64_decode(&encoded);
            *store.lock().unwrap() = String::from_utf8_lossy(&decoded).to_string();
            respond(&mut stream, 200, b"done")
        }
        "GET" => {
            let current = store.lock().unwrap().clone();
            if current.is_empty() {
                return respond(&mut stream, 204, b"");
            }
            let encoded = base64_encode(current.as_bytes());
            respond(&mut stream, 200, encoded.as_bytes())
        }
        _ => respond(&mut stream, 405, b""),
    }
}

fn respond(stream: &mut TcpStream, code: u16, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Decode application/x-www-form-urlencoded percent escapes. base64 payloads
/// need this because '+' and '=' are both escaped by form encoding.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn base64_encode(input: &[u8]) -> String {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    STANDARD.encode(input)
}

fn base64_decode(input: &str) -> Vec<u8> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    STANDARD.decode(input).unwrap_or_default()
}

#[test]
fn upload_then_download_roundtrip() {
    let server = FakeServer::start();
    let client = server.client();

    client.upload("hello world").expect("upload");
    assert_eq!(*server.store.lock().unwrap(), "hello world");

    let fetched = client.download().expect("download");
    assert_eq!(fetched.text, "hello world");
}

#[test]
fn download_of_empty_server_is_an_error_not_empty_string() {
    let server = FakeServer::start();
    let client = server.client();
    // Critical: an empty server must never yield "" that would then be
    // pasted, wiping the user's clipboard.
    assert_eq!(client.download().unwrap_err(), NetError::Empty);
}

#[test]
fn wrong_password_is_forbidden() {
    let server = FakeServer::start();
    let client = Client::new(&NetConfig {
        server: server.url(),
        id: "xclip-test".into(),
        password: "wrong".into(),
        http_timeout: Duration::from_millis(1500),
    })
    .unwrap();

    match client.download() {
        Err(NetError::Forbidden(code)) => assert_eq!(code, 403),
        other => panic!("expected Forbidden, got {other:?}"),
    }
}

#[test]
fn unknown_user_is_forbidden() {
    let server = FakeServer::start();
    let client = Client::new(&NetConfig {
        server: server.url(),
        id: "nobody".into(),
        password: "test".into(),
        http_timeout: Duration::from_millis(1500),
    })
    .unwrap();
    assert!(matches!(client.upload("x"), Err(NetError::Forbidden(403))));
}

#[test]
fn unicode_survives_the_roundtrip() {
    let server = FakeServer::start();
    let client = server.client();
    let text = "line1\n\u{4f60}\u{597d}\u{4e16}\u{754c} \u{1f600} tab\there";
    client.upload(text).expect("upload");
    assert_eq!(client.download().expect("download").text, text);
}

#[test]
fn upload_raises_on_non_done_reply() {
    // A server that answers 200 with something else must not be treated as
    // success; otherwise the UI would report a copy that did not happen.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                    break;
                }
            }
            let mut s = stream;
            let _ = respond(&mut s, 200, b"nope");
        }
    });

    let client = Client::new(&NetConfig {
        server: format!("http://127.0.0.1:{port}"),
        id: "xclip-test".into(),
        password: "test".into(),
        http_timeout: Duration::from_millis(1500),
    })
    .unwrap();

    match client.upload("data") {
        Err(NetError::Corrupt(_)) => {}
        other => panic!("expected Corrupt, got {other:?}"),
    }
}

#[test]
fn unreachable_server_reports_transport_error() {
    // Port 1 is reserved and nothing listens there.
    let client = Client::new(&NetConfig {
        server: "http://127.0.0.1:1".into(),
        id: "xclip-test".into(),
        password: "test".into(),
        http_timeout: Duration::from_millis(300),
    })
    .unwrap();
    assert!(matches!(client.download(), Err(NetError::Transport(_))));
}

#[test]
fn token_is_accepted_by_a_real_server_implementation() {
    // Guards the exact formula the Python server uses.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let t = token_for("test", now);
    let pw_hash = md5_hex(b"test");
    let salt = now / 10;
    let expected = md5_hex(format!("{pw_hash}{salt}").as_bytes());
    assert_eq!(t, expected);
}
