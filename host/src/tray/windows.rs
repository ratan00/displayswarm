//! The Windows notification-area icon.
//!
//! Win32 tray icons belong to a window, and that window's thread must pump
//! messages, so the icon lives on its own thread with a hidden window
//! ([`TrayWindow`]). The async side ([`run`]) keeps the device list in a shared
//! [`Snapshot`] and posts [`WM_APP_REFRESH`] when it changed; a click builds the
//! popup menu from [`super::menu_model`] (the same entries the Linux tray
//! shows) and sends the chosen [`TrayCmd`] back over a channel.
//!
//! Notifications are balloon tips on the icon (`NIF_INFO`), with the same text
//! the Linux notifications use ([`super::notify::notice_for`]). A click on the
//! balloon opens the window, which is where the choices are.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use super::{menu_model, run_cmd, MenuEntry, TrayCmd};
use crate::daemon::core::DaemonCore;
use crate::ipc::proto::{DeviceInfo, Event};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NIN_BALLOONUSERCLICK, NIN_SELECT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow, DispatchMessageW,
    GetCursorPos, GetMessageW, GetWindowLongPtrW, LoadIconW, PostMessageW, PostQuitMessage, RegisterClassW,
    RegisterWindowMessageW, SetForegroundWindow, SetWindowLongPtrW, TrackPopupMenu, TranslateMessage, CW_USEDEFAULT,
    GWLP_USERDATA, HMENU, IDI_APPLICATION, IDI_WARNING, MF_CHECKED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MSG, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CLOSE,
    WM_CONTEXTMENU, WM_DESTROY, WM_NCCREATE, WNDCLASSW, WS_OVERLAPPED, CREATESTRUCTW,
};

/// The icon's callback message (`uCallbackMessage`).
const WM_APP_TRAY: u32 = WM_APP + 1;
/// Posted by the async side: the snapshot changed, refresh tooltip and icon.
const WM_APP_REFRESH: u32 = WM_APP + 2;
/// Posted by the async side: show the balloon stored in the snapshot.
const WM_APP_BALLOON: u32 = WM_APP + 3;
/// The icon's id within our window.
const ICON_ID: u32 = 1;

/// What the icon shows; written by the async side, read by the window thread.
#[derive(Default)]
struct Snapshot {
    devices: Vec<DeviceInfo>,
    server_running: bool,
    balloon: Option<(String, String)>,
}

struct TrayWindow {
    hwnd: HWND,
    shared: Arc<Mutex<Snapshot>>,
    tx: mpsc::UnboundedSender<TrayCmd>,
    taskbar_created: u32,
    /// The menu ids of the entries in the menu currently shown.
    commands: Vec<TrayCmd>,
}

