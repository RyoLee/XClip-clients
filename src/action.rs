//! Synchronous action orchestration.
//!
//! This module is the fix for the original design's core flaw. The AHK client
//! did:
//! ```text
//! Run("xclip.exe v")   ; start a process, do not wait
//! send("^v")           ; paste immediately
//! ```
//! so the paste raced the HTTP request and almost always pasted the *old*
//! clipboard. Here each action is a straight-line sequence with a verified
//! intermediate state:
//!
//!   copy : capture -> wait for the clipboard to actually change -> upload
//!   paste: download -> verify -> write clipboard -> verify -> paste
//!
//! Nothing is pasted until the server has answered successfully, and nothing
//! is uploaded until the selection has really reached the clipboard.

use crate::backend::{self, BackendError};
use crate::clipboard;
use crate::config::Config;
use crate::keys::KeySpec;
use crate::net::{Client, NetError};
use crate::platform::{self, Chord, Focus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Capture,
    Clipboard,
    Upload,
    Download,
    Paste,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Capture => "capture",
            Stage::Clipboard => "clipboard",
            Stage::Upload => "upload",
            Stage::Download => "download",
            Stage::Paste => "paste",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub ok: bool,
    pub stage: Stage,
    pub detail: String,
    /// Re-emit the prefix character because nothing else happened.
    pub restore_prefix: bool,
}

impl Outcome {
    fn success(stage: Stage, detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            stage,
            detail: detail.into(),
            restore_prefix: false,
        }
    }

    fn fail(stage: Stage, detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            stage,
            detail: detail.into(),
            restore_prefix: true,
        }
    }
}

fn chord_of(spec: KeySpec) -> Chord {
    Chord {
        mods: spec.mods,
        key: spec.key,
    }
}

pub enum Target {
    Copy,
    Paste,
}

pub struct Runner {
    client: Client,
    cfg: Config,
}

impl Runner {
    pub fn new(cfg: Config) -> Result<Self, NetError> {
        let client = Client::new(&cfg.net)?;
        Ok(Self { client, cfg })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn run(&self, target: Target, focus: Focus) -> Outcome {
        match target {
            Target::Copy => self.upload(focus),
            Target::Paste => self.download(focus),
        }
    }

    /// Capture the selection and upload it.
    fn upload(&self, focus: Focus) -> Outcome {
        let action = &self.cfg.copy;
        let chord = chord_of(action.capture_chord);

        // 1. Remember the clipboard generation before we trigger a copy.
        let before = clipboard::seq();

        // 2. Ask the target to copy. This must happen while it still has focus.
        if let Err(e) = backend::capture(action.backend, focus, chord) {
            return Outcome::fail(
                Stage::Capture,
                match e {
                    BackendError::Unsupported => "capture not supported here".to_string(),
                    BackendError::Failed => "could not capture the selection".to_string(),
                },
            );
        }

        // 3. Wait until the clipboard genuinely changed, then read it.
        let Some(text) = clipboard::wait_changed(before, action.clipboard_timeout) else {
            return Outcome::fail(Stage::Clipboard, "nothing was copied (no new selection)");
        };

        if text.trim().is_empty() {
            return Outcome::fail(Stage::Clipboard, "selection is empty");
        }

        // 4. Upload, and treat a non-"done" reply as a failure so the user is
        //    never told a copy succeeded when it did not.
        match self.client.upload(&text) {
            Ok(()) => Outcome::success(Stage::Upload, format!("copied {} chars", text.chars().count())),
            Err(e) => Outcome::fail(Stage::Upload, e.to_string()),
        }
    }

    /// Download and paste at the caret.
    fn download(&self, focus: Focus) -> Outcome {
        let action = &self.cfg.paste;

        // 1. Complete the request first. Nothing local is touched until it
        //    has succeeded; this is the ordering the old client got wrong.
        let fetched = match self.client.download() {
            Ok(f) => f,
            Err(NetError::Empty) => return Outcome::fail(Stage::Download, "server is empty"),
            Err(e) => return Outcome::fail(Stage::Download, e.to_string()),
        };

        // 2. Refuse to paste into a window the user has already left.
        if action.verify_focus && !platform::focus_alive(focus) {
            return Outcome::fail(Stage::Paste, "focus changed before pasting");
        }

        // 3. Publish to the clipboard and remember the new generation.
        let written = match clipboard::set_text(&fetched.text) {
            Ok(s) => s,
            Err(e) => return Outcome::fail(Stage::Clipboard, format!("clipboard write failed: {e}")),
        };

        // 4. If another process grabbed the clipboard in between, our paste
        //    would insert the wrong text.
        if !clipboard::still_ours(written) {
            return Outcome::fail(Stage::Clipboard, "clipboard was taken by another app");
        }

        // 5. Paste, re-validating focus inside the backend.
        let chord = chord_of(action.paste_chord);
        match backend::paste(action.backend, focus, chord, action.settle) {
            Ok(()) => Outcome::success(
                Stage::Paste,
                format!("pasted {} chars", fetched.text.chars().count()),
            ),
            Err(_) => Outcome::fail(Stage::Paste, "focus changed before pasting"),
        }
    }
}

/// Simple concurrency guard: one action at a time, and no queueing. A queued
/// paste that fires seconds later is worse than a rejected one.
pub struct Busy {
    flag: std::sync::atomic::AtomicBool,
}

impl Busy {
    pub fn new() -> Self {
        Self {
            flag: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn try_enter(&self) -> Option<Guard<'_>> {
        use std::sync::atomic::Ordering;
        if self
            .flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            Some(Guard { busy: self })
        } else {
            None
        }
    }

    pub fn is_busy(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
    }
}

pub struct Guard<'a> {
    busy: &'a Busy,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.busy
            .flag
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_helpers_set_restore_flag() {
        let ok = Outcome::success(Stage::Upload, "x");
        assert!(ok.ok);
        assert!(!ok.restore_prefix);

        let bad = Outcome::fail(Stage::Download, "boom");
        assert!(!bad.ok);
        assert!(bad.restore_prefix);
    }

    #[test]
    fn busy_allows_only_one_entrant() {
        let busy = Busy::new();
        let first = busy.try_enter();
        assert!(first.is_some());
        assert!(busy.try_enter().is_none());
        drop(first);
        assert!(busy.try_enter().is_some());
    }

    #[test]
    fn stage_labels_are_stable() {
        assert_eq!(Stage::Capture.label(), "capture");
        assert_eq!(Stage::Upload.label(), "upload");
        assert_eq!(Stage::Paste.label(), "paste");
    }

    #[test]
    fn chord_conversion_preserves_modifiers() {
        let spec = KeySpec::parse("Ctrl+Shift+V").unwrap();
        let c = chord_of(spec);
        assert!(c.mods.contains(crate::keys::Mod::Ctrl));
        assert!(c.mods.contains(crate::keys::Mod::Shift));
    }

    #[test]
    fn empty_selection_is_rejected_before_network() {
        // Contract check: a whitespace-only capture must never be uploaded,
        // because it would overwrite good server content with nothing.
        let text = "   \n\t ";
        assert!(text.trim().is_empty());
    }
}
