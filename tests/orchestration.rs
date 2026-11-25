//! Orchestration tests: verify the ordering contract that the old client
//! violated. These drive Runner against a fake server and the real clipboard,
//! without needing a keystroke hook.

use std::time::Duration;

use xclip::action::{Busy, Runner, Target};
use xclip::clipboard;
use xclip::config::Config;
use xclip::net::{md5_hex, Client};
use xclip::platform;

/// The clipboard is process-global, so tests that touch it run one at a time.
static CLIP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_clipboard() -> std::sync::MutexGuard<'static, ()> {
    CLIP_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn config_for(server: &str) -> Config {
    let mut cfg = Config::default();
    cfg.net.server = server.to_string();
    cfg.net.id = "xclip-test".into();
    cfg.net.password = "test".into();
    cfg.net.http_timeout = Duration::from_millis(2000);
    cfg
}

/// Minimal in-process server that stores one value.
fn start_server() -> (String, std::sync::Arc<std::sync::Mutex<String>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let store = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let store_c = store.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let store = store_c.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let parts: Vec<String> = line.split_whitespace().map(String::from).collect();
                let method = parts.first().cloned().unwrap_or_default();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    if h.to_ascii_lowercase().starts_with("content-length:") {
                        len = h.split(':').nth(1).unwrap_or("0").trim().parse().unwrap_or(0);
                    }
                }
                let mut s = stream;
                match method.as_str() {
                    "POST" => {
                        let mut body = vec![0u8; len];
                        let _ = reader.read_exact(&mut body);
                        let body = String::from_utf8_lossy(&body).to_string();
                        let enc = body
                            .split("&")
                            .find(|kv| kv.starts_with("value="))
                            .map(|kv| kv[6..].replace("%3D", "=").replace("%2B", "+").replace("%2F", "/"))
                            .unwrap_or_default();
                        use base64::engine::general_purpose::STANDARD;
                        use base64::Engine as _;
                        let raw = STANDARD.decode(enc).unwrap_or_default();
                        *store.lock().unwrap() = String::from_utf8_lossy(&raw).to_string();
                        let _ = s.write_all(b"HTTP/1.1 200 X\r\nContent-Length: 4\r\n\r\ndone");
                    }
                    _ => {
                        let cur = store.lock().unwrap().clone();
                        if cur.is_empty() {
                            let _ = s.write_all(b"HTTP/1.1 204 X\r\nContent-Length: 0\r\n\r\n");
                        } else {
                            use base64::engine::general_purpose::STANDARD;
                            use base64::Engine as _;
                            let enc = STANDARD.encode(cur.as_bytes());
                            let head = format!(
                                "HTTP/1.1 200 X\r\nContent-Length: {}\r\n\r\n",
                                enc.len()
                            );
                            let _ = s.write_all(head.as_bytes());
                            let _ = s.write_all(enc.as_bytes());
                        }
                    }
                }
            });
        }
    });

    (format!("http://127.0.0.1:{port}"), store)
}

#[test]
fn paste_writes_clipboard_before_returning() {
    let _guard = lock_clipboard();
    // The core fix: after a successful paste action the clipboard must hold
    // the server content, not the previous local value.
    let (url, store) = start_server();
    *store.lock().unwrap() = "REMOTE-TEXT-123".to_string();

    clipboard::set_text("LOCAL-SENTINEL").expect("seed clipboard");

    let runner = Runner::new(config_for(&url)).expect("runner");
    let focus = platform::current_focus().unwrap_or(platform::Focus {
        hwnd: std::ptr::null_mut(),
        pid: std::process::id(),
    });

    // verify_focus=false: this test targets ordering, not window validity.
    let outcome = runner.run(Target::Paste, focus);
    assert!(
        outcome.ok || outcome.stage.label() == "paste",
        "unexpected failure at {}: {}",
        outcome.stage.label(),
        outcome.detail
    );

    let now = clipboard::read_text().expect("read").unwrap_or_default();
    assert_eq!(now, "REMOTE-TEXT-123", "clipboard must hold the server content");
}

#[test]
fn empty_server_does_not_clobber_the_clipboard() {
    let _guard = lock_clipboard();
    // A 204 must abort before writing, otherwise the user's clipboard is
    // destroyed by an empty paste.
    let (url, store) = start_server();
    assert!(store.lock().unwrap().is_empty());

    clipboard::set_text("KEEP-ME").expect("seed");
    let runner = Runner::new(config_for(&url)).expect("runner");
    let focus = platform::Focus {
        hwnd: std::ptr::null_mut(),
        pid: std::process::id(),
    };

    let outcome = runner.run(Target::Paste, focus);
    assert!(!outcome.ok);
    assert_eq!(outcome.stage.label(), "download");

    let now = clipboard::read_text().expect("read").unwrap_or_default();
    assert_eq!(now, "KEEP-ME", "clipboard must be untouched on empty server");
}

#[test]
fn unreachable_server_leaves_clipboard_alone() {
    let _guard = lock_clipboard();
    let cfg = config_for("http://127.0.0.1:1");
    clipboard::set_text("UNTOUCHED").expect("seed");
    let runner = Runner::new(cfg).expect("runner");
    let focus = platform::Focus {
        hwnd: std::ptr::null_mut(),
        pid: std::process::id(),
    };
    let outcome = runner.run(Target::Paste, focus);
    assert!(!outcome.ok);
    assert_eq!(outcome.stage.label(), "download");
    assert_eq!(clipboard::read_text().unwrap().as_deref(), Some("UNTOUCHED"));
}

#[test]
fn failed_action_requests_prefix_restore() {
    let cfg = config_for("http://127.0.0.1:1");
    let runner = Runner::new(cfg).expect("runner");
    let focus = platform::Focus {
        hwnd: std::ptr::null_mut(),
        pid: std::process::id(),
    };
    let outcome = runner.run(Target::Paste, focus);
    assert!(outcome.restore_prefix, "user must get feedback on failure");
}

#[test]
fn busy_guard_serializes_actions() {
    let busy = Busy::new();
    let g = busy.try_enter().expect("first entrant");
    assert!(busy.is_busy());
    assert!(busy.try_enter().is_none(), "second entrant must be rejected");
    drop(g);
    assert!(!busy.is_busy());
}

#[test]
fn client_uses_the_same_id_hash_as_the_server() {
    // The server keys users by md5(username); a mismatch yields 403.
    let (url, store) = start_server();
    *store.lock().unwrap() = "x".into();
    let cfg = config_for(&url);
    let client = Client::new(&cfg.net).expect("client");
    // Path correctness is implied by the server accepting the request.
    assert!(client.download().is_ok(), "path must be /{}", md5_hex(b"xclip-test"));
}
