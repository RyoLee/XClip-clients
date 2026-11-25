//! Win32 primitives shared by the clipboard, backend and hotkey layers.
//!
//! Everything in this module is thin and fallible: no panics, no unwraps on
//! API results, because these calls run on the hot path of a keystroke.

#![cfg(windows)]

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable,
    OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow, GetGUIThreadInfo,
    GetMessageW, GetWindowThreadProcessId, IsWindow, PostMessageW, RegisterClassW, SendMessageW,
    TranslateMessage, GUITHREADINFO, HWND_MESSAGE, MSG, WM_COPY, WM_PASTE, WNDCLASSW,
};

pub const CF_UNICODETEXT: u32 = 13;

pub fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

fn open_clipboard(hwnd: HWND) -> Result<(), String> {
    // OpenClipboard can fail with ERROR_ACCESS_DENIED when another process
    // holds it; the caller retries.
    unsafe {
        if OpenClipboard(hwnd) != 0 {
            Ok(())
        } else {
            Err(last_error())
        }
    }
}

/// Read CF_UNICODETEXT. Returns Ok(None) when the clipboard has no text.
pub fn clipboard_read_text() -> Result<Option<String>, String> {
    let hwnd: HWND = std::ptr::null_mut();
    // Retry briefly: the clipboard is a global mutex, transiently held by
    // Explorer, IME helpers and clipboard managers.
    let mut last = String::new();
    for attempt in 0..5 {
        open_clipboard(hwnd)?;
        unsafe {
            if IsClipboardFormatAvailable(CF_UNICODETEXT) == 0 {
                let _ = CloseClipboard();
                return Ok(None);
            }
            let handle = GetClipboardData(CF_UNICODETEXT);
            if handle.is_null() {
                last = last_error();
                let _ = CloseClipboard();
                std::thread::sleep(std::time::Duration::from_millis(5 * (attempt + 1)));
                continue;
            }
            let ptr = GlobalLock(handle as *mut c_void) as *const u16;
            if ptr.is_null() {
                last = last_error();
                let _ = CloseClipboard();
                continue;
            }
            let mut len = 0usize;
            while *ptr.add(len) != 0 {
                len += 1;
            }
            let slice = std::slice::from_raw_parts(ptr, len);
            let out = String::from_utf16_lossy(slice);
            let _ = GlobalUnlock(handle as *mut c_void);
            let _ = CloseClipboard();
            return Ok(Some(out));
        }
    }
    Err(if last.is_empty() { "clipboard read failed".into() } else { last })
}

/// Write CF_UNICODETEXT, replacing the previous contents.
pub fn clipboard_write_text(text: &str) -> Result<(), String> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();

    unsafe {
        let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if hmem.is_null() {
            return Err(last_error());
        }
        let dst = GlobalLock(hmem) as *mut u16;
        if dst.is_null() {
            let _ = GlobalFree(hmem);
            return Err(last_error());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(hmem);

        open_clipboard(std::ptr::null_mut())?;
        if EmptyClipboard() == 0 {
            let e = last_error();
            let _ = CloseClipboard();
            let _ = GlobalFree(hmem);
            return Err(e);
        }
        // After EmptyClipboard the system owns the handle; do not free it.
        if SetClipboardData(CF_UNICODETEXT, hmem as HANDLE).is_null() {
            let e = last_error();
            let _ = CloseClipboard();
            let _ = GlobalFree(hmem);
            return Err(e);
        }
        let _ = CloseClipboard();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Modifier state
// ---------------------------------------------------------------------------

pub fn mods_held() -> crate::keys::ModMask {
    unsafe {
        let down = |vk: u16| (GetAsyncKeyState(vk as i32) as u16) & 0x8000 != 0;
        crate::keys::ModMask::from_parts(
            down(VK_CONTROL),
            down(VK_MENU),
            down(VK_SHIFT),
            down(VK_LWIN) || down(VK_RWIN),
        )
    }
}

// ---------------------------------------------------------------------------
// Key injection
// ---------------------------------------------------------------------------

fn vk_from_scancode(sc: u16) -> u16 {
    unsafe {
        let scan = ((sc & 0xFF) as u32) | ((((sc >> 8) & 0xFF) as u32) << 16);
        windows_sys::Win32::UI::Input::KeyboardAndMouse::MapVirtualKeyW(
            scan,
            windows_sys::Win32::UI::Input::KeyboardAndMouse::MAPVK_VSC_TO_VK_EX,
        ) as u16
    }
}

fn key_input(vk: u16, scancode: Option<u16>, up: bool) -> INPUT {
    let mut flags = 0u32;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    let mut ki = KEYBDINPUT {
        wVk: 0,
        wScan: 0,
        dwFlags: flags,
        time: 0,
        dwExtraInfo: 0,
    };
    match scancode {
        Some(sc) => {
            ki.wScan = sc;
            ki.dwFlags |= KEYEVENTF_SCANCODE;
            // Extended keys (arrows, right-hand modifiers) set bit 0x100.
            if sc & 0x100 != 0 {
                ki.dwFlags |= windows_sys::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_EXTENDEDKEY;
            }
        }
        None => {
            ki.wVk = vk;
        }
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki },
    }
}

fn send(inputs: &[INPUT]) -> usize {
    if inputs.is_empty() {
        return 0;
    }
    unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) as usize }
}

