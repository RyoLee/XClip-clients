//! XClip client: a cross-platform clipboard bridge.
//!
//! Two entry modes share one binary:
//!   xclip.exe              resident: hotkey hook, tray icon, context menu
//!   xclip.exe c | copy     one-shot: capture the selection and upload
//!   xclip.exe v | paste    one-shot: download and paste
//!
//! The one-shot mode exists for compatibility with the old AHK script and for
//! scripting; the resident mode replaces AHK entirely.

#![windows_subsystem = "windows"]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use xclip::action::{self, Runner};
use xclip::config::{self, Config, FeedbackKind};
use xclip::feedback;
use xclip::hotkey::{self, Action, Event, HookRuntime};
use xclip::platform;
use xclip::tray::{Command, TrayHost};

/// Append a line to xclip.log next to the executable.
fn log(msg: &str) {
    use std::io::Write;
    let mut path = config::config_path();
    path.set_file_name("xclip.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{msg}");
    }
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.to_ascii_lowercase().as_str() {
        "c" | "copy" => std::process::exit(run_once(Action::Copy)),
        "v" | "paste" => std::process::exit(run_once(Action::Paste)),
        // Installs and removes the hook repeatedly, which is exactly what the
        // tray toggle does. Used to verify that stopping never deadlocks.
        "selftest" => std::process::exit(run_selftest()),
        "traycheck" => std::process::exit(run_traycheck()),
        _ => {}
    }
    std::process::exit(run_resident());
}


/// Verify the tray callback path end to end: create the real window, post both
/// the legacy and the v4 encodings, and confirm each reaches the dispatcher.
fn run_traycheck() -> i32 {
    use std::sync::atomic::AtomicU32;
    static HITS: AtomicU32 = AtomicU32::new(0);

    let Some(host) = TrayHost::create("XClip", |cmd| {
        log(&format!("traycheck: command {cmd:?}"));
        HITS.fetch_add(1, Ordering::Release);
    }) else {
        log("traycheck: tray window creation FAILED");
        return 1;
    };
    log("traycheck: tray window created");

    // Let the message loop run on a thread so we can post and observe.
    let hwnd = host.as_hwnd();

    // Post the same WM_COMMAND the shell sends when a menu item is chosen.
    // This exercises wndproc -> dispatch -> sink without needing a real click.
    let ids = [
        (xclip::tray::IDM_TOGGLE, "Toggle"),
        (xclip::tray::IDM_TOGGLE, "Toggle-back"),
        (xclip::tray::IDM_RELOAD, "Reload"),
        (xclip::tray::IDM_EXIT, "Exit"),
    ];
    for (id, name) in ids {
        log(&format!("traycheck: sending {name}"));
        xclip::tray::post_command(hwnd, id);
        xclip::tray::pump_for(Duration::from_millis(300));
    }

    let hits = HITS.load(Ordering::Acquire);
    log(&format!("traycheck: commands dispatched = {hits}"));
    if hits == 4 {
        log("traycheck: passed");
        0
    } else {
        log("traycheck: FAILED");
        1
    }
}

/// Exercise the exact operations the tray menu performs: install, stop, and
/// reinstall. A stop that blocks in GetMessageW shows up as a long duration.
fn run_selftest() -> i32 {
    let Some(cfg) = load_config() else { return 2 };
    let mut ok = true;

    for round in 1..=3u32 {
        let start = std::time::Instant::now();
        let Some(mut h) = start_hook(&cfg) else {
            log(&format!("selftest round {round}: install failed"));
            return 1;
        };
        let installed = start.elapsed();

        let t2 = std::time::Instant::now();
        h.stop();
        let stopped = t2.elapsed();

        log(&format!(
            "selftest round {round}: install={installed:?} stop={stopped:?}"
        ));
        if stopped > Duration::from_millis(1500) {
            log(&format!("selftest round {round}: STOP TOO SLOW"));
            ok = false;
        }
    }

    // Mirror the paused -> resumed transition the menu drives.
    let mut slot: Option<HookRuntime> = start_hook(&cfg);
    let paused_ok = {
        if let Some(h) = slot.as_mut() {
            h.stop();
        }
        slot = None;
        slot.is_none()
    };
    let resumed_ok = {
        slot = start_hook(&cfg);
        slot.is_some()
    };
    if let Some(h) = slot.as_mut() {
        h.stop();
    }

    log(&format!(
        "selftest toggle: paused={paused_ok} resumed={resumed_ok}"
    ));
    if !paused_ok || !resumed_ok {
        ok = false;
    }

    log(if ok { "selftest: passed" } else { "selftest: FAILED" });
    if ok { 0 } else { 1 }
}

