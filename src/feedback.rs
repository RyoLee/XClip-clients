//! User feedback: tray balloon and audio cues.
//!
//! Because copy/paste is now synchronous and can take a few hundred
//! milliseconds, silence is ambiguous: the user cannot tell "still working"
//! from "did nothing". Every outcome therefore produces a visible signal.

use crate::config::FeedbackKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Message<'a> {
    pub tone: Tone,
    pub title: &'a str,
    pub body: &'a str,
}

pub fn tone_for(success: bool) -> Tone {
    if success {
        Tone::Info
    } else {
        Tone::Warn
    }
}

#[cfg(windows)]
mod user32 {
    extern "system" {
        pub fn MessageBeep(u_type: u32) -> i32;
    }
}

#[cfg(windows)]
pub fn beep(tone: Tone) {
    const MB_ICONASTERISK: u32 = 0x0000_0040;
    const MB_ICONEXCLAMATION: u32 = 0x0000_0030;
    const MB_ICONHAND: u32 = 0x0000_0010;
    let kind = match tone {
        Tone::Info => MB_ICONASTERISK,
        Tone::Warn => MB_ICONEXCLAMATION,
        Tone::Error => MB_ICONHAND,
    };
    unsafe {
        user32::MessageBeep(kind);
    }
}

/// Report an outcome through whichever channels the configuration enables.
///
/// `notify` is an optional speaker for tray balloons (the resident mode passes
/// the tray host); without one, toasts degrade to a beep rather than silence.
pub fn report(
    kind: FeedbackKind,
    notify: Option<&dyn Fn(&str, &str, Tone) -> bool>,
    title: &str,
    body: &str,
    tone: Tone,
) {
    match kind {
        FeedbackKind::None => {}
        FeedbackKind::Beep => {
            #[cfg(windows)]
            beep(tone);
        }
        FeedbackKind::Toast => {
            let shown = notify.map(|n| n(title, body, tone)).unwrap_or(false);
            if !shown {
                #[cfg(windows)]
                beep(tone);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_maps_to_info_tone() {
        assert_eq!(tone_for(true), Tone::Info);
        assert_eq!(tone_for(false), Tone::Warn);
    }
}
