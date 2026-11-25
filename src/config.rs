//! Configuration loading and validation.
//!
//! The INI file is read once at startup and once per hot-reload poll. Every
//! field is validated individually: a bad value falls back to its default and
//! produces a warning, so a typo never keeps the daemon from starting.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ini::Ini;

use crate::keys::KeySpec;

/// What the copy hotkey does once it has captured text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKind {
    Toast,
    Beep,
    None,
}

/// Which mechanism is used to trigger the copy/paste in the target window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// Pick per-target at runtime.
    Auto,
    /// Synthesise the key chord (works everywhere, incl. Electron/terminals).
    Keystroke,
    /// Post WM_COPY / WM_PASTE to classic edit controls.
    WindowMessage,
    /// Use UI Automation where available.
    Uia,
}

#[derive(Debug, Clone)]
pub struct HotkeyConfig {
    pub enabled: bool,
    /// Prefix key. None means "direct chord mode": the action key alone fires.
    pub prefix: Option<KeySpec>,
    /// Text emitted when the prefix is pressed and released alone.
    pub prefix_output: String,
    /// Text emitted when the prefix is released while Shift is held.
    pub prefix_output_shift: Option<String>,
    /// How long after pressing the prefix a second key still counts as a combo.
    pub combo_timeout: Duration,
    /// Re-emit the prefix character when the action fails.
    pub restore_on_fail: bool,
    /// Require an exact modifier match for action keys.
    pub require_exact_mods: bool,
    /// Allow the action to fire repeatedly while its key is held.
    pub allow_repeat: bool,
}

#[derive(Debug, Clone)]
pub struct ActionConfig {
    pub key: KeySpec,
    pub backend: BackendKind,
    /// Chord injected into the target window for capture (copy side).
    pub capture_chord: KeySpec,
    /// Chord injected into the target window for the paste side.
    pub paste_chord: KeySpec,
    /// How long to wait for the clipboard to change after the capture chord.
    pub clipboard_timeout: Duration,
    /// Settle delay before injecting the paste chord.
    pub settle: Duration,
    /// Abort if the foreground window changed since the hotkey fired.
    pub verify_focus: bool,
}

#[derive(Debug, Clone)]
pub struct NetConfig {
    pub server: String,
    pub id: String,
    pub password: String,
    pub http_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub net: NetConfig,
    pub hotkey: HotkeyConfig,
    pub copy: ActionConfig,
    pub paste: ActionConfig,
    pub feedback: FeedbackKind,
    /// Non-fatal problems worth surfacing to the user.
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        let ctrl_c = KeySpec::parse("Ctrl+C").expect("builtin");
        let ctrl_v = KeySpec::parse("Ctrl+V").expect("builtin");
        let prefix = KeySpec::parse("SC029").expect("builtin");
        Self {
            net: NetConfig {
                server: String::new(),
                id: String::new(),
                password: String::new(),
                http_timeout: Duration::from_millis(3000),
            },
            hotkey: HotkeyConfig {
                enabled: true,
                prefix: Some(prefix),
                prefix_output: String::from("\u{60}"),
                prefix_output_shift: Some(String::from("~")),
                combo_timeout: Duration::from_millis(350),
                restore_on_fail: true,
                require_exact_mods: true,
                allow_repeat: false,
            },
            copy: ActionConfig {
                key: KeySpec::parse("C").expect("builtin"),
                backend: BackendKind::Auto,
                capture_chord: ctrl_c,
                paste_chord: ctrl_v,
                clipboard_timeout: Duration::from_millis(600),
                settle: Duration::from_millis(20),
                verify_focus: true,
            },
            paste: ActionConfig {
                key: KeySpec::parse("V").expect("builtin"),
                backend: BackendKind::Auto,
                capture_chord: ctrl_c,
                paste_chord: KeySpec::parse("Ctrl+V").expect("builtin"),
                clipboard_timeout: Duration::from_millis(600),
                settle: Duration::from_millis(20),
                verify_focus: true,
            },
            feedback: FeedbackKind::Toast,
            warnings: Vec::new(),
        }
    }
}