fn load_config() -> Option<Config> {
    match Config::load() {
        Ok(c) => {
            for w in &c.warnings {
                log(&format!("config warning: {w}"));
            }
            Some(c)
        }
        Err(e) => {
            log(&format!("config load failed: {e}"));
            None
        }
    }
}

fn run_once(action: Action) -> i32 {
    let Some(cfg) = load_config() else { return 2 };
    let runner = match Runner::new(cfg) {
        Ok(r) => r,
        Err(e) => {
            log(&format!("client init failed: {e}"));
            return 2;
        }
    };
    let Some(focus) = platform::current_focus() else {
        log("no focus window");
        return 3;
    };
    let target = match action {
        Action::Copy => action::Target::Copy,
        Action::Paste => action::Target::Paste,
    };
    let outcome = runner.run(target, focus);
    log(&format!(
        "{} [{}] {}",
        if outcome.ok { "ok" } else { "fail" },
        outcome.stage.label(),
        outcome.detail
    ));
    if outcome.ok { 0 } else { 1 }
}

fn run_resident() -> i32 {
    // The suffix exists so a wedged previous instance (or an integration test)
    // cannot block startup forever, and so parallel test runs are possible.
    let instance_name = match std::env::var("XCLIP_INSTANCE_SUFFIX") {
        Ok(s) if !s.is_empty() => format!("XClip.Client.SingleInstance.{s}"),
        _ => "XClip.Client.SingleInstance".to_string(),
    };
    let _guard = match platform::acquire_single_instance(&instance_name) {
        Some(g) => g,
        None => {
            log("another instance is already running");
            return 4;
        }
    };

    let Some(cfg) = load_config() else { return 2 };
    let cfg = Arc::new(Mutex::new(cfg));

    let runner = {
        let c = cfg.lock().unwrap().clone();
        match Runner::new(c) {
            Ok(r) => Arc::new(r),
            Err(e) => {
                log(&format!("client init failed: {e}"));
                return 2;
            }
        }
    };

    // Actions run here, never on the hook thread: the callback must stay O(1).
    let (tx, rx) = std::sync::mpsc::channel::<(Action, platform::Focus)>();
    let busy = Arc::new(Mutex::new(action::Busy::new()));
    let notify_sink: Arc<Mutex<Option<Arc<TrayHost>>>> = Arc::new(Mutex::new(None));
    {
        let runner = runner.clone();
        let busy = busy.clone();
        let notify_sink = notify_sink.clone();
        std::thread::Builder::new()
            .name("xclip-worker".into())
            .spawn(move || {
                for (act, focus) in rx {
                    let Ok(busy) = busy.lock() else { continue };
                    let Some(guard) = busy.try_enter() else {
                        log(&format!("{} ignored: previous action still running", act.name()));
                        continue;
                    };
                    let target = match act {
                        Action::Copy => action::Target::Copy,
                        Action::Paste => action::Target::Paste,
                    };
                    let outcome = runner.run(target, focus);
                    log(&format!(
                        "{} [{}] {}",
                        if outcome.ok { "ok" } else { "fail" },
                        outcome.stage.label(),
                        outcome.detail
                    ));

                    let kind = runner.config().feedback;
                    if kind != FeedbackKind::None {
                        let host = notify_sink.lock().ok().and_then(|g| g.clone());
                        let speaker = |t: &str, b: &str, tone: feedback::Tone| -> bool {
                            match host.as_ref() {
                                Some(h) => h.balloon(t, b, tone),
                                None => false,
                            }
                        };
                        feedback::report(
                            kind,
                            Some(&speaker),
                            "XClip",
                            &format!("{}: {}", outcome.stage.label(), outcome.detail),
                            feedback::tone_for(outcome.ok),
                        );
                    }
                    drop(guard);
                }
            })
            .ok();
    }

    let enabled = Arc::new(AtomicBool::new(cfg.lock().unwrap().hotkey.enabled));
    let mut hook: Option<HookRuntime> = None;
    if enabled.load(Ordering::Acquire) {
        hook = start_hook(&cfg.lock().unwrap());
    }
    forward_hook_events(&mut hook, tx.clone(), runner.clone());

    // The tray window must be created and pumped on this, the main thread.
    let enabled_for_menu = enabled.clone();
    let cfg_for_menu = cfg.clone();
    let hook_slot: Arc<Mutex<Option<HookRuntime>>> = Arc::new(Mutex::new(None));
    {
        let mut slot = hook_slot.lock().unwrap();
        *slot = hook.take();
    }

    let host = {
        let slot = hook_slot.clone();
        let enabled = enabled_for_menu.clone();
        let cfg = cfg_for_menu.clone();
        let tx = tx.clone();
        let runner = runner.clone();
        // The callback cannot reference the host it is being attached to, so it
        // reaches it through a slot filled in immediately afterwards.
        let pending: Arc<Mutex<Option<Arc<TrayHost>>>> = notify_sink.clone();
        let built: Option<TrayHost> = TrayHost::create("XClip", move |cmd| {
            match cmd {
                Command::Toggle => {
                    let now = !enabled.load(Ordering::Acquire);
                    enabled.store(now, Ordering::Release);
                    log(&format!("hotkey {}", if now { "enabled" } else { "paused" }));
                    let mut slot = match slot.lock() { Ok(s) => s, Err(_) => return };
                    if now {
                        if slot.is_none() {
                            let c = match cfg.lock() { Ok(c) => c.clone(), Err(_) => return };
                            *slot = start_hook(&c);
                            if let Some(h) = slot.as_mut() {
                                forward_hook_events_single(h, tx.clone(), runner.clone());
                            }
                        }
                    } else {
                        if let Some(h) = slot.as_mut() {
                            h.stop();
                        }
                        *slot = None;
                    }
                    if let Some(host) = pending.lock().ok().and_then(|g| g.clone()) {
                        host.refresh("XClip", now);
                    }
                }
                Command::Reload => {
                    log("reload requested");
                    match Config::load() {
                        Ok(fresh) => {
                            for w in &fresh.warnings {
                                log(&format!("config warning: {w}"));
                            }
                            let was_enabled = enabled.load(Ordering::Acquire);
                            if let Ok(mut c) = cfg.lock() {
                                *c = fresh.clone();
                            }
                            // The hook is thread-affine, so it must be torn
                            // down and rebuilt to pick up new key bindings.
                            if let Ok(mut slot) = slot.lock() {
                                if let Some(h) = slot.as_mut() {
                                    h.stop();
                                }
                                *slot = None;
                                if was_enabled {
                                    *slot = start_hook(&fresh);
                                    if let Some(h) = slot.as_mut() {
                                        forward_hook_events_single(h, tx.clone(), runner.clone());
                                    }
                                }
                            }
                            log("config reloaded");
                        }
                        Err(e) => log(&format!("reload failed: {e}")),
                    }
                }
                Command::Exit => {
                    log("exit requested");
                    if let Ok(mut slot) = slot.lock() {
                        if let Some(h) = slot.as_mut() {
                            h.stop();
                        }
                        *slot = None;
                    }
                    if let Some(host) = pending.lock().ok().and_then(|g| g.clone()) {
                        host.request_quit();
                    }
                }
                Command::AutostartUser => {
                    // Clicking an already-checked item clears it; clicking the
                    // other item switches. Either way the two stay exclusive.
                    let next = if xclip::autostart::current_mode() == xclip::autostart::Mode::User {
                        xclip::autostart::Mode::None
                    } else {
                        xclip::autostart::Mode::User
                    };
                    apply_autostart(next, &pending, &enabled);
                }
                Command::AutostartAdmin => {
                    // The menu item is greyed out when unelevated, so this only
                    // fires for an elevated process. Guard defensively anyway:
                    // an unelevated click is rejected with a clear message
                    // instead of silently failing or prompting UAC.
                    if !xclip::autostart::is_elevated() {
                        log("admin autostart rejected: not elevated");
                        if let Some(host) = pending.lock().ok().and_then(|g| g.clone()) {
                            host.balloon(
                                "需要管理员身份",
                                "请以管理员身份启动 XClip 后再开启此项。",
                                feedback::Tone::Warn,
                            );
                        }
                        return;
                    }
                    let mode = xclip::autostart::current_mode();
                    let next = if mode == xclip::autostart::Mode::Admin {
                        xclip::autostart::Mode::None
                    } else {
                        xclip::autostart::Mode::Admin
                    };
                    apply_autostart(next, &pending, &enabled);
                }
                Command::AutostartNone => {
                    apply_autostart(xclip::autostart::Mode::None, &pending, &enabled);
                }
            }
        });
        built
    };

    let Some(host) = host else {
        log("failed to create the tray icon");
        return 5;
    };
    let host = Arc::new(host);
    if let Ok(mut sink) = notify_sink.lock() {
        *sink = Some(host.clone());
    }
    host.refresh("XClip", enabled.load(Ordering::Acquire));

    log(&format!("resident started (pid {})", platform::current_pid()));
    start_config_watcher(cfg.clone());

    // No startup notification: the tray icon is the signal. Only action results
    // raise a balloon, and only when action.feedback says so.

    // Blocks until the user picks Exit.
    host.run();

    if let Ok(mut slot) = hook_slot.lock() {
        if let Some(h) = slot.as_mut() {
            h.stop();
        }
        *slot = None;
    }
    log("resident stopped");
    0
}

