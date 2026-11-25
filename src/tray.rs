//! Tray icon, its context menu and the hidden message window it lives on.
//!
//! The icon is owned by a message-only window: Shell_NotifyIcon needs an HWND
//! to deliver callbacks to, and a message-only window keeps the process out of
//! the taskbar and the alt-tab list. Window creation and its message loop must
//! run on the same thread, so the resident mode pumps messages from here.

#![cfg(windows)]

use std::sync::Mutex;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, TrackPopupMenu, TranslateMessage, MF_CHECKED,
    MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND,
    WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW,
    WS_POPUP,
};

pub const WM_TRAY: u32 = WM_APP + 1;

pub const IDM_TOGGLE: usize = 1001;
pub const IDM_RELOAD: usize = 1002;
pub const IDM_EXIT: usize = 1003;
pub const IDM_AUTOSTART_USER: usize = 1010;
pub const IDM_AUTOSTART_ADMIN: usize = 1011;

const ICON_ID: u32 = 1;

fn app_icon() -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
    load_icon(1)
}

/// Desaturated variant shown while the hotkey is paused, so the tray state is
/// readable at a glance instead of relying on a tooltip.
fn paused_icon() -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
    load_icon(2)
}

fn load_icon(id: u16) -> windows_sys::Win32::UI::WindowsAndMessaging::HICON {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{LoadImageW, IMAGE_ICON, LR_DEFAULTSIZE};
    static CACHE: std::sync::Mutex<Vec<(u16, usize)>> = std::sync::Mutex::new(Vec::new());
    if let Ok(cache) = CACHE.lock() {
        if let Some((_, h)) = cache.iter().find(|(i, _)| *i == id) {
            return *h as windows_sys::Win32::UI::WindowsAndMessaging::HICON;
        }
    }
    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        // MAKEINTRESOURCE(id): ids assigned by build.rs from assets/*.ico.
        let h = LoadImageW(hinstance, id as *const u16, IMAGE_ICON, 0, 0, LR_DEFAULTSIZE);
        let v = h as usize;
        if let Ok(mut cache) = CACHE.lock() {
            cache.push((id, v));
        }
        h
    }
}



/// Testing hook: post the callback message to our own window, exactly as the
/// Post a WM_COMMAND for a menu id, exactly as the shell does when the user
/// picks an entry. Used by the traycheck self-test.
pub fn post_command(hwnd: HWND, id: usize) {
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(hwnd, WM_COMMAND, id, 0);
    }
}

/// Drain pending messages for up to `budget`, then return.
pub fn pump_for(budget: std::time::Duration) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    let deadline = std::time::Instant::now() + budget;
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while std::time::Instant::now() < deadline {
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// Timestamp of the last menu open, used to ignore duplicate notifications
/// (the shell emits both a button message and a context-menu message).
static LAST_MENU_OPEN: Mutex<Option<std::time::Instant>> = Mutex::new(None);

fn menu_open_recently() -> bool {
    LAST_MENU_OPEN
        .lock()
        .ok()
        .and_then(|g| *g)
        .map(|t| t.elapsed() < std::time::Duration::from_millis(400))
        .unwrap_or(false)
}

fn mark_menu_opened() {
    if let Ok(mut g) = LAST_MENU_OPEN.lock() {
        *g = Some(std::time::Instant::now());
    }
}

fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn fill(dst: &mut [u16], src: &str) {
    let mut i = 0;
    for unit in src.encode_utf16() {
        if i + 1 >= dst.len() {
            break;
        }
        dst[i] = unit;
        i += 1;
    }
    dst[i] = 0;
}

/// Things the tray window asks the application to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Toggle,
    Reload,
    Exit,
    AutostartUser,
    AutostartAdmin,
    AutostartNone,
}

static SINK: Mutex<Option<Box<dyn Fn(Command) + Send + 'static>>> = Mutex::new(None);
static ENABLED: Mutex<bool> = Mutex::new(true);

fn dispatch(cmd: Command) {
    if let Ok(sink) = SINK.lock() {
        if let Some(f) = sink.as_ref() {
            f(cmd);
        }
    }
}

fn set_enabled_flag(v: bool) {
    if let Ok(mut e) = ENABLED.lock() {
        *e = v;
    }
}

fn enabled_flag() -> bool {
    ENABLED.lock().map(|e| *e).unwrap_or(true)
}

/// Text of the hotkey entry.
///
/// Deliberately constant: a checkable menu item describes the capability, and
/// the check mark alone reports whether it is currently on. Changing the text
/// *and* the check mark yields two indicators that read as opposites, e.g. a
/// checked "pause hotkey" looks like "paused" while the hotkey is working.
pub fn toggle_label() -> &'static str {
    "启用热键"
}

/// Menu flags for the hotkey entry: checked while the hook is installed.
pub fn toggle_flags(enabled: bool) -> u32 {
    MF_STRING | if enabled { MF_CHECKED } else { 0 }
}