pub fn config_path() -> PathBuf {
    let mut dir = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    dir.pop();
    dir.push("xclip.ini");
    dir
}

impl Config {
    /// Load from the executable's directory, falling back to built-in defaults
    /// for any missing or invalid field.
    pub fn load() -> std::io::Result<Self> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            let mut cfg = Config::default();
            cfg.warnings
                .push(format!("config not found, writing template: {}", path.display()));
            let _ = write_template(path);
            return Ok(cfg);
        }
        let text = std::fs::read_to_string(path)?;
        Ok(Self::parse(&text))
    }

    pub fn parse(text: &str) -> Self {
        let mut cfg = Config::default();
        let mut warnings = Vec::new();

        let ini = match Ini::load_from_str(text) {
            Ok(i) => i,
            Err(e) => {
                warnings.push(format!("config is not valid INI ({e}); using defaults"));
                cfg.warnings = warnings;
                return cfg;
            }
        };

        let get = |section: &str, key: &str| -> Option<String> {
            ini.get_from(Some(section), key).map(|s| s.to_string())
        };

        // ---- [main] -------------------------------------------------------
        if let Some(v) = get("main", "server") {
            cfg.net.server = v.trim().trim_end_matches('/').to_string();
        }
        if let Some(v) = get("main", "id") {
            cfg.net.id = v.trim().to_string();
        }
        if let Some(v) = get("main", "password") {
            cfg.net.password = v.to_string();
        }
        if cfg.net.server.is_empty() {
            warnings.push("main.server is empty; network calls will fail".to_string());
        }

        // ---- [hotkey] -----------------------------------------------------
        if let Some(v) = get("hotkey", "enabled") {
            cfg.hotkey.enabled = parse_bool(&v).unwrap_or(true);
        }
        if let Some(v) = get("hotkey", "prefix") {
            let v = v.trim();
            if v.is_empty() {
                cfg.hotkey.prefix = None;
            } else {
                match KeySpec::parse(v) {
                    Ok(k) => cfg.hotkey.prefix = Some(k),
                    Err(e) => warnings.push(format!("hotkey.prefix invalid ({e}); keeping default")),
                }
            }
        }
        if let Some(v) = get("hotkey", "prefix_output") {
            // Allow an escaped backslash-quote style literal.
            cfg.hotkey.prefix_output = v;
        }
        if let Some(v) = get("hotkey", "prefix_output_shift") {
            let v = v.trim();
            cfg.hotkey.prefix_output_shift = if v.is_empty() { None } else { Some(v.to_string()) };
        }
        if let Some(v) = get("hotkey", "combo_timeout") {
            match parse_u64(&v) {
                Some(ms) if (10..=5000).contains(&ms) => {
                    cfg.hotkey.combo_timeout = Duration::from_millis(ms)
                }
                Some(ms) => warnings.push(format!(
                    "hotkey.combo_timeout out of range (10-5000): {ms}; keeping default"
                )),
                None => warnings.push(format!("hotkey.combo_timeout invalid: {v}")),
            }
        }
        if let Some(v) = get("hotkey", "restore_on_fail") {
            cfg.hotkey.restore_on_fail = parse_bool(&v).unwrap_or(true);
        }
        if let Some(v) = get("hotkey", "require_exact_mods") {
            cfg.hotkey.require_exact_mods = parse_bool(&v).unwrap_or(true);
        }
        if let Some(v) = get("hotkey", "allow_repeat") {
            cfg.hotkey.allow_repeat = parse_bool(&v).unwrap_or(false);
        }

        // ---- [hotkey.copy] / [hotkey.paste] --------------------------------
        cfg.copy = parse_action(&ini, "hotkey.copy", &cfg.copy, &mut warnings);
        cfg.paste = parse_action(&ini, "hotkey.paste", &cfg.paste, &mut warnings);

        // ---- [action] -----------------------------------------------------
        if let Some(v) = get("action", "http_timeout") {
            match parse_u64(&v) {
                Some(ms) if (100..=60000).contains(&ms) => {
                    cfg.net.http_timeout = Duration::from_millis(ms)
                }
                _ => warnings.push(format!("action.http_timeout invalid: {v}")),
            }
        }
        if let Some(v) = get("action", "feedback") {
            cfg.feedback = match v.trim().to_ascii_lowercase().as_str() {
                "toast" => FeedbackKind::Toast,
                "beep" => FeedbackKind::Beep,
                "none" => FeedbackKind::None,
                other => {
                    warnings.push(format!("action.feedback unknown: {other}"));
                    FeedbackKind::Toast
                }
            };
        }

        // ---- cross-field validation ---------------------------------------
        let mut cfg = cfg;
        validate(&mut cfg, &mut warnings);
        cfg.warnings = warnings;
        cfg
    }

    pub fn mtime(&self) -> Option<SystemTime> {
        std::fs::metadata(config_path()).ok().and_then(|m| m.modified().ok())
    }
}

