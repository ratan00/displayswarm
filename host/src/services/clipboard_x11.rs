//! Clipboard through X11 selections, for X11 sessions and for Wayland
//! desktops without the data-control protocol (GNOME), where Xwayland's
//! clipboard is bridged to the Wayland one by the compositor.
//!
//! Two connections, so reading never waits behind serving:
//!
//! * The **reader** owns a hidden window, asks XFixes to be told when the
//!   `CLIPBOARD` owner changes, and on [`X11Clipboard::read`] converts the
//!   selection into a property on that window (with `INCR` for large data).
//!   It reads only after a change, so an idle clipboard costs nothing.
//! * The **owner** thread holds what came from the phone, owns `CLIPBOARD`
//!   with a second window, and answers other clients' `SelectionRequest`s
//!   (`TARGETS`, the text targets, `image/png`).
//!
//! The ICCCM conventions followed are those of section 2 ("Peer-to-Peer
//! Communication by Means of Selections").

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Property, SelectionNotifyEvent,
    SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::CURRENT_TIME;

use super::clipboard::{ClipContent, ClipboardBackend, MAX_IMAGE, MAX_TEXT};

/// How long another client gets to answer a conversion.
const CONVERT_TIMEOUT: Duration = Duration::from_secs(2);

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    utf8: Atom,
    text: Atom,
    text_plain_utf8: Atom,
    text_plain: Atom,
    png: Atom,
    incr: Atom,
    prop: Atom,
    wake: Atom,
    take: Atom,
}

impl Atoms {
    fn intern(conn: &RustConnection) -> Result<Self, String> {
        let a = |name: &[u8]| -> Result<Atom, String> {
            Ok(conn.intern_atom(false, name).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?.atom)
        };
        Ok(Self {
            clipboard: a(b"CLIPBOARD")?,
            targets: a(b"TARGETS")?,
            utf8: a(b"UTF8_STRING")?,
            text: a(b"TEXT")?,
            text_plain_utf8: a(b"text/plain;charset=utf-8")?,
            text_plain: a(b"text/plain")?,
            png: a(b"image/png")?,
            incr: a(b"INCR")?,
            prop: a(b"DISPLAYSWARM_SEL")?,
            wake: a(b"DISPLAYSWARM_WAKE")?,
            take: a(b"DISPLAYSWARM_TAKE")?,
        })
    }

    fn is_text(&self, t: Atom) -> bool {
        t == self.utf8 || t == self.text || t == self.text_plain_utf8 || t == self.text_plain || t == u32::from(AtomEnum::STRING)
    }
}

fn hidden_window(conn: &RustConnection, screen: usize, mask: EventMask) -> Result<Window, String> {
    let root = conn.setup().roots.get(screen).ok_or("no X screen")?.root;
    let win = conn.generate_id().map_err(|e| e.to_string())?;
    conn.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().event_mask(mask),
    )
    .map_err(|e| e.to_string())?;
    Ok(win)
}

/// What the owner thread serves.
#[derive(Default)]
struct Owned {
    content: Option<ClipContent>,
    stop: bool,
}

pub struct X11Clipboard {
    conn: RustConnection,
    win: Window,
    atoms: Atoms,
    /// The owner changed since the last read (or nothing was read yet).
    changed: bool,
    /// The owner thread's window: a change to it is our own write.
    owner_win: Window,
    owned: Arc<Mutex<Owned>>,
    owner_thread: Option<std::thread::JoinHandle<()>>,
}

