//! Key specification parsing.
//!
//! A KeySpec is the parsed form of a configuration string such as
//! "SC029", "C", "Ctrl+Shift+V" or "F9". The same type is used for the
//! prefix key, the two action keys and the injected copy/paste chords, so
//! every key in the application flows through a single parser.

use std::fmt;

/// A physical key, identified either by scancode or virtual-key code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// Raw scancode. Layout independent, which is why the default prefix
    /// uses one: SC029 is the backtick key on every QWERTY variant.
    Scan(u16),
    /// Virtual-key code (VK_C, VK_F9, ...). Layout dependent for
    /// punctuation, which is why punctuation should use SCnnn instead.
    Vk(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mod {
    Ctrl,
    Alt,
    Shift,
    Win,
}

impl Mod {
    pub const ALL: [Mod; 4] = [Mod::Ctrl, Mod::Alt, Mod::Shift, Mod::Win];

    pub fn bit(self) -> u8 {
        match self {
            Mod::Ctrl => 0b0001,
            Mod::Alt => 0b0010,
            Mod::Shift => 0b0100,
            Mod::Win => 0b1000,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mod::Ctrl => "Ctrl",
            Mod::Alt => "Alt",
            Mod::Shift => "Shift",
            Mod::Win => "Win",
        }
    }
}

/// Modifier mask, kept as a bitset so matching is a single comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ModMask(u8);

impl ModMask {
    pub const fn empty() -> Self {
        ModMask(0)
    }

    pub fn with(mut self, m: Mod) -> Self {
        self.0 |= m.bit();
        self
    }

    pub fn insert(&mut self, m: Mod) {
        self.0 |= m.bit();
    }

    pub fn contains(self, m: Mod) -> bool {
        self.0 & m.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    /// Build a mask from Win32 GetAsyncKeyState-style booleans.
    pub fn from_parts(ctrl: bool, alt: bool, shift: bool, win: bool) -> Self {
        let mut m = ModMask::empty();
        if ctrl {
            m.insert(Mod::Ctrl);
        }
        if alt {
            m.insert(Mod::Alt);
        }
        if shift {
            m.insert(Mod::Shift);
        }
        if win {
            m.insert(Mod::Win);
        }
        m
    }
}

/// A key plus the modifiers that must be held with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeySpec {
    pub mods: ModMask,
    pub key: Key,
}

impl KeySpec {
    pub fn new(mods: ModMask, key: Key) -> Self {
        Self { mods, key }
    }

    /// Parse a configuration key string.
    ///
    /// Grammar: [Mod+]... Key, where Mod is Ctrl/Alt/Shift/Win (case
    /// insensitive, order independent) and Key is SCnnn (hex scancode), a
    /// single ASCII letter or digit, or a named key.
    pub fn parse(input: &str) -> Result<Self, String> {
        let s = input.trim();
        if s.is_empty() {
            return Err("empty key specification".to_string());
        }

        let (mods_part, key_part) = if let Some(stripped) = s.strip_suffix("++") {
            (stripped, "+")
        } else if let Some(idx) = s.rfind('+') {
            (&s[..idx], s[idx + 1..].trim())
        } else {
            ("", s)
        };

        let mut mods = ModMask::empty();
        if !mods_part.trim().is_empty() {
            for raw in mods_part.split('+') {
                let name = raw.trim();
                if name.is_empty() {
                    return Err(format!("empty modifier in {input:?}"));
                }
                match parse_mod(name) {
                    Some(m) => mods.insert(m),
                    None => return Err(format!("unknown modifier: {name:?}")),
                }
            }
        }

        if key_part.is_empty() {
            return Err(format!("missing key after modifiers in {input:?}"));
        }
        Ok(Self::new(mods, parse_key(key_part)?))
    }

    /// Whether the given physical key press satisfies this specification.
    ///
    /// exact_mods controls whether extra modifiers disqualify the match:
    /// when true, Ctrl+C does not fire on Ctrl+Shift+C.
    pub fn matches(&self, key: Key, held: ModMask, exact_mods: bool) -> bool {
        if self.key != key {
            return false;
        }
        if exact_mods {
            self.mods == held
        } else {
            self.mods.bits() & held.bits() == self.mods.bits()
        }
    }
}

fn parse_mod(s: &str) -> Option<Mod> {
    match s.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(Mod::Ctrl),
        "alt" => Some(Mod::Alt),
        "shift" => Some(Mod::Shift),
        "win" | "super" | "meta" | "cmd" => Some(Mod::Win),
        _ => None,
    }
}

