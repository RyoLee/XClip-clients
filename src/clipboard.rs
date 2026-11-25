//! Clipboard access with change detection.
//!
//! The old client read the clipboard immediately after spawning a child
//! process and injecting Ctrl+C, which only worked because process startup
//! happened to be slower than a copy. Here every transition is verified with
//! GetClipboardSequenceNumber, so a capture never reads stale content.

use std::thread;
use std::time::{Duration, Instant};

use crate::platform;

/// A clipboard generation counter. Every successful SetClipboardData bumps it.
pub fn seq() -> u32 {
    unsafe { win32::GetClipboardSequenceNumber() }
}

/// Poll until the clipboard sequence changes, or the deadline passes.
/// Returns the text that is now on the clipboard.
pub fn wait_changed(since: u32, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if seq() != since {
            // The sequence bumped; the owner may still be publishing formats,
            // so give it a couple of milliseconds before reading.
            thread::sleep(Duration::from_millis(2));
            if let Ok(Some(text)) = read_text() {
                return Some(text);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Read UNICODE text from the clipboard, retrying while another process
/// holds it open.
pub fn read_text() -> Result<Option<String>, String> {
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match try_read_text() {
            Ok(Some(s)) => return Ok(Some(s)),
            Ok(None) => return Ok(None),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn try_read_text() -> Result<Option<String>, String> {
    platform::clipboard_read_text()
}

/// Write text and return the clipboard sequence after the write, or None if
/// the write could not be performed.
pub fn set_text(text: &str) -> Result<u32, String> {
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        match platform::clipboard_write_text(text) {
            Ok(()) => return Ok(seq()),
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// True when the clipboard still holds what we put there, i.e. nobody
/// copied something else between our write and our paste.
pub fn still_ours(expected: u32) -> bool {
    seq() == expected
}

#[cfg(windows)]
mod win32 {
    #[link(name = "user32")]
    extern "system" {
        pub fn GetClipboardSequenceNumber() -> u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // The clipboard is process-global, so these tests must not overlap.
    static CLIP_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn sequence_number_bumps_on_write() {
        let _g = CLIP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let before = seq();
        if platform::clipboard_write_text("xclip-seq-probe").is_err() {
            return; // no interactive clipboard (headless CI)
        }
        let after = seq();
        assert_ne!(before, after, "clipboard write should bump the sequence");

        let changed = wait_changed(before, Duration::from_millis(500));
        assert_eq!(changed.as_deref(), Some("xclip-seq-probe"));
        assert!(still_ours(after));
    }

    #[test]
    fn wait_changed_times_out_without_changes() {
        let _g = CLIP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if platform::clipboard_write_text("xclip-baseline").is_err() {
            return;
        }
        let s = seq();
        // Nothing writes: expect a clean timeout rather than a bogus read.
        assert!(wait_changed(s, Duration::from_millis(60)).is_none());
    }

    #[test]
    fn roundtrips_unicode_text() {
        let _g = CLIP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let text = "xclip \u{4f60}\u{597d} \u{1f600}";
        if platform::clipboard_write_text(text).is_err() {
            return;
        }
        match read_text() {
            Ok(Some(got)) => assert_eq!(got, text),
            Ok(None) => panic!("clipboard reported empty after write"),
            Err(e) => panic!("read failed: {e}"),
        }
    }

    #[test]
    fn overwrite_replaces_previous_content() {
        let _g = CLIP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if platform::clipboard_write_text("first").is_err() {
            return;
        }
        let seq_after_first = set_text("second").expect("write");
        assert!(still_ours(seq_after_first));
        assert_eq!(read_text().unwrap().as_deref(), Some("second"));
    }
}