impl X11Clipboard {
    /// Connects to `$DISPLAY`. `Err` when there is no X server (or Xwayland).
    pub fn connect() -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("no X display: {e}"))?;
        let atoms = Atoms::intern(&conn)?;
        let win = hidden_window(&conn, screen, EventMask::PROPERTY_CHANGE)?;
        conn.xfixes_query_version(5, 0).map_err(|e| e.to_string())?.reply().map_err(|e| format!("XFixes: {e}"))?;
        conn.xfixes_select_selection_input(
            win,
            atoms.clipboard,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )
        .map_err(|e| e.to_string())?;
        conn.flush().map_err(|e| e.to_string())?;

        let (oconn, oscreen) = x11rb::connect(None).map_err(|e| format!("no X display: {e}"))?;
        let owner_win = hidden_window(&oconn, oscreen, EventMask::NO_EVENT)?;
        oconn.flush().map_err(|e| e.to_string())?;
        let owned = Arc::new(Mutex::new(Owned::default()));
        let o2 = owned.clone();
        let owner_thread = std::thread::Builder::new()
            .name("clipboard-x11-owner".into())
            .spawn(move || serve(oconn, owner_win, o2))
            .map_err(|e| e.to_string())?;
        Ok(Self { conn, win, atoms, changed: true, owner_win, owned, owner_thread: Some(owner_thread) })
    }

    /// Drains pending events; returns whether the clipboard owner changed to
    /// someone other than us.
    fn poll_changes(&mut self) {
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            if let Event::XfixesSelectionNotify(n) = ev {
                if n.selection == self.atoms.clipboard {
                    self.changed = n.owner != self.owner_win;
                }
            }
        }
    }

    /// Waits for an event matching `want`, up to `deadline`.
    fn wait_event(&mut self, deadline: Instant, mut want: impl FnMut(&Event) -> bool) -> Option<Event> {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(ev)) => {
                    if let Event::XfixesSelectionNotify(n) = &ev {
                        if n.selection == self.atoms.clipboard && n.owner != self.owner_win {
                            self.changed = true;
                        }
                    }
                    if want(&ev) {
                        return Some(ev);
                    }
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => return None,
            }
        }
    }

    /// Converts the clipboard to `target` and returns its bytes and type.
    fn convert(&mut self, target: Atom, limit: usize) -> Option<(Atom, Vec<u8>)> {
        let (win, prop, sel) = (self.win, self.atoms.prop, self.atoms.clipboard);
        self.conn.delete_property(win, prop).ok()?;
        self.conn.convert_selection(win, sel, target, prop, CURRENT_TIME).ok()?;
        self.conn.flush().ok()?;
        let deadline = Instant::now() + CONVERT_TIMEOUT;
        let notify = self.wait_event(deadline, |e| matches!(e, Event::SelectionNotify(n) if n.requestor == win))?;
        let Event::SelectionNotify(n) = notify else { return None };
        if n.property == x11rb::NONE {
            return None; // the owner refused this target
        }
        let reply = self.conn.get_property(true, win, prop, AtomEnum::ANY, 0, (limit as u32 / 4) + 1).ok()?.reply().ok()?;
        if reply.type_ != self.atoms.incr {
            if reply.value.len() > limit {
                return None;
            }
            return Some((reply.type_, reply.value));
        }
        // INCR: the owner sends chunks, each announced by a new property value;
        // deleting it asks for the next, and an empty one ends the transfer.
        let mut data = Vec::new();
        let mut ty = x11rb::NONE;
        loop {
            let deadline = Instant::now() + CONVERT_TIMEOUT;
            self.wait_event(deadline, |e| {
                matches!(e, Event::PropertyNotify(p) if p.window == win && p.atom == prop && p.state == Property::NEW_VALUE)
            })?;
            let chunk = self.conn.get_property(true, win, prop, AtomEnum::ANY, 0, u32::MAX / 4).ok()?.reply().ok()?;
            self.conn.flush().ok()?;
            if chunk.value.is_empty() {
                return Some((ty, data));
            }
            ty = chunk.type_;
            data.extend_from_slice(&chunk.value);
            if data.len() > limit {
                return None;
            }
        }
    }

    fn targets(&mut self) -> Vec<Atom> {
        match self.convert(self.atoms.targets, 64 * 1024) {
            Some((_, bytes)) => bytes.chunks_exact(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]])).collect(),
            None => Vec::new(),
        }
    }
}

