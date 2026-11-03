//! The notification-area icon, and one KKCUT at a time.
//!
//! The icon lives on its own thread with a hidden window. A second KKCUT
//! finds that window by its class name and asks it to show the first one's
//! window, then quits.

use crate::wide;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const CLASS: &str = "KKCUT.Tray";
const WM_TRAY: u32 = WM_APP + 1;
/// Posted by a second KKCUT: show the window.
const WM_SHOW: u32 = WM_APP + 2;
/// Posted when the main switch changes: update the tooltip.
const WM_STATE: u32 = WM_APP + 3;

const ID_OPEN: usize = 1;
const ID_TOGGLE: usize = 2;
const ID_EXIT: usize = 3;

/// What the icon asks of the window.
#[derive(Debug, Clone, Copy)]
pub enum Command {
    Show,
    Toggle,
    Quit,
}

static WINDOW: AtomicIsize = AtomicIsize::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static HANDLER: OnceLock<fn(Command)> = OnceLock::new();

/// True for the first KKCUT. A later one shows the first one's window
/// (unless it was started hidden, by autostart) and must quit.
pub fn single_instance(hidden: bool) -> bool {
    unsafe {
        // Kept open for the life of the process.
        CreateMutexW(std::ptr::null(), 0, wide("Local\\KKCUT.Instance").as_ptr());
        if GetLastError() != ERROR_ALREADY_EXISTS {
            return true;
        }
        if !hidden {
            let other = FindWindowW(wide(CLASS).as_ptr(), std::ptr::null());
            if !other.is_null() {
                PostMessageW(other, WM_SHOW, 0, 0);
            }
        }
        false
    }
}

/// Starts the icon. `handler` runs on the icon's thread.
pub fn spawn(handler: fn(Command), enabled: bool) {
    let _ = HANDLER.set(handler);
    ENABLED.store(enabled, Ordering::Release);
    std::thread::spawn(|| unsafe { run() });
}

/// The tooltip follows the main switch.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
    let hwnd = WINDOW.load(Ordering::Acquire);
    if hwnd != 0 {
        unsafe { PostMessageW(hwnd as HWND, WM_STATE, 0, 0) };
    }
}

/// Takes the icon off before KKCUT quits.
pub fn remove() {
    let hwnd = WINDOW.load(Ordering::Acquire);
    if hwnd != 0 {
        let mut nid = icon_data(hwnd as HWND);
        unsafe { Shell_NotifyIconW(NIM_DELETE, &mut nid) };
    }
}

fn send(command: Command) {
    if let Some(handler) = HANDLER.get() {
        handler(command);
    }
}

fn tip() -> &'static str {
    if ENABLED.load(Ordering::Acquire) {
        "KKCUT：自動切歌中"
    } else {
        "KKCUT：已停止"
    }
}

fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut nid: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = 1;
    nid
}

unsafe fn show_icon(hwnd: HWND, add: bool) {
    let mut nid = icon_data(hwnd);
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAY;
    unsafe {
        // Resource 1 is the application icon (build.rs).
        nid.hIcon = LoadImageW(
            GetModuleHandleW(std::ptr::null()),
            1 as _,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        ) as _;
    }
    // NOTIFYICONDATAW is packed on 32-bit Windows: fields are written whole.
    let mut text = [0u16; 128];
    for (slot, c) in text.iter_mut().zip(tip().encode_utf16()) {
        *slot = c;
    }
    nid.szTip = text;
    unsafe { Shell_NotifyIconW(if add { NIM_ADD } else { NIM_MODIFY }, &nid) };
}

unsafe fn menu(hwnd: HWND) {
    unsafe {
        let menu = CreatePopupMenu();
        let toggle = if ENABLED.load(Ordering::Acquire) {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
        AppendMenuW(menu, MF_STRING, ID_OPEN, wide("開啟 KKCUT").as_ptr());
        AppendMenuW(menu, MF_STRING | toggle, ID_TOGGLE, wide("自動切歌").as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        AppendMenuW(menu, MF_STRING, ID_EXIT, wide("結束").as_ptr());
        SetMenuDefaultItem(menu, ID_OPEN as u32, 0);
        let mut at = POINT { x: 0, y: 0 };
        GetCursorPos(&mut at);
        // The menu closes on a click elsewhere only while KKCUT is in front.
        SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
            at.x,
            at.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        match chosen as usize {
            ID_OPEN => send(Command::Show),
            ID_TOGGLE => send(Command::Toggle),
            ID_EXIT => send(Command::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match lp as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => send(Command::Show),
                WM_RBUTTONUP | WM_CONTEXTMENU => unsafe { menu(hwnd) },
                _ => {}
            }
            0
        }
        WM_SHOW => {
            send(Command::Show);
            0
        }
        WM_STATE => {
            unsafe { show_icon(hwnd, false) };
            0
        }
        // Explorer restarted: the icon is gone and must be added again.
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Acquire) => {
            unsafe { show_icon(hwnd, true) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

unsafe fn run() {
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(window_proc);
        wc.hInstance = instance;
        wc.lpszClassName = class.as_ptr();
        RegisterClassW(&wc);
        TASKBAR_CREATED.store(
            RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            Ordering::Release,
        );
        // A hidden top-level window: message-only windows miss TaskbarCreated.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("KKCUT").as_ptr(),
            0,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            crate::log("Tray icon window not created");
            return;
        }
        WINDOW.store(hwnd as isize, Ordering::Release);
        show_icon(hwnd, true);
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