/// A key chord to inject: modifiers are pressed around a single key.
#[derive(Debug, Clone, Copy)]
pub struct Chord {
    pub mods: crate::keys::ModMask,
    pub key: crate::keys::Key,
}

fn modifier_vk(m: crate::keys::Mod) -> u16 {
    match m {
        crate::keys::Mod::Ctrl => VK_CONTROL,
        crate::keys::Mod::Alt => VK_MENU,
        crate::keys::Mod::Shift => VK_SHIFT,
        crate::keys::Mod::Win => VK_LWIN,
    }
}

/// Press and release a chord. Every injected event carries dwExtraInfo, which
/// the low-level hook uses to recognise (and ignore) its own output.
pub fn send_chord(chord: Chord, marker: usize) {
    let mut inputs: Vec<INPUT> = Vec::with_capacity(8);

    let mods: Vec<u16> = crate::keys::Mod::ALL
        .iter()
        .filter(|m| chord.mods.contains(**m))
        .map(|m| modifier_vk(*m))
        .collect();

    for vk in &mods {
        let mut inp = key_input(*vk, None, false);
        inp.Anonymous.ki.dwExtraInfo = marker;
        inputs.push(inp);
    }

    let (vk, scan) = match chord.key {
        crate::keys::Key::Vk(vk) => (vk, None),
        crate::keys::Key::Scan(sc) => (vk_from_scancode(sc), Some(sc)),
    };
    let mut down = key_input(vk, scan, false);
    down.Anonymous.ki.dwExtraInfo = marker;
    inputs.push(down);

    let mut up = key_input(vk, scan, true);
    up.Anonymous.ki.dwExtraInfo = marker;
    inputs.push(up);

    for vk in mods.iter().rev() {
        let mut inp = key_input(*vk, None, true);
        inp.Anonymous.ki.dwExtraInfo = marker;
        inputs.push(inp);
    }

    send(&inputs);
}

/// Type a literal string (used to replay the prefix character).
pub fn send_text(text: &str, marker: usize) {
    for unit in text.encode_utf16() {
        let mut down = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: marker,
                },
            },
        };
        let mut up = down;
        up.Anonymous.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
        down.Anonymous.ki.dwExtraInfo = marker;
        up.Anonymous.ki.dwExtraInfo = marker;
        send(&[down, up]);
    }
}

// ---------------------------------------------------------------------------
// Focus tracking
// ---------------------------------------------------------------------------

/// The window that will receive injected keys.
///
/// HWND is a raw pointer, which is not Send by default. The handle is only
/// ever used on the worker thread for the duration of one action, and validity
/// is re-checked with IsWindow/focus_alive before every use, so asserting
/// Send is sound here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Focus {
    pub hwnd: HWND,
    pub pid: u32,
}

unsafe impl Send for Focus {}

/// Resolve the control that currently has keyboard focus, not merely the
/// foreground frame. This is what must be re-validated before pasting.
pub fn current_focus() -> Option<Focus> {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() {
            return None;
        }
        let mut pid = 0u32;
        let tid = GetWindowThreadProcessId(fg, &mut pid);
        if tid == 0 {
            return None;
        }
        let mut gti: GUITHREADINFO = std::mem::zeroed();
        gti.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
        if GetGUIThreadInfo(tid, &mut gti) != 0 && !gti.hwndFocus.is_null() {
            return Some(Focus { hwnd: gti.hwndFocus, pid });
        }
        Some(Focus { hwnd: fg, pid })
    }
}

