//! Low-level keyboard hook and the prefix-key state machine.
//!
//! Responsibilities are split so the hook callback stays trivial:
//!   * the hook callback decides swallow/pass in O(1) and posts to a channel
//!   * a worker thread performs every action (clipboard, HTTP, injection)
//!
//! Two invariants matter more than anything else here:
//!
//! 1. Never block inside the hook callback. Windows silently unhooks a
//!    low-level hook that exceeds LowLevelHooksTimeout (~300ms), which shows
//!    up as "the hotkey stopped working after a while".
//! 2. Ignore events we injected ourselves (dwExtraInfo marker, or
//!    LLKHF_INJECTED) or SendInput output would re-enter the hook and loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::HotkeyConfig;
use crate::keys::{Key, KeySpec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Copy,
    Paste,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Copy => "copy",
            Action::Paste => "paste",
        }
    }
}

/// Events produced by the hook and consumed by the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Fire(Action),
    /// The prefix was pressed and released alone; replay its character.
    ReplayPrefix { shift: bool },
    /// The user held the prefix past the combo window; emit now, then any
    /// auto-repeat continues as ordinary key repeat from the OS.
    ReplayOnTimeout { shift: bool },
}

/// The state machine. Pure logic: no Win32 calls, so it is unit-testable.
pub struct Machine {
    cfg: HotkeyConfig,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// Prefix is down, waiting to see whether a second key follows.
    Armed { at: Instant, shift: bool },
    /// The combo already fired; keep swallowing until the prefix is released.
    Consumed,
    /// Timeout already replayed the character; swallow the rest of this press.
    Emitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Let the OS deliver this key.
    Pass,
    /// Eat the key; the app must not see it.
    Swallow,
    /// Eat the key and emit these events.
    Emit(Option<Event>),
}

impl Machine {
    pub fn new(cfg: HotkeyConfig) -> Self {
        Self {
            cfg,
            state: State::Idle,
        }
    }