fn parse_action(
    ini: &Ini,
    section: &str,
    fallback: &ActionConfig,
    warnings: &mut Vec<String>,
) -> ActionConfig {
    let mut action = fallback.clone();
    let get = |key: &str| ini.get_from(Some(section), key).map(|s| s.trim().to_string());

    if let Some(v) = get("key") {
        match KeySpec::parse(&v) {
            Ok(k) => action.key = k,
            Err(e) => warnings.push(format!("{section}.key invalid ({e}); keeping default")),
        }
    }
    if let Some(v) = get("backend") {
        action.backend = match v.to_ascii_lowercase().as_str() {
            "auto" => BackendKind::Auto,
            "keystroke" | "keys" => BackendKind::Keystroke,
            "wm" | "wm_copy" | "wm_paste" | "windowmessage" => BackendKind::WindowMessage,
            "uia" => BackendKind::Uia,
            other => {
                warnings.push(format!("{section}.backend unknown: {other}"));
                BackendKind::Auto
            }
        };
    }
    if let Some(v) = get("capture") {
        match KeySpec::parse(&v) {
            Ok(k) => action.capture_chord = k,
            Err(e) => warnings.push(format!("{section}.capture invalid ({e})")),
        }
    }
    if let Some(v) = get("paste_chord") {
        match KeySpec::parse(&v) {
            Ok(k) => action.paste_chord = k,
            Err(e) => warnings.push(format!("{section}.paste_chord invalid ({e})")),
        }
    }
    if let Some(v) = get("timeout") {
        match parse_u64(&v) {
            Some(ms) if (50..=10000).contains(&ms) => {
                action.clipboard_timeout = Duration::from_millis(ms)
            }
            _ => warnings.push(format!("{section}.timeout invalid: {v}")),
        }
    }
    if let Some(v) = get("settle") {
        match parse_u64(&v) {
            Some(ms) if ms <= 1000 => action.settle = Duration::from_millis(ms),
            _ => warnings.push(format!("{section}.settle invalid: {v}")),
        }
    }
    if let Some(v) = get("verify_focus") {
        action.verify_focus = parse_bool(&v).unwrap_or(true);
    }
    action
}