/// True when the focus we captured is still the place keys would go.
pub fn focus_alive(focus: Focus) -> bool {
    unsafe {
        if IsWindow(focus.hwnd) == 0 {
            return false;
        }
        match current_focus() {
            Some(now) => now.hwnd == focus.hwnd && now.pid == focus.pid,
            None => false,
        }
    }
}

/// Test-only escape hatch: when XCLIP_TEST_INPUT=1, the hook also reacts to
/// synthetic (injected) keystrokes so automated tests can drive it.
pub fn testing_input_enabled() -> bool {
    std::env::var_os("XCLIP_TEST_INPUT").is_some()
}

/// Marker placed in dwExtraInfo of every injected event so the low-level
/// hook can ignore our own output instead of swallowing it recursively.
pub fn injected_marker() -> usize {
    // Any stable, unusual value works; it only has to be unique to us.
    0x58_43_4c_50 // "XCLP"
}

/// Heuristic: is this window a classic edit control that honours WM_COPY?
pub fn is_classic_edit(focus: Focus) -> bool {
    if focus.hwnd.is_null() {
        return false;
    }
    let Ok(class) = window_class(focus.hwnd) else {
        return false;
    };
    class == "Edit" || class.starts_with("RichEdit") || class == "RICHEDIT60W"
}

fn window_class(hwnd: HWND) -> Result<String, String> {
    let mut buf = [0u16; 256];
    let n = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32)
    };
    if n <= 0 {
        return Err(last_error());
    }
    Ok(String::from_utf16_lossy(&buf[..n as usize]))
}

/// Post WM_COPY / WM_PASTE. Only useful for classic edit controls; the
/// backends probe before using it.
pub fn post_window_command(hwnd: HWND, paste: bool) -> bool {
    unsafe {
        let msg = if paste { WM_PASTE } else { WM_COPY };
        let ok = SendMessageW(hwnd, msg, 0, 0);
        // WM_COPY/WM_PASTE return TRUE-ish on success in most controls; the
        // caller treats 0 as "unsupported" and falls back to keystrokes.
        ok != 0
    }
}

// ---------------------------------------------------------------------------
// Single instance
// ---------------------------------------------------------------------------

pub struct MutexHandle(*mut c_void);
unsafe impl Send for MutexHandle {}

impl Drop for MutexHandle {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0 as HANDLE);
        }
    }
}

/// Acquire the single-instance mutex. Returns None when another copy runs.
pub fn acquire_single_instance(name: &str) -> Option<MutexHandle> {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let h = CreateMutexW(std::ptr::null_mut(), 0, wide.as_ptr());
        if h.is_null() {
            return None;
        }
        if GetLastError() == ERROR_ALREADY_EXISTS {
            windows_sys::Win32::Foundation::CloseHandle(h);
            return None;
        }
        Some(MutexHandle(h as *mut c_void))
    }
}

pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

/// Minimal message-only window used as the notification sink for hot reload
/// and tray callbacks.
pub struct MessageWindow {
    pub hwnd: HWND,
}

impl MessageWindow {
    pub fn new(class_name: &str, wndproc: unsafe extern "system" fn(HWND, u32, usize, isize) -> isize) -> Option<Self> {
        let wide: Vec<u16> = class_name.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let hinstance = windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(
                std::ptr::null(),
            );
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: wide.as_ptr(),
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                wide.as_ptr(),
                wide.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null_mut(),
            );
            if hwnd.is_null() {
                None
            } else {
                Some(Self { hwnd })
            }
        }
    }
}

impl Drop for MessageWindow {
    fn drop(&mut self) {
        unsafe {
            if !self.hwnd.is_null() {
                DestroyWindow(self.hwnd);
            }
        }
    }
}

/// Standard blocking message loop. Used by the hook thread and the UI thread.
pub fn run_message_loop() {
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Nudge a message loop out of GetMessage.
pub fn post_quit(hwnd: HWND) {
    const WM_QUIT_LOCAL: u32 = 0x0012;
    unsafe {
        let _ = PostMessageW(hwnd, WM_QUIT_LOCAL, 0, 0);
    }
}