unsafe fn show_menu(hwnd: HWND) {
    let enabled = enabled_flag();
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    let label = wide(toggle_label());
    let flags = toggle_flags(enabled);
    AppendMenuW(menu, flags, IDM_TOGGLE, label.as_ptr());
    let reload = wide("重新载入配置");
    AppendMenuW(menu, MF_STRING, IDM_RELOAD, reload.as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());

    // Autostart entries: two mutually-exclusive radio-like items. There is no
    // native radio group in a popup menu, so the checked marks simulate it and
    // the command handler enforces the exclusion.
    let mode = crate::autostart::current_mode();
    let user_label = wide("开机自启");
    AppendMenuW(
        menu,
        MF_STRING | if mode == crate::autostart::Mode::User { MF_CHECKED } else { 0 },
        IDM_AUTOSTART_USER,
        user_label.as_ptr(),
    );
    // The admin item is disabled unless the process is elevated: registering
    // a HIGHEST scheduled task requires an elevated context, and an unelevated
    // process would either fail silently or prompt UAC, both of which confuse.
    // When disabled, the label carries the reason so the user is not left
    // guessing why it is greyed out.
    let elevated = crate::autostart::is_elevated();
    let admin_label = wide(if elevated {
        "以管理员身份开机自启"
    } else {
        "以管理员身份开机自启（需管理员身份运行）"
    });
    let admin_flags = if elevated {
        MF_STRING | if mode == crate::autostart::Mode::Admin { MF_CHECKED } else { 0 }
    } else {
        MF_STRING | MF_GRAYED
    };
    AppendMenuW(menu, admin_flags, IDM_AUTOSTART_ADMIN, admin_label.as_ptr());

    AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
    let quit = wide("退出");
    AppendMenuW(menu, MF_STRING, IDM_EXIT, quit.as_ptr());

    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    // Without the foreground window set first, the menu can linger after
    // the user clicks elsewhere.
    SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        std::ptr::null(),
    );
    let _ = PostMessageW(hwnd, 0, 0, 0);
    DestroyMenu(menu);

    match cmd as usize {
        0 => {}
        IDM_TOGGLE => dispatch(Command::Toggle),
        IDM_RELOAD => dispatch(Command::Reload),
        IDM_EXIT => dispatch(Command::Exit),
        IDM_AUTOSTART_USER => dispatch(Command::AutostartUser),
        IDM_AUTOSTART_ADMIN => dispatch(Command::AutostartAdmin),
        _ => {}
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            // Two encodings exist and they disagree about where the event lives:
            //
            //   v4 (set via NIM_SETVERSION):
            //     HIWORD(wParam) = message, LOWORD(wParam) = icon id,
            //     lParam         = packed mouse coordinates (x, y)
            //
            //   v3 / legacy:
            //     wParam = icon id, lParam = message
            //
            // Reading the message out of lParam under v4 yields a coordinate
            // pair, which never matches WM_RBUTTONUP, so clicks were ignored.
            // Observed on this shell (and on Windows 10/11 generally): the event
            // arrives in lParam, but HIWORD carries the icon id while LOWORD
            // carries the message, i.e. lParam == (id << 16) | WM_*. It is a
            // hybrid of the documented v3 and v4 layouts, so both halves are
            // checked instead of trusting a single documented encoding.
            let lo = lparam as usize & 0xFFFF;
            let hi = hiword(wparam as usize) as usize;
            let event = if lo != 0 { lo } else { hi };

            // A right-click arrives twice: once as WM_RBUTTONUP and once as
            // WM_CONTEXTMENU. Opening the menu for both means the second call
            // closes the first, so only the context-menu message triggers.
            // Left-clicks have no such companion message.
            let trigger = event == WM_CONTEXTMENU as usize
                || event == WM_LBUTTONUP as usize;

            if trigger && !menu_open_recently() {
                mark_menu_opened();
                show_menu(hwnd);
            }
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xffff;
            match id {
                IDM_TOGGLE => dispatch(Command::Toggle),
                IDM_RELOAD => dispatch(Command::Reload),
                IDM_EXIT => dispatch(Command::Exit),
                IDM_AUTOSTART_USER => dispatch(Command::AutostartUser),
                IDM_AUTOSTART_ADMIN => dispatch(Command::AutostartAdmin),
                _ => {}
            }
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// The tray window: owns the icon and pumps the application messages.
pub struct TrayHost {
    hwnd: HWND,
    /// Whether NIM_ADD already succeeded; Mutex so refresh works through Arc.
    added: Mutex<bool>,
}

// HWND is a user handle, not a pointer into this process: passing it across
// threads for PostMessage/Shell_NotifyIcon is safe. Only `run` and menu code
// must stay on the owning thread, which the type-level usage in main enforces
// by calling run() on the creating thread.
unsafe impl Send for TrayHost {}
unsafe impl Sync for TrayHost {}

impl TrayHost {
    pub fn create(tooltip: &str, on_command: impl Fn(Command) + Send + 'static) -> Option<Self> {
        if let Ok(mut s) = SINK.lock() {
            *s = Some(Box::new(on_command));
        }

        let class = wide("XClipTrayWindow");
        unsafe {
            let hinstance =
                windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: app_icon(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);

            // A top-level window (not HWND_MESSAGE): TrackPopupMenu needs a
            // real owner or the menu never appears. WS_POPUP with zero size
            // keeps it invisible; WS_EX_TOOLWINDOW keeps it out of the taskbar
            // and the Alt-Tab list.
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class.as_ptr(),
                class.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                return None;
            }

            let host = Self {
                hwnd,
                added: Mutex::new(false),
            };
            let added = host.notify(NIM_ADD, tooltip);
            if let Ok(mut a) = host.added.lock() {
                *a = added;
            }
            if added {
                // Opt into v4 message encoding (documented lparam layout).
                host.set_version(NOTIFYICON_VERSION_4);
            }
            Some(host)
        }
    }

    fn notify(&self, action: u32, tooltip: &str) -> bool {
        unsafe {
            let hicon = if enabled_flag() { app_icon() } else { paused_icon() };
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = ICON_ID;
            // NIF_SHOWTIP opts into the modern tooltip behaviour; without it
            // the tip can be ignored on Windows 10/11.
            nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
            nid.uCallbackMessage = WM_TRAY;
            nid.hIcon = hicon;
            fill(&mut nid.szTip, tooltip);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(action, &nid) != 0
        }
    }

    fn set_version(&self, version: u32) -> bool {
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = ICON_ID;
            nid.Anonymous.uVersion = version;
            Shell_NotifyIconW(NIM_SETVERSION, &nid) != 0
        }
    }

    pub fn as_hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn refresh(&self, tooltip: &str, enabled: bool) {
        set_enabled_flag(enabled);
        // Mirror the state into the tooltip; the greyscale paused icon carries
        // the same information for anyone who does not hover.
        let text = if enabled {
            format!("{tooltip} - 热键已启用")
        } else {
            format!("{tooltip} - 热键已暂停")
        };
        let already = self.added.lock().map(|a| *a).unwrap_or(false);
        if !already {
            let ok = self.notify(NIM_ADD, &text);
            if let Ok(mut a) = self.added.lock() {
                *a = ok;
            }
        } else {
            self.notify(NIM_MODIFY, &text);
        }
    }

    pub fn balloon(&self, title: &str, body: &str, tone: crate::feedback::Tone) -> bool {
        use windows_sys::Win32::UI::Shell::{NIF_INFO, NIIF_ERROR, NIIF_INFO, NIIF_WARNING};
        unsafe {
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = self.hwnd;
            nid.uID = ICON_ID;
            nid.uFlags = NIF_INFO;
            nid.dwInfoFlags = match tone {
                crate::feedback::Tone::Info => NIIF_INFO,
                crate::feedback::Tone::Warn => NIIF_WARNING,
                crate::feedback::Tone::Error => NIIF_ERROR,
            };
            fill(&mut nid.szInfoTitle, title);
            fill(&mut nid.szInfo, body);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            Shell_NotifyIconW(NIM_MODIFY, &nid) != 0
        }
    }

    /// Blocking message loop; returns when the window is destroyed.
    pub fn run(&self) {
        unsafe {
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    pub fn request_quit(&self) {
        unsafe {
            let _ = PostMessageW(self.hwnd, WM_DESTROY, 0, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_label_is_constant() {
        // The label must not describe the action, otherwise it contradicts the
        // check mark: "checked" always means enabled.
        assert_eq!(toggle_label(), "启用热键");
    }

    #[test]
    fn check_mark_tracks_enabled_state() {
        assert_ne!(toggle_flags(true) & MF_CHECKED, 0, "enabled must be checked");
        assert_eq!(toggle_flags(false) & MF_CHECKED, 0, "paused must be unchecked");
    }

    #[test]
    fn flags_are_never_separator_or_popup() {
        // MF_STRING == 0, so the useful assertion is the negative one: neither
        // state may render as a separator or a submenu.
        const MF_POPUP: u32 = 0x00000010;
        const MF_SEPARATOR: u32 = 0x00000800;
        for enabled in [true, false] {
            let f = toggle_flags(enabled);
            assert_eq!(f & MF_POPUP, 0, "must not be a submenu");
            assert_eq!(f & MF_SEPARATOR, 0, "must not be a separator");
        }
    }
}

impl Drop for TrayHost {
    fn drop(&mut self) {
        let added = self.added.lock().map(|a| *a).unwrap_or(false);
        if added {
            unsafe {
                let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
                nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = self.hwnd;
                nid.uID = ICON_ID;
                Shell_NotifyIconW(NIM_DELETE, &nid);
            }
        }
        unsafe {
            if !self.hwnd.is_null() {
                DestroyWindow(self.hwnd);
            }
        }
    }
}