impl ClipboardBackend for X11Clipboard {
    fn read(&mut self) -> Option<ClipContent> {
        self.poll_changes();
        if !self.changed {
            return None;
        }
        self.changed = false;
        let targets = self.targets();
        if targets.contains(&self.atoms.png) {
            let (_, png) = self.convert(self.atoms.png, MAX_IMAGE + 1)?;
            return (!png.is_empty()).then_some(ClipContent::Png(png));
        }
        let text_target = [self.atoms.utf8, self.atoms.text_plain_utf8, u32::from(AtomEnum::STRING)]
            .into_iter()
            .find(|t| targets.contains(t) || targets.is_empty())?;
        let (_, bytes) = self.convert(text_target, MAX_TEXT + 1)?;
        (!bytes.is_empty()).then(|| ClipContent::Text(String::from_utf8_lossy(&bytes).into_owned()))
    }

    fn write(&mut self, c: &ClipContent) -> Result<(), String> {
        self.owned.lock().unwrap_or_else(|e| e.into_inner()).content = Some(c.clone());
        // The X server sends SelectionRequests to the client that called
        // SetSelectionOwner, so the owner thread must make that call itself:
        // ask it to, then wait until it owns the selection.
        let ev = x11rb::protocol::xproto::ClientMessageEvent::new(32, self.owner_win, self.atoms.take, [0u32; 5]);
        self.conn.send_event(false, self.owner_win, EventMask::NO_EVENT, ev).map_err(|e| e.to_string())?;
        self.conn.flush().map_err(|e| e.to_string())?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let owner = self
                .conn
                .get_selection_owner(self.atoms.clipboard)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?
                .owner;
            if owner == self.owner_win {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("could not take the X clipboard".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for X11Clipboard {
    fn drop(&mut self) {
        self.owned.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        // Wake the owner thread out of its blocking wait with a client message.
        let ev = x11rb::protocol::xproto::ClientMessageEvent::new(32, self.owner_win, self.atoms.wake, [0u32; 5]);
        let _ = self.conn.send_event(false, self.owner_win, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
        if let Some(t) = self.owner_thread.take() {
            let _ = t.join();
        }
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.flush();
    }
}

/// The owner thread: answers requests for what the phone sent until stopped.
fn serve(conn: RustConnection, win: Window, owned: Arc<Mutex<Owned>>) {
    let Ok(atoms) = Atoms::intern(&conn) else { return };
    let max = conn.maximum_request_bytes().saturating_sub(64);
    loop {
        let Ok(ev) = conn.wait_for_event() else { return };
        if owned.lock().unwrap_or_else(|e| e.into_inner()).stop {
            let _ = conn.destroy_window(win);
            let _ = conn.flush();
            return;
        }
        match ev {
            Event::SelectionRequest(req) => {
                let content = owned.lock().unwrap_or_else(|e| e.into_inner()).content.clone();
                let ok = answer(&conn, &atoms, &req, content.as_ref(), max);
                let property = if ok { effective_property(&req) } else { x11rb::NONE };
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: req.time,
                    requestor: req.requestor,
                    selection: req.selection,
                    target: req.target,
                    property,
                };
                let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
                let _ = conn.flush();
            }
            Event::ClientMessage(m) if m.type_ == atoms.take => {
                let _ = conn.set_selection_owner(win, atoms.clipboard, CURRENT_TIME);
                let _ = conn.flush();
            }
            Event::SelectionClear(c) if c.selection == atoms.clipboard => {
                owned.lock().unwrap_or_else(|e| e.into_inner()).content = None;
            }
            _ => {}
        }
    }
}

/// ICCCM: a request with no property is from an obsolete client; use the target.
fn effective_property(req: &SelectionRequestEvent) -> Atom {
    if req.property == x11rb::NONE {
        req.target
    } else {
        req.property
    }
}

/// Writes the requested target onto the requestor's property. False refuses.
fn answer(conn: &RustConnection, atoms: &Atoms, req: &SelectionRequestEvent, content: Option<&ClipContent>, max: usize) -> bool {
    let Some(content) = content else { return false };
    let prop = effective_property(req);
    let put8 = |ty: Atom, data: &[u8]| -> bool {
        // Too large for one request: refuse rather than implement INCR sending;
        // with BIG-REQUESTS the limit is far above what the phone sends.
        data.len() <= max
            && conn.change_property8(PropMode::REPLACE, req.requestor, prop, ty, data).is_ok()
    };
    match content {
        ClipContent::Text(t) => {
            if req.target == atoms.targets {
                let list = [atoms.targets, atoms.utf8, atoms.text_plain_utf8, atoms.text, AtomEnum::STRING.into()];
                conn.change_property32(PropMode::REPLACE, req.requestor, prop, AtomEnum::ATOM, &list).is_ok()
            } else if req.target == u32::from(AtomEnum::STRING) {
                // STRING is ISO Latin-1: characters outside it become '?'.
                let latin1: Vec<u8> = t.chars().map(|c| if (c as u32) < 256 { c as u8 } else { b'?' }).collect();
                put8(AtomEnum::STRING.into(), &latin1)
            } else if atoms.is_text(req.target) {
                put8(if req.target == atoms.text { atoms.utf8 } else { req.target }, t.as_bytes())
            } else {
                false
            }
        }
        ClipContent::Png(p) => {
            if req.target == atoms.targets {
                let list = [atoms.targets, atoms.png];
                conn.change_property32(PropMode::REPLACE, req.requestor, prop, AtomEnum::ATOM, &list).is_ok()
            } else if req.target == atoms.png {
                put8(atoms.png, p)
            } else {
                false
            }
        }
    }
}

/// Needs an X server and `xclip` (another X client to exchange with):
/// `xvfb-run cargo test clipboard_x11 -- --ignored`.
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    fn xclip_set(target: &str, data: &[u8]) -> std::process::Child {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard", "-t", target, "-i"])
            .stdin(Stdio::piped())
            .spawn()
            .expect("xclip");
        child.stdin.take().unwrap().write_all(data).unwrap();
        // xclip forks a server and the parent exits once it owns the selection.
        child
    }

    fn xclip_get(target: &str) -> Vec<u8> {
        Command::new("xclip").args(["-selection", "clipboard", "-t", target, "-o"]).output().expect("xclip").stdout
    }

    fn read_until(cb: &mut X11Clipboard, want: impl Fn(&ClipContent) -> bool) -> Option<ClipContent> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(c) = cb.read() {
                if want(&c) {
                    return Some(c);
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    #[test]
    #[ignore = "needs an X server and xclip"]
    fn exchanges_text_images_and_large_text_with_another_client() {
        let mut cb = X11Clipboard::connect().expect("X display");
        let _ = cb.read(); // whatever was there at start

        let mut c = xclip_set("UTF8_STRING", "h\u{e9}llo from X".as_bytes());
        let _ = c.wait();
        assert_eq!(read_until(&mut cb, |_| true), Some(ClipContent::Text("h\u{e9}llo from X".into())));
        assert_eq!(cb.read(), None, "no change, no read");

        // Large enough that xclip sends it with INCR.
        let big: String = "0123456789abcdef".repeat(64 * 1024);
        let mut c = xclip_set("UTF8_STRING", big.as_bytes());
        let _ = c.wait();
        assert_eq!(read_until(&mut cb, |c| matches!(c, ClipContent::Text(t) if t.len() == big.len())), Some(ClipContent::Text(big)));

        cb.write(&ClipContent::Text("from the phone \u{1F600}".into())).unwrap();
        assert_eq!(String::from_utf8(xclip_get("UTF8_STRING")).unwrap(), "from the phone \u{1F600}");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(cb.read(), None, "our own write is not read back");

        let png = super::super::clipboard_windows::dib::png_encode(2, 1, &[1, 2, 3, 255, 4, 5, 6, 255]).unwrap();
        cb.write(&ClipContent::Png(png.clone())).unwrap();
        assert_eq!(xclip_get("image/png"), png);
        let targets = String::from_utf8(xclip_get("TARGETS")).unwrap();
        assert!(targets.contains("image/png"), "{targets}");

        let mut c = xclip_set("image/png", &png);
        let _ = c.wait();
        assert_eq!(read_until(&mut cb, |c| matches!(c, ClipContent::Png(_))), Some(ClipContent::Png(png)));
    }
}
