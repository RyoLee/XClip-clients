//! Capture and paste backends.
//!
//! "Capture" means: make the target window put its selection in the
//! clipboard. "Paste" means: make the target window insert the clipboard at
//! its caret. Keystroke injection is the only mechanism that works across
//! essentially all applications, so it is the default; window messages are a
//! narrowly-scoped optimisation for classic edit controls, where they avoid
//! any dependence on focus timing.

use crate::config::BackendKind;
use crate::platform::{self, Chord, Focus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendError {
    /// The backend does not support this target; caller should fall back.
    Unsupported,
    /// The target refused or the API failed.
    Failed,
}

/// Decide which mechanism to use for the current target.
fn resolve(kind: BackendKind, focus: Focus, _paste: bool) -> BackendKind {
    match kind {
        BackendKind::Auto => {
            // Only classic edit controls reliably accept WM_COPY/WM_PASTE
            // across processes (these two messages are among the few the
            // system marshals for other windows).
            if platform::is_classic_edit(focus) {
                BackendKind::WindowMessage
            } else {
                BackendKind::Keystroke
            }
        }
        other => other,
    }
}

/// Make the focused control copy its selection to the clipboard.
pub fn capture(kind: BackendKind, focus: Focus, chord: Chord) -> Result<(), BackendError> {
    match resolve(kind, focus, false) {
        BackendKind::Keystroke | BackendKind::Auto => {
            platform::send_chord(chord, platform::injected_marker());
            Ok(())
        }
        BackendKind::WindowMessage => {
            if platform::post_window_command(focus.hwnd, false) {
                Ok(())
            } else {
                // Fall back rather than silently doing nothing.
                platform::send_chord(chord, platform::injected_marker());
                Ok(())
            }
        }
        // UIA has no reliable "insert at caret" or cross-process selection
        // capability worth the latency; fall back to keystrokes.
        BackendKind::Uia => {
            platform::send_chord(chord, platform::injected_marker());
            Ok(())
        }
    }
}

/// Make the focused control paste the clipboard at its caret.
///
/// Note: there is deliberately no UIA / ValuePattern path. Setting a value
/// replaces the whole content instead of inserting at the caret, and
/// EM_REPLACESEL-style messages are not marshalled across processes, so
/// keystrokes are the only faithful implementation of "paste here".
pub fn paste(kind: BackendKind, focus: Focus, chord: Chord, settle: std::time::Duration) -> Result<(), BackendError> {
    if !platform::focus_alive(focus) {
        return Err(BackendError::Failed);
    }

    let resolved = resolve(kind, focus, true);
    if resolved == BackendKind::WindowMessage && platform::post_window_command(focus.hwnd, true) {
        return Ok(());
    }

    if !settle.is_zero() {
        std::thread::sleep(settle);
    }
    // Re-check after the settle delay: focus can move while we wait.
    if !platform::focus_alive(focus) {
        return Err(BackendError::Failed);
    }
    platform::send_chord(chord, platform::injected_marker());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_without_classic_edit_uses_keystrokes() {
        let fake = Focus {
            hwnd: std::ptr::null_mut(),
            pid: 0,
        };
        // A null window is not a classic edit control.
        assert_eq!(resolve(BackendKind::Auto, fake, false), BackendKind::Keystroke);
    }

    #[test]
    fn explicit_backend_is_respected() {
        let fake = Focus {
            hwnd: std::ptr::null_mut(),
            pid: 0,
        };
        assert_eq!(resolve(BackendKind::Uia, fake, false), BackendKind::Uia);
        assert_eq!(
            resolve(BackendKind::Keystroke, fake, false),
            BackendKind::Keystroke
        );
    }
}