    pub fn state_name(&self) -> &'static str {
        match self.state {
            State::Idle => "idle",
            State::Armed { .. } => "armed",
            State::Consumed => "consumed",
            State::Emitted => "emitted",
        }
    }

    /// True when the prefix key matched.
    ///
    /// Shift is deliberately excluded from the comparison: Shift+prefix is
    /// what produces the "shifted" output character (backtick -> tilde), so a
    /// held Shift must not stop the prefix from being recognised.
    pub fn is_prefix_match(&self, key: Key, held: crate::keys::ModMask) -> bool {
        match self.cfg.prefix {
            Some(p) => {
                if p.key != key {
                    return false;
                }
                let required = p.mods.bits();
                let held_sans_shift = held.bits() & !crate::keys::Mod::Shift.bit();
                if self.cfg.require_exact_mods {
                    held_sans_shift == required
                } else {
                    held_sans_shift & required == required
                }
            }
            None => false,
        }
    }

    fn action_for(&self, key: Key, held: crate::keys::ModMask, copy: KeySpec, paste: KeySpec) -> Option<Action> {
        let exact = self.cfg.require_exact_mods;
        // In prefix mode the modifiers belong to the combination, not to the
        // action key, so the action matches on the key alone.
        let held_for_action = if self.cfg.prefix.is_some() {
            crate::keys::ModMask::empty()
        } else {
            held
        };
        if copy.matches(key, held_for_action, exact) {
            return Some(Action::Copy);
        }
        if paste.matches(key, held_for_action, exact) {
            return Some(Action::Paste);
        }
        None
    }

    /// Try several identifiers for one physical event. Whichever matches first
    /// wins; Pass only if none do.
    pub fn on_key_down_multi(
        &mut self,
        keys: &[Key],
        held: crate::keys::ModMask,
        now: Instant,
        copy: KeySpec,
        paste: KeySpec,
    ) -> Verdict {
        for &key in keys {
            let verdict = self.on_key_down(key, held, now, copy, paste);
            if verdict != Verdict::Pass {
                return verdict;
            }
        }
        Verdict::Pass
    }

    pub fn on_key_up_multi(
        &mut self,
        keys: &[Key],
        held: crate::keys::ModMask,
        now: Instant,
    ) -> Verdict {
        for &key in keys {
            let verdict = self.on_key_up(key, held, now);
            if verdict != Verdict::Pass {
                return verdict;
            }
        }
        Verdict::Pass
    }

    pub fn on_key_down(
        &mut self,
        key: Key,
        held: crate::keys::ModMask,
        now: Instant,
        copy: KeySpec,
        paste: KeySpec,
    ) -> Verdict {
        let shift = held.contains(crate::keys::Mod::Shift);

        // --- prefix mode -------------------------------------------------
        if self.cfg.prefix.is_some() {
            if self.is_prefix_match(key, held) {
                return match self.state {
                    State::Idle => {
                        self.state = State::Armed { at: now, shift };
                        // Swallow the physical key; we replay it on key-up.
                        Verdict::Swallow
                    }
                    // Auto-repeat of the prefix while armed: swallow.
                    State::Armed { .. } => Verdict::Swallow,
                    State::Consumed | State::Emitted => Verdict::Swallow,
                };
            }

            if let State::Armed { at, .. } = self.state {
                if now.duration_since(at) <= self.cfg.combo_timeout {
                    if let Some(action) = self.action_for(key, held, copy, paste) {
                        if self.cfg.allow_repeat {
                            // Fire but stay armed so a held key repeats.
                            return Verdict::Emit(Some(Event::Fire(action)));
                        }
                        self.state = State::Consumed;
                        return Verdict::Emit(Some(Event::Fire(action)));
                    }
                } else {
                    // Window expired: replay the prefix, then let this key
                    // through untouched.
                    self.state = State::Emitted;
                    return Verdict::Emit(Some(Event::ReplayOnTimeout { shift }));
                }
                // An unrelated key inside the window is passed through and the
                // prefix stays armed: the action key may still follow within
                // the timeout. Cancelling here made fast typing and macro
                // replay (which inserts modifier/space events) lose the combo.
                return Verdict::Pass;
            }

            if let State::Consumed | State::Emitted = self.state {
                // Swallow a repeat of the action key right after firing, so
                // holding the combo does not trigger twice.
                if self.action_for(key, held, copy, paste).is_some() {
                    return Verdict::Swallow;
                }
            }
            return Verdict::Pass;
        }

        // --- direct chord mode -------------------------------------------
        if let Some(action) = self.action_for(key, held, copy, paste) {
            if self.cfg.allow_repeat {
                return Verdict::Emit(Some(Event::Fire(action)));
            }
            match self.state {
                State::Consumed => return Verdict::Swallow,
                _ => {
                    self.state = State::Consumed;
                    return Verdict::Emit(Some(Event::Fire(action)));
                }
            }
        }
        Verdict::Pass
    }

    pub fn on_key_up(
        &mut self,
        key: Key,
        _held: crate::keys::ModMask,
        now: Instant,
    ) -> Verdict {
        if self.cfg.prefix.is_none() {
            if self.state == State::Consumed {
                self.state = State::Idle;
            }
            return Verdict::Pass;
        }

        let is_prefix_key = matches!(self.cfg.prefix, Some(p) if p.key == key);
        if !is_prefix_key {
            return Verdict::Pass;
        }

        match self.state {
            State::Armed { at, shift: down_shift } => {
                if now.duration_since(at) >= self.cfg.combo_timeout {
                    // Window already elapsed: replay now and finish.
                    self.state = State::Idle;
                    return Verdict::Emit(Some(Event::ReplayPrefix { shift: down_shift }));
                }
                // Stay armed past the key-up. Users routinely release the
                // prefix before pressing the action key, and the AHK original
                // accepted that; replaying here would insert the character and
                // make the combo unreachable. The timer thread replays once
                // the window closes.
                return Verdict::Swallow;
            }
            State::Consumed => {
                self.state = State::Idle;
                return Verdict::Swallow;
            }
            State::Emitted => {
                self.state = State::Idle;
                return Verdict::Swallow;
            }
            State::Idle => return Verdict::Pass,
        }
    }

    /// Called by the timer thread when the combo window expires.
    pub fn on_timeout(&mut self, now: Instant) -> Option<Event> {
        if let State::Armed { at, shift } = self.state {
            if now.duration_since(at) >= self.cfg.combo_timeout {
                self.state = State::Emitted;
                return Some(Event::ReplayOnTimeout { shift });
            }
        }
        None
    }

    pub fn combo_timeout(&self) -> Duration {
        self.cfg.combo_timeout
    }
}