/// Apply an autostart mode change and log the outcome, then refresh the tray
/// so the check marks follow the new state immediately.
fn apply_autostart(
    mode: xclip::autostart::Mode,
    pending: &Arc<Mutex<Option<Arc<TrayHost>>>>,
    enabled: &Arc<AtomicBool>,
) {
    match xclip::autostart::set_mode(mode) {
        Ok(()) => log(&format!("autostart set to {}", mode.label())),
        Err(e) => log(&format!("autostart change failed: {e}")),
    }
    if let Some(host) = pending.lock().ok().and_then(|g| g.clone()) {
        host.refresh("XClip", enabled.load(Ordering::Acquire));
    }
}

fn start_hook(cfg: &Config) -> Option<HookRuntime> {
    match hotkey::install(Arc::new(cfg.hotkey.clone()), cfg.copy.key, cfg.paste.key) {
        Ok(runtime) => {
            log("hotkey hook installed");
            Some(runtime)
        }
        Err(e) => {
            log(&format!("hook install failed: {e}"));
            None
        }
    }
}

fn forward_hook_events(
    hook: &mut Option<HookRuntime>,
    tx: std::sync::mpsc::Sender<(Action, platform::Focus)>,
    runner: Arc<Runner>,
) {
    if let Some(h) = hook.as_mut() {
        forward_hook_events_single(h, tx, runner);
    }
}