fn parse_key(s: &str) -> Result<Key, String> {
    let upper = s.to_ascii_uppercase();

    if let Some(hex) = upper.strip_prefix("SC") {
        let v = u16::from_str_radix(hex, 16)
            .map_err(|_| format!("invalid scancode: {s:?} (expected hex, e.g. SC029)"))?;
        if v == 0 || v > 0x1FF {
            return Err(format!("scancode out of range: {s:?}"));
        }
        return Ok(Key::Scan(v));
    }

    if s.chars().count() == 1 {
        let c = s.chars().next().unwrap();
        if c.is_ascii_alphanumeric() {
            return Ok(Key::Vk(c.to_ascii_uppercase() as u16));
        }
        // '+' is the sole punctuation with an unambiguous spelling
        // ("Ctrl++"), so it gets a stable virtual-key mapping.
        if c == '+' {
            return Ok(Key::Vk(0xBB)); // VK_OEM_PLUS
        }
        if c.is_ascii() {
            return Err(format!(
                "punctuation key {s:?} is layout dependent; use its scancode (SCnnn)"
            ));
        }
        return Err(format!("non-ASCII key {s:?} must use a named key or SCnnn"));
    }

    let vk = match upper.as_str() {
        "SPACE" => 0x20,
        "ENTER" | "RETURN" => 0x0D,
        "TAB" => 0x09,
        "ESC" | "ESCAPE" => 0x1B,
        "BACKSPACE" | "BKSP" => 0x08,
        "DELETE" | "DEL" => 0x2E,
        "INSERT" | "INS" => 0x2D,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21,
        "PAGEDOWN" | "PGDN" => 0x22,
        "UP" => 0x26,
        "DOWN" => 0x28,
        "LEFT" => 0x25,
        "RIGHT" => 0x27,
        "CAPSLOCK" => 0x14,
        "NUMLOCK" => 0x90,
        "SCROLLLOCK" => 0x91,
        "MENU" | "APPS" => 0x5D,
        "PRINTSCREEN" | "PRTSC" => 0x2C,
        "PAUSE" => 0x13,
        other if other.starts_with('F') && other.len() <= 3 => {
            let n: u16 = other[1..]
                .parse()
                .map_err(|_| format!("invalid function key: {s:?}"))?;
            if !(1..=24).contains(&n) {
                return Err(format!("function key out of range (F1-F24): {s:?}"));
            }
            0x70 + (n - 1)
        }
        other => return Err(format!("unknown key name: {other:?}")),
    };
    Ok(Key::Vk(vk))
}

pub fn vk_name(vk: u16) -> String {
    let named = match vk {
        0x20 => "Space",
        0x0D => "Enter",
        0x09 => "Tab",
        0x1B => "Esc",
        0x08 => "Backspace",
        0x2E => "Delete",
        0x2D => "Insert",
        0x24 => "Home",
        0x23 => "End",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x26 => "Up",
        0x28 => "Down",
        0x25 => "Left",
        0x27 => "Right",
        0x14 => "CapsLock",
        0x90 => "NumLock",
        0x91 => "ScrollLock",
        0x5D => "Menu",
        0x2C => "PrintScreen",
        0x13 => "Pause",
        _ => "",
    };
    if !named.is_empty() {
        return named.to_string();
    }
    if (0x70..=0x87).contains(&vk) {
        return format!("F{}", vk - 0x70 + 1);
    }
    if (0x30..=0x39).contains(&vk) || (0x41..=0x5A).contains(&vk) {
        return ((vk as u8) as char).to_string();
    }
    format!("VK{vk:02X}")
}