/// Handle to the running hook. Dropping it must unhook from the hook thread,
/// because SetWindowsHookExW hooks are thread-affine.
pub struct HookRuntime {
    events: Option<std::sync::mpsc::Receiver<Event>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Thread id of the hook's message loop; WM_QUIT is posted directly to it.
    thread_id: u32,
}

impl HookRuntime {
    /// Move the event stream out. Ownership transfers to the caller, which
    /// guarantees exactly one consumer.
    pub fn take_events(&mut self) -> Option<std::sync::mpsc::Receiver<Event>> {
        self.events.take()
    }

    /// Unhook and join the hook thread.
    ///
    /// The thread sits in GetMessageW, so setting a flag is not enough: the
    /// loop must be woken with WM_QUIT posted to its thread. Joining without
    /// that would hang the caller (the tray menu callback) forever.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                self.thread_id,
                windows_sys::Win32::UI::WindowsAndMessaging::WM_QUIT,
                0,
                0,
            );
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for HookRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(windows)]
pub fn install(
    cfg: Arc<HotkeyConfig>,
    copy: KeySpec,
    paste: KeySpec,
) -> Result<HookRuntime, String> {
    use std::sync::Mutex;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    };

    let (tx, rx) = channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let machine = Arc::new(Mutex::new(Machine::new((*cfg).clone())));

    // Shared with the timer thread so it can decide when to fire.
    let shared: Arc<SharedHooks> = Arc::new(SharedHooks {
        tx: Mutex::new(Some(tx.clone())),
        machine: machine.clone(),
        copy,
        paste,
    });

    // The timer thread promotes "prefix held past the window" into a replay,
    // because the hook only runs when a key arrives.
    {
        let shared = shared.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(15));
                shared.tick(Instant::now());
            }
        });
    }

    // The hook is thread-affine, so it must be installed on the thread that
    // runs the message loop. Report the outcome back synchronously: otherwise
    // the caller would log success before SetWindowsHookExW even ran.
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, String>>();

    let thread = std::thread::Builder::new()
        .name("xclip-hook".into())
        .spawn(move || unsafe {
            // hMod must be our module handle; a NULL module while hooking a
            // thread other than our own silently installs a hook that is
            // never invoked.
            let hinstance =
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), hinstance, 0);
            if hook.is_null() {
                let _ = ready_tx.send(Err(crate::platform::last_error()));
                return;
            }
            // Report the thread id so stop() can post WM_QUIT to it.
            let tid = windows_sys::Win32::System::Threading::GetCurrentThreadId();
            // Publish the state, keeping the Arc alive in a static so the raw
            // pointer handed to the callback never dangles.
            SHARED.store(Arc::as_ptr(&shared) as *mut SharedHooks, Ordering::Release);
            let _ = SHARED_OWNER.set(shared);
            let _ = ready_tx.send(Ok(tid));
            crate::platform::run_message_loop();
            UnhookWindowsHookEx(hook);
        })
        .map_err(|e| e.to_string())?;

    let thread_id = match ready_rx.recv_timeout(Duration::from_millis(2000)) {
        Ok(Ok(tid)) => tid,
        Ok(Err(e)) => {
            stop.store(true, Ordering::Release);
            let _ = thread.join();
            return Err(format!("SetWindowsHookExW failed: {e}"));
        }
        Err(_) => {
            stop.store(true, Ordering::Release);
            return Err("hook thread did not report within 2s".to_string());
        }
    };

    Ok(HookRuntime {
        events: Some(rx),
        stop,
        thread: Some(thread),
        thread_id,
    })
}

#[cfg(windows)]
struct SharedHooks {
    tx: std::sync::Mutex<Option<Sender<Event>>>,
    machine: Arc<std::sync::Mutex<Machine>>,
    copy: KeySpec,
    paste: KeySpec,
}