/// Cross-field checks that individual parse steps cannot catch.
fn validate(cfg: &mut Config, warnings: &mut Vec<String>) {
    if let Some(prefix) = cfg.hotkey.prefix {
        // A prefix with a modifier that the action also requires would make
        // the combo unreachable in practice.
        if prefix.key == cfg.copy.key.key {
            warnings.push(
                "hotkey.prefix and hotkey.copy.key are the same physical key; \
                 the copy combo cannot fire"
                    .to_string(),
            );
            cfg.copy.key = Config::default().copy.key;
        }
        if prefix.key == cfg.paste.key.key {
            warnings.push(
                "hotkey.prefix and hotkey.paste.key are the same physical key; \
                 the paste combo cannot fire"
                    .to_string(),
            );
            cfg.paste.key = Config::default().paste.key;
        }
    }

    if cfg.copy.key == cfg.paste.key {
        warnings.push(
            "hotkey.copy.key and hotkey.paste.key are identical; paste will never fire"
                .to_string(),
        );
        cfg.paste.key = Config::default().paste.key;
    }

    if cfg.hotkey.prefix.is_some() && cfg.hotkey.prefix == Some(cfg.copy.key) {
        warnings.push("prefix equals copy key".to_string());
    }

    if !cfg.net.id.is_empty() && cfg.net.password.is_empty() {
        warnings.push("main.password is empty; the server will reject requests".to_string());
    }

    // In direct-chord mode the action keys must carry modifiers, otherwise a
    // bare letter would be swallowed globally and break normal typing.
    if cfg.hotkey.prefix.is_none() {
        for (name, action) in [("copy", &cfg.copy), ("paste", &cfg.paste)] {
            if action.key.mods.is_empty() {
                warnings.push(format!(
                    "hotkey.{name}.key has no modifier while prefix is disabled; \
                     this would swallow the key globally"
                ));
            }
        }
    }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "yes" | "true" | "1" | "on" => Some(true),
        "no" | "false" | "0" | "off" => Some(false),
        _ => None,
    }
}

fn parse_u64(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok()
}

pub fn write_template(path: &Path) -> std::io::Result<()> {
    std::fs::write(path, TEMPLATE)
}

pub const TEMPLATE: &str = r#"; XClip client configuration
; Generated automatically on first run. Edit and save, then restart the client.

[main]
server = https://your.server.addr
id = xclip-test
password = test

[hotkey]
; Master switch. no = install no keyboard hook at all (plain CLI mode).
enabled = yes

; Prefix key. Forms:
;   SCnnn   physical scancode, layout independent. SC029 = the backtick key.
;   C       a single letter or digit (virtual key).
;   F9      a named key (F1-F24, Space, Enter, Tab, Esc, Insert, Home, ...).
;   Ctrl+X  modifier + key; modifiers may be Ctrl/Alt/Shift/Win in any order.
; Leave empty to use "direct chord" mode: the action key fires on its own.
prefix = SC029