impl fmt::Display for KeySpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for m in Mod::ALL {
            if self.mods.contains(m) {
                write!(f, "{}+", m.name())?;
            }
        }
        match self.key {
            Key::Scan(sc) if sc < 0x100 => write!(f, "SC{sc:03X}"),
            Key::Scan(sc) => write!(f, "SC{sc:04X}"),
            Key::Vk(vk) => write!(f, "{}", vk_name(vk)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> String {
        KeySpec::parse(s)
            .unwrap_or_else(|e| panic!("{s}: {e}"))
            .to_string()
    }

    #[test]
    fn parses_scancodes() {
        assert_eq!(KeySpec::parse("SC029").unwrap().key, Key::Scan(0x29));
        assert_eq!(KeySpec::parse("sc029").unwrap().key, Key::Scan(0x29));
        assert_eq!(p("sc029"), "SC029");
    }

    #[test]
    fn parses_letters_case_insensitively() {
        assert_eq!(KeySpec::parse("c").unwrap().key, Key::Vk(0x43));
        assert_eq!(KeySpec::parse("C").unwrap().key, Key::Vk(0x43));
        assert_eq!(p("v"), "V");
    }

    #[test]
    fn parses_modifiers_order_independently() {
        let a = KeySpec::parse("Ctrl+Shift+C").unwrap();
        let b = KeySpec::parse("shift+ctrl+c").unwrap();
        assert_eq!(a, b);
        assert!(a.mods.contains(Mod::Ctrl));
        assert!(a.mods.contains(Mod::Shift));
        assert!(!a.mods.contains(Mod::Alt));
    }

    #[test]
    fn parses_function_and_named_keys() {
        assert_eq!(KeySpec::parse("F9").unwrap().key, Key::Vk(0x78));
        assert_eq!(p("f12"), "F12");
        assert_eq!(KeySpec::parse("pageup").unwrap().key, Key::Vk(0x21));
        assert_eq!(p("Enter"), "Enter");
    }

    #[test]
    fn parses_plus_as_key() {
        let spec = KeySpec::parse("Ctrl++").unwrap();
        assert_eq!(spec.key, Key::Vk(0xBB)); // VK_OEM_PLUS
        assert!(spec.mods.contains(Mod::Ctrl));
    }

    #[test]
    fn rejects_invalid_input() {
        for bad in ["", "Ctrl+Foo", "Hyper+C", "F25", "SC", "SCzzz", "Ctrl+", "Ctrl+Alt+"] {
            assert!(KeySpec::parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn rejects_layout_dependent_punctuation() {
        // The raw backtick character depends on the active layout; SC029 is the way.
        let backtick = '\u{60}'.to_string();
        assert!(KeySpec::parse(&backtick).is_err());
        assert!(KeySpec::parse("~").is_err());
    }

    #[test]
    fn roundtrips_through_display() {
        for s in [
            "SC029", "C", "Ctrl+V", "Shift+F5", "Alt+Enter", "Win+Space",
            "Ctrl+Shift+PageUp", "SC150",
        ] {
            let once = p(s);
            assert_eq!(p(&once), once, "no roundtrip for {s}");
        }
    }

    #[test]
    fn matching_respects_exact_mods() {
        let spec = KeySpec::parse("Ctrl+C").unwrap();
        let ctrl = ModMask::empty().with(Mod::Ctrl);
        let ctrl_shift = ctrl.with(Mod::Shift);

        assert!(spec.matches(Key::Vk(0x43), ctrl, true));
        assert!(!spec.matches(Key::Vk(0x43), ctrl_shift, true));
        assert!(spec.matches(Key::Vk(0x43), ctrl_shift, false));
        assert!(!spec.matches(Key::Vk(0x56), ctrl, true));
    }

    #[test]
    fn plain_key_rejects_extra_mods_when_exact() {
        let spec = KeySpec::parse("C").unwrap();
        let shift = ModMask::empty().with(Mod::Shift);
        assert!(!spec.matches(Key::Vk(0x43), shift, true));
        assert!(spec.matches(Key::Vk(0x43), shift, false));
    }
}