/// Copies `s` into a fixed UTF-16 field, truncated and NUL-terminated.
fn fill(dst: &mut [u16], s: &str) {
    let n = dst.len().saturating_sub(1);
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    dst[..units.len()].copy_from_slice(&units);
    dst[units.len()] = 0;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

impl TrayWindow {
    fn icon_data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        }
    }

    /// Adds the icon (again, after Explorer restarted) with the current tip.
    fn add_icon(&self) {
        let mut nid = self.icon_data();
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = WM_APP_TRAY;
        self.decorate(&mut nid);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_ADD, &nid);
            nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
            let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
        }
    }

    /// Tooltip and icon from the snapshot: a warning icon while a connected
    /// device waits for a role, as the Linux tray's "needs attention".
    fn decorate(&self, nid: &mut NOTIFYICONDATAW) {
        let snap = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        let attention = super::needs_attention(&snap.devices);
        let connected = snap.devices.iter().filter(|d| d.connected).count();
        let tip = match (snap.server_running, connected) {
            (false, _) => "DisplaySwarm (server stopped)".to_string(),
            (true, 0) => "DisplaySwarm: waiting for a device".to_string(),
            (true, 1) => "DisplaySwarm: 1 device connected".to_string(),
            (true, n) => format!("DisplaySwarm: {n} devices connected"),
        };
        fill(&mut nid.szTip, &tip);
        nid.hIcon = unsafe { LoadIconW(None, if attention { IDI_WARNING } else { IDI_APPLICATION }) }.unwrap_or_default();
    }

    fn refresh(&self) {
        let mut nid = self.icon_data();
        nid.uFlags = NIF_ICON | NIF_TIP;
        self.decorate(&mut nid);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn balloon(&self) {
        let Some((title, body)) = self.shared.lock().unwrap_or_else(|e| e.into_inner()).balloon.take() else { return };
        let mut nid = self.icon_data();
        nid.uFlags = NIF_INFO;
        nid.dwInfoFlags = NIIF_INFO;
        fill(&mut nid.szInfoTitle, &title);
        // An empty body hides the balloon, so repeat the title then.
        fill(&mut nid.szInfo, if body.is_empty() { &title } else { &body });
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    /// Appends `entries` to `menu`, recording each command's id in `commands`.
    fn build(menu: HMENU, entries: &[MenuEntry], commands: &mut Vec<TrayCmd>) {
        for e in entries {
            unsafe {
                match e {
                    MenuEntry::Separator => {
                        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
                    }
                    MenuEntry::Item { label, cmd, checked, enabled } => {
                        let mut flags = MF_STRING;
                        if *checked {
                            flags |= MF_CHECKED;
                        }
                        if !*enabled || cmd.is_none() {
                            flags |= MF_GRAYED;
                        }
                        let id = match cmd {
                            Some(c) => {
                                commands.push(c.clone());
                                commands.len() // ids start at 1; 0 means "nothing chosen"
                            }
                            None => 0,
                        };
                        let text = wide(label);
                        let _ = AppendMenuW(menu, flags, id, PCWSTR(text.as_ptr()));
                    }
                    MenuEntry::Sub { label, entries } => {
                        let Ok(sub) = CreatePopupMenu() else { continue };
                        Self::build(sub, entries, commands);
                        let text = wide(label);
                        let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(text.as_ptr()));
                    }
                }
            }
        }
    }

    fn show_menu(&mut self) {
        let entries = {
            let snap = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            menu_model(&snap.devices, snap.server_running)
        };
        self.commands.clear();
        unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            Self::build(menu, &entries, &mut self.commands);
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            // Without this the menu does not close when the user clicks elsewhere.
            let _ = SetForegroundWindow(self.hwnd);
            let chosen = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                None,
            );
            let _ = DestroyMenu(menu);
            let id = chosen.0 as usize;
            if id >= 1 {
                if let Some(cmd) = self.commands.get(id - 1) {
                    let _ = self.tx.send(cmd.clone());
                }
            }
        }
    }

    fn on_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_APP_TRAY => {
                // NOTIFYICON_VERSION_4: the event is the low word of lParam.
                match (lparam.0 as u32) & 0xFFFF {
                    NIN_SELECT | NIN_BALLOONUSERCLICK => {
                        let _ = self.tx.send(TrayCmd::OpenUi);
                    }
                    WM_CONTEXTMENU => self.show_menu(),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_APP_REFRESH => {
                self.refresh();
                Some(LRESULT(0))
            }
            WM_APP_BALLOON => {
                self.balloon();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                unsafe {
                    let _ = Shell_NotifyIconW(NIM_DELETE, &self.icon_data());
                    let _ = DestroyWindow(self.hwnd);
                }
                Some(LRESULT(0))
            }
            WM_DESTROY => {
                unsafe { PostQuitMessage(0) };
                Some(LRESULT(0))
            }
            m if m == self.taskbar_created && m != 0 => {
                // Explorer restarted: the icon is gone until added again.
                self.add_icon();
                Some(LRESULT(0))
            }
            _ => {
                let _ = wparam;
                None
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut TrayWindow;
    if let Some(tw) = ptr.as_mut() {
        if tw.hwnd.0.is_null() {
            tw.hwnd = hwnd;
        }
        if let Some(r) = tw.on_message(msg, wparam, lparam) {
            return r;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// A handle the async side uses to poke the window thread.
#[derive(Clone, Copy)]
struct WindowHandle(isize);

impl WindowHandle {
    fn post(&self, msg: u32) {
        unsafe {
            let _ = PostMessageW(HWND(self.0 as *mut _), msg, WPARAM(0), LPARAM(0));
        }
    }
}

/// Creates the hidden window and the icon, then pumps messages until the
/// window is closed. Sends the window handle once it exists.
fn window_thread(
    shared: Arc<Mutex<Snapshot>>,
    tx: mpsc::UnboundedSender<TrayCmd>,
    ready: std::sync::mpsc::Sender<Result<WindowHandle, String>>,
) {
    unsafe {
        let instance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => return drop(ready.send(Err(e.to_string()))),
        };
        let class = w!("DisplaySwarmTray");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        // Boxed so its address is stable for the window's user data.
        let mut state = Box::new(TrayWindow {
            hwnd: HWND::default(),
            shared,
            tx,
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
            commands: Vec::new(),
        });
        // A hidden top-level window rather than a message-only one: only
        // top-level windows receive the TaskbarCreated broadcast.
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("DisplaySwarm"),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            instance,
            Some(&mut *state as *mut TrayWindow as *const _),
        ) {
            Ok(h) => h,
            Err(e) => return drop(ready.send(Err(format!("CreateWindowExW: {e}")))),
        };
        state.hwnd = hwnd;
        state.add_icon();
        let _ = ready.send(Ok(WindowHandle(hwnd.0 as isize)));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // The window is gone; `state` may be freed now.
        drop(state);
    }
}

/// Runs the tray until the daemon quits.
pub async fn run(core: Arc<DaemonCore>) {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<TrayCmd>();
    let shared = Arc::new(Mutex::new(Snapshot {
        devices: core.devices(),
        server_running: core.server_info().running,
        balloon: None,
    }));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread_shared = shared.clone();
    let spawned = std::thread::Builder::new()
        .name("displayswarm-tray".into())
        .spawn(move || window_thread(thread_shared, cmd_tx, ready_tx));
    let window = match spawned.map_err(|e| e.to_string()).and_then(|_| {
        ready_rx.recv().map_err(|e| e.to_string()).and_then(|r| r)
    }) {
        Ok(w) => {
            log::info!("Tray icon added");
            core.set_tray_mode("tray");
            w
        }
        Err(e) => {
            log::warn!("No tray icon: {e}");
            core.set_tray_mode("none");
            return;
        }
    };

    let mut quit = core.quit_signal();
    let mut events = core.subscribe();
    let mut ui_child: Option<std::process::Child> = None;
    loop {
        tokio::select! {
            _ = quit.changed() => break,
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { break };
                run_cmd(&core, cmd, &mut ui_child).await;
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(e) => e,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Event::DevicesChanged { devices: core.devices() },
                    Err(_) => break,
                };
                if matches!(ev, Event::DevicesChanged { .. } | Event::ServerChanged { .. }) {
                    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
                    s.devices = core.devices();
                    s.server_running = core.server_info().running;
                    drop(s);
                    window.post(WM_APP_REFRESH);
                }
                if let Some(n) = super::notify::notice_for(&ev, true) {
                    shared.lock().unwrap_or_else(|e| e.into_inner()).balloon = Some((n.summary, n.body));
                    window.post(WM_APP_BALLOON);
                }
            }
        }
    }
    window.post(WM_CLOSE);
}