#[cfg(windows)]
impl SharedHooks {
    fn tick(&self, now: Instant) {
        let mut m = self.machine.lock().unwrap();
        if let Some(ev) = m.on_timeout(now) {
            if let Some(tx) = self.tx.lock().unwrap().as_ref() {
                let _ = tx.send(ev);
            }
        }
    }

    fn send(&self, ev: Event) {
        if let Some(tx) = self.tx.lock().unwrap().as_ref() {
            let _ = tx.send(ev);
        }
    }
}

/// The hook callback has no user-data parameter, so the shared state is
/// published through a process-global slot. The owning Arc is parked in
/// SHARED_OWNER so the pointer stays valid for the hook's whole lifetime.
#[cfg(windows)]
static SHARED: std::sync::atomic::AtomicPtr<SharedHooks> =
    std::sync::atomic::AtomicPtr::new(std::ptr::null_mut());

#[cfg(windows)]
static SHARED_OWNER: std::sync::OnceLock<Arc<SharedHooks>> = std::sync::OnceLock::new();

#[cfg(windows)]
unsafe extern "system" fn hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, KBDLLHOOKSTRUCT, LLKHF_INJECTED,
    };

    if code < 0 {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }

    let shared_ptr = SHARED.load(Ordering::Acquire);
    if shared_ptr.is_null() {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }
    let shared = &*shared_ptr;

    let kb = &*(lparam as *const KBDLLHOOKSTRUCT);

    // Ignore our own injected input. Without this, SendInput output re-enters
    // the hook and can loop or double-handle the combo.
    //
    // XCLIP_TEST_INPUT=1 relaxes this for automated tests only: synthetic
    // keystrokes (keybd_event/SendKeys) also carry LLKHF_INJECTED, so they
    // would otherwise be invisible to the hook.
    let ignore_injected = !crate::platform::testing_input_enabled();
    if kb.dwExtraInfo == crate::platform::injected_marker()
        || (ignore_injected && kb.flags & LLKHF_INJECTED != 0)
    {
        return CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam);
    }

    const WM_KEYDOWN: usize = 0x0100;
    const WM_KEYUP: usize = 0x0101;
    const WM_SYSKEYDOWN: usize = 0x0104;
    const WM_SYSKEYUP: usize = 0x0105;

    let scancode = (kb.scanCode & 0xFF) as u16 | if kb.flags & 0x01 != 0 { 0x100 } else { 0 };
    let held = crate::platform::mods_held();

    // A KeySpec may be identified by scancode or by virtual key, and the two
    // spaces disagree (backtick is VK_OEM_3 but SC029). Test the event against
    // both identifiers instead of picking one global convention, otherwise a
    // Scan-style prefix would disable Vk-style action keys.
    let candidates = [Key::Scan(scancode), Key::Vk(kb.vkCode as u16)];

    let now = Instant::now();
    let mut m = shared.machine.lock().unwrap();

    let verdict = match wparam {
        WM_KEYDOWN | WM_SYSKEYDOWN => m.on_key_down_multi(&candidates, held, now, shared.copy, shared.paste),
        WM_KEYUP | WM_SYSKEYUP => m.on_key_up_multi(&candidates, held, now),
        _ => Verdict::Pass,
    };

    drop(m);

    match verdict {
        Verdict::Pass => CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam),
        Verdict::Swallow => 1,
        Verdict::Emit(ev) => {
            if let Some(ev) = ev {
                // Never inject from inside the hook callback: SendInput here
                // can stall the input queue. The pump thread performs it.
                shared.send(ev);
            }
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{Mod, ModMask};

    fn prefix_cfg() -> HotkeyConfig {
        HotkeyConfig {
            enabled: true,
            prefix: Some(KeySpec::parse("SC029").unwrap()),
            prefix_output: "\u{60}".into(),
            prefix_output_shift: Some("~".into()),
            combo_timeout: Duration::from_millis(350),
            restore_on_fail: true,
            require_exact_mods: true,
            allow_repeat: false,
        }
    }

    fn direct_cfg() -> HotkeyConfig {
        HotkeyConfig {
            prefix: None,
            ..prefix_cfg()
        }
    }

    fn copy() -> KeySpec {
        KeySpec::parse("C").unwrap()
    }
    fn paste() -> KeySpec {
        KeySpec::parse("V").unwrap()
    }

    const PREFIX: Key = Key::Scan(0x29);

    #[test]
    fn lone_prefix_press_replays_after_the_window() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        assert_eq!(m.on_key_down(PREFIX, ModMask::empty(), t, copy(), paste()), Verdict::Swallow);
        // Releasing early must NOT replay: the combo may still follow.
        let v = m.on_key_up(PREFIX, ModMask::empty(), t + Duration::from_millis(50));
        assert_eq!(v, Verdict::Swallow);
        assert_eq!(m.state_name(), "armed");
        // Once the window closes, the character appears exactly once.
        let ev = m.on_timeout(t + Duration::from_millis(400));
        assert!(matches!(ev, Some(Event::ReplayOnTimeout { shift: false })));
        assert_eq!(m.state_name(), "emitted");
    }

    #[test]
    fn shift_prefix_replays_tilde() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        let shift = ModMask::empty().with(Mod::Shift);
        assert_eq!(m.on_key_down(PREFIX, shift, t, copy(), paste()), Verdict::Swallow);
        assert_eq!(m.on_key_up(PREFIX, shift, t + Duration::from_millis(30)), Verdict::Swallow);
        let ev = m.on_timeout(t + Duration::from_millis(400));
        assert!(matches!(ev, Some(Event::ReplayOnTimeout { shift: true })));
    }

    #[test]
    fn release_then_press_action_key_still_fires() {
        // The common real-world rhythm: tap the prefix, then press C.
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        m.on_key_down(PREFIX, ModMask::empty(), t, copy(), paste());
        m.on_key_up(PREFIX, ModMask::empty(), t + Duration::from_millis(40));
        let v = m.on_key_down_multi(
            &[Key::Scan(0x2E), Key::Vk(0x43)],
            ModMask::empty(),
            t + Duration::from_millis(120),
            copy(),
            paste(),
        );
        assert!(matches!(v, Verdict::Emit(Some(Event::Fire(Action::Copy)))));
        // And the prefix character is never emitted afterwards.
        assert!(m.on_timeout(t + Duration::from_millis(500)).is_none());
    }

    #[test]
    fn combo_fires_and_swallows_the_prefix() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        m.on_key_down(PREFIX, ModMask::empty(), t, copy(), paste());
        let v = m.on_key_down(Key::Vk(0x43), ModMask::empty(), t + Duration::from_millis(80), copy(), paste());
        assert!(matches!(v, Verdict::Emit(Some(Event::Fire(Action::Copy)))));
        // Releasing the prefix afterwards must not replay the character.
        let v = m.on_key_up(PREFIX, ModMask::empty(), t + Duration::from_millis(120));
        assert_eq!(v, Verdict::Swallow);
        assert_eq!(m.state_name(), "idle");
    }

    #[test]
    fn paste_combo_has_distinct_action() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        m.on_key_down(PREFIX, ModMask::empty(), t, copy(), paste());
        let v = m.on_key_down(Key::Vk(0x56), ModMask::empty(), t + Duration::from_millis(60), copy(), paste());
        assert!(matches!(v, Verdict::Emit(Some(Event::Fire(Action::Paste)))));
    }

    #[test]
    fn combo_outside_window_replays_and_passes() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        m.on_key_down(PREFIX, ModMask::empty(), t, copy(), paste());
        let v = m.on_key_down(
            Key::Vk(0x43),
            ModMask::empty(),
            t + Duration::from_millis(500),
            copy(),
            paste(),
        );
        assert!(matches!(v, Verdict::Emit(Some(Event::ReplayOnTimeout { .. }))));
    }

    #[test]
    fn timeout_event_fires_once() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        m.on_key_down(PREFIX, ModMask::empty(), t, copy(), paste());
        assert!(m.on_timeout(t + Duration::from_millis(100)).is_none());
        let ev = m.on_timeout(t + Duration::from_millis(400));
        assert!(matches!(ev, Some(Event::ReplayOnTimeout { .. })));
        assert!(m.on_timeout(t + Duration::from_millis(500)).is_none());
    }

    #[test]
    fn unrelated_key_inside_window_passes_through() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        m.on_key_down(PREFIX, ModMask::empty(), t, copy(), paste());
        let v = m.on_key_down(Key::Vk(0x41), ModMask::empty(), t + Duration::from_millis(50), copy(), paste());
        assert_eq!(v, Verdict::Pass);
    }

    #[test]
    fn plain_keys_are_never_swallowed() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        assert_eq!(m.on_key_down(Key::Vk(0x41), ModMask::empty(), t, copy(), paste()), Verdict::Pass);
        assert_eq!(m.on_key_up(Key::Vk(0x41), ModMask::empty(), t), Verdict::Pass);
    }

    #[test]
    fn direct_mode_fires_on_bare_chord() {
        let mut m = Machine::new(direct_cfg());
        let t = Instant::now();
        let spec_copy = KeySpec::parse("Ctrl+Alt+C").unwrap();
        let spec_paste = KeySpec::parse("Ctrl+Alt+V").unwrap();
        let held = ModMask::empty().with(Mod::Ctrl).with(Mod::Alt);
        let v = m.on_key_down(Key::Vk(0x43), held, t, spec_copy, spec_paste);
        assert!(matches!(v, Verdict::Emit(Some(Event::Fire(Action::Copy)))));
        // Repeating while held is swallowed.
        let v = m.on_key_down(Key::Vk(0x43), held, t, spec_copy, spec_paste);
        assert_eq!(v, Verdict::Swallow);
    }

    #[test]
    fn direct_mode_requires_exact_modifiers() {
        let mut m = Machine::new(direct_cfg());
        let t = Instant::now();
        let spec_copy = KeySpec::parse("Ctrl+Alt+C").unwrap();
        let spec_paste = KeySpec::parse("Ctrl+Alt+V").unwrap();
        let held = ModMask::empty().with(Mod::Ctrl).with(Mod::Alt).with(Mod::Shift);
        let v = m.on_key_down(Key::Vk(0x43), held, t, spec_copy, spec_paste);
        assert_eq!(v, Verdict::Pass);
    }

    #[test]
    fn dual_identifier_matching_handles_scan_prefix_with_vk_action() {
        // The real hook sees one event with a scancode AND a virtual key.
        // A Scan prefix with Vk action keys must still work.
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();

        // backtick arrives as SC029 (scan) / VK_OEM_3 (0xC0)
        let v = m.on_key_down_multi(&[Key::Scan(0x29), Key::Vk(0xC0)], ModMask::empty(), t, copy(), paste());
        assert_eq!(v, Verdict::Swallow, "prefix should be swallowed");

        // V arrives as scan 0x2F / VK 0x56
        let v = m.on_key_down_multi(
            &[Key::Scan(0x2F), Key::Vk(0x56)],
            ModMask::empty(),
            t + Duration::from_millis(80),
            copy(),
            paste(),
        );
        assert!(
            matches!(v, Verdict::Emit(Some(Event::Fire(Action::Paste)))),
            "paste combo should fire, got {v:?}"
        );
    }

    #[test]
    fn dual_identifier_matching_rejects_unrelated_keys() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        m.on_key_down_multi(&[Key::Scan(0x29), Key::Vk(0xC0)], ModMask::empty(), t, copy(), paste());
        // 'A' is scan 0x1E / VK 0x41: neither the copy nor paste key.
        let v = m.on_key_down_multi(
            &[Key::Scan(0x1E), Key::Vk(0x41)],
            ModMask::empty(),
            t + Duration::from_millis(50),
            copy(),
            paste(),
        );
        assert_eq!(v, Verdict::Pass);
    }

    #[test]
    fn holding_action_key_after_combo_does_not_refire() {
        let mut m = Machine::new(prefix_cfg());
        let t = Instant::now();
        m.on_key_down(PREFIX, ModMask::empty(), t, copy(), paste());
        m.on_key_down(Key::Vk(0x43), ModMask::empty(), t + Duration::from_millis(50), copy(), paste());
        let v = m.on_key_down(Key::Vk(0x43), ModMask::empty(), t + Duration::from_millis(80), copy(), paste());
        assert_eq!(v, Verdict::Swallow);
    }
}