/// Drain hook events on their own thread and translate them into actions.
fn forward_hook_events_single(
    hook: &mut HookRuntime,
    tx: std::sync::mpsc::Sender<(Action, platform::Focus)>,
    runner: Arc<Runner>,
) {
    let Some(rx_events) = hook.take_events() else { return };
    std::thread::Builder::new()
        .name("xclip-events".into())
        .spawn(move || {
            for ev in rx_events {
                match ev {
                    Event::Fire(act) => match platform::current_focus() {
                        Some(f) => {
                            let _ = tx.send((act, f));
                        }
                        None => log(&format!("{}: no focus window", act.name())),
                    },
                    // Replaying the prefix character must happen off the hook
                    // thread: injecting inside the callback can stall input.
                    Event::ReplayPrefix { shift } | Event::ReplayOnTimeout { shift } => {
                        let cfg = runner.config();
                        let text = if shift {
                            cfg.hotkey
                                .prefix_output_shift
                                .clone()
                                .unwrap_or_else(|| cfg.hotkey.prefix_output.clone())
                        } else {
                            cfg.hotkey.prefix_output.clone()
                        };
                        if !text.is_empty() {
                            platform::send_text(&text, platform::injected_marker());
                        }
                    }
                }
            }
        })
        .ok();
}

/// Log config edits. Applying them needs a hook rebuild, which the tray menu
/// performs on demand; the watcher just makes the change discoverable.
fn start_config_watcher(_cfg: Arc<Mutex<Config>>) {
    std::thread::Builder::new()
        .name("xclip-config".into())
        .spawn(move || {
            let mut last = config_mtime();
            loop {
                std::thread::sleep(Duration::from_secs(3));
                let now = config_mtime();
                if now != last {
                    last = now;
                    log("config file changed; use Reload config in the tray menu");
                }
            }
        })
        .ok();
}

fn config_mtime() -> Option<std::time::SystemTime> {
    std::fs::metadata(config::config_path())
        .ok()
        .and_then(|m| m.modified().ok())
}