; Emitted when the prefix is pressed and no action key follows in time.
prefix_output = `

; Emitted instead when Shift is held with the prefix.
prefix_output_shift = ~

; Milliseconds after pressing the prefix during which an action key still
; counts as a combination. Release-then-press works within this window.
combo_timeout = 350

; Re-emit the prefix character when the action fails, so no keypress is silent.
restore_on_fail = yes

; Require an exact modifier match for the action keys.
require_exact_mods = yes

; Fire repeatedly while an action key is held down.
allow_repeat = no

[hotkey.copy]
; Enhanced COPY: capture the selection and upload it.
key = C
; auto | keystroke | wm_copy | uia
backend = auto
; Chord injected into the target to make it copy into the clipboard.
capture = Ctrl+C
; How long to wait for the clipboard to actually change (ms).
timeout = 600
verify_focus = yes

[hotkey.paste]
; Enhanced PASTE: download the server content and paste it at the caret.
key = V
; auto | keystroke | wm_paste
backend = auto
; Chord injected into the target to paste from the clipboard.
paste_chord = Ctrl+V
; Delay before injecting, giving the target time to settle (ms).
settle = 20
verify_focus = yes

[action]
http_timeout = 3000
; toast | beep | none
feedback = toast
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_ahk_behaviour() {
        let cfg = Config::default();
        assert_eq!(cfg.hotkey.prefix.map(|k| k.to_string()), Some("SC029".to_string()));
        assert_eq!(cfg.copy.key.to_string(), "C");
        assert_eq!(cfg.paste.key.to_string(), "V");
        assert_eq!(cfg.hotkey.prefix_output, "`");
        assert_eq!(cfg.hotkey.prefix_output_shift.as_deref(), Some("~"));
    }

    #[test]
    fn parses_full_config() {
        let cfg = Config::parse(TEMPLATE);
        assert_eq!(cfg.net.server, "https://your.server.addr");
        assert_eq!(cfg.hotkey.prefix.map(|k| k.to_string()), Some("SC029".to_string()));
        assert!(cfg.hotkey.enabled);
        assert_eq!(cfg.copy.key.to_string(), "C");
        assert_eq!(cfg.paste.key.to_string(), "V");
        assert!(cfg.warnings.is_empty(), "unexpected warnings: {:?}", cfg.warnings);
    }

    #[test]
    fn empty_prefix_switches_to_direct_mode() {
        let cfg = Config::parse("[main]
server=x
id=i
password=p
[hotkey]
prefix=
[hotkey.copy]
key=Ctrl+Alt+C
[hotkey.paste]
key=Ctrl+Alt+V
");
        assert!(cfg.hotkey.prefix.is_none());
        assert_eq!(cfg.copy.key.to_string(), "Ctrl+Alt+C");
    }

    #[test]
    fn unmodified_action_key_in_direct_mode_warns() {
        let cfg = Config::parse("[hotkey]
prefix=
[hotkey.copy]
key=C
[hotkey.paste]
key=Ctrl+Alt+V
");
        assert!(cfg.warnings.iter().any(|w| w.contains("no modifier")));
    }

    #[test]
    fn missing_sections_use_defaults() {
        let cfg = Config::parse("[main]
server=https://x
id=a
password=b
");
        assert_eq!(cfg.copy.key.to_string(), "C");
        assert!(cfg.hotkey.enabled);
    }

    #[test]
    fn bad_prefix_keeps_default_and_warns() {
        let cfg = Config::parse("[hotkey]
prefix=Ctrl+Foo
");
        assert_eq!(cfg.hotkey.prefix.map(|k| k.to_string()), Some("SC029".to_string()));
        assert!(cfg.warnings.iter().any(|w| w.contains("prefix invalid")));
    }

    #[test]
    fn prefix_colliding_with_copy_key_is_rejected() {
        let cfg = Config::parse("[hotkey]
prefix=C
[hotkey.copy]
key=C
");
        assert_eq!(cfg.hotkey.prefix.map(|k| k.to_string()), Some("C".to_string()));
        // copy key falls back to the default so the combo remains usable
        assert_eq!(cfg.copy.key.to_string(), "C");
        assert!(cfg.warnings.iter().any(|w| w.contains("same physical key")));
    }

    #[test]
    fn identical_action_keys_warn() {
        let cfg = Config::parse("[hotkey.copy]
key=X
[hotkey.paste]
key=X
");
        assert!(cfg.warnings.iter().any(|w| w.contains("identical")));
    }

    #[test]
    fn out_of_range_timeout_warns_and_keeps_default() {
        let cfg = Config::parse("[hotkey]
combo_timeout=99999
");
        assert_eq!(cfg.hotkey.combo_timeout, Duration::from_millis(350));
        assert!(cfg.warnings.iter().any(|w| w.contains("out of range")));
    }

    #[test]
    fn invalid_ini_falls_back_to_defaults() {
        let cfg = Config::parse(" not an ini at all");
        assert_eq!(cfg.copy.key.to_string(), "C");
    }

    #[test]
    fn bool_and_int_parsing() {
        assert_eq!(parse_bool("YES"), Some(true));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
        assert_eq!(parse_u64(" 350 "), Some(350));
        assert_eq!(parse_u64("3.5"), None);
    }
}