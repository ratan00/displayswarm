//! Clipboard sync (Phase 8): text and PNG images, both ways.
//!
//! The Wayland clipboard is read by polling and written through
//! `wl-clipboard-rs` (wlr/ext data-control, which KWin implements). All of it
//! runs on one worker thread per session, so `on_message` never blocks.
//!
//! Loop prevention: [`SyncState`] remembers a hash of the content last seen in
//! either direction. Content that just arrived from the phone is written to
//! the clipboard and would be read back on the next poll; the hash matches, so
//! it is not sent again. The clipboard's content at connect time is recorded
//! but never sent.
//!
//! Limits: images up to [`MAX_IMAGE`] (8 MiB), text up to [`MAX_TEXT`]. Larger
//! content is ignored (and remembered, so it is not retried every poll).
//! Images are re-read only every few seconds while the offered types do not
//! change, because reading one makes the source application re-encode it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::mpsc;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

use super::{ServiceCtx, ServiceOut, SessionService};
use crate::protocol::wire::Message;

pub const MAX_IMAGE: usize = 8 * 1024 * 1024;
pub const MAX_TEXT: usize = 4 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(500);
#[cfg(target_os = "linux")]
const IMAGE_REREAD: Duration = Duration::from_secs(3);
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipContent {
    Text(String),
    Png(Vec<u8>),
}

impl ClipContent {
    fn hash(&self) -> u64 {
        let mut h = DefaultHasher::new();
        match self {
            Self::Text(t) => (0u8, t.as_bytes()).hash(&mut h),
            Self::Png(p) => (1u8, p.as_slice()).hash(&mut h),
        }
        h.finish()
    }

    fn within_limits(&self) -> bool {
        match self {
            Self::Text(t) => !t.is_empty() && t.len() <= MAX_TEXT,
            Self::Png(p) => !p.is_empty() && p.len() <= MAX_IMAGE,
        }
    }

    pub fn to_message(&self) -> Message {
        match self {
            Self::Text(t) => Message::Clipboard { mime: "text/plain".into(), data: t.clone().into_bytes() },
            Self::Png(p) => Message::Clipboard { mime: "image/png".into(), data: p.clone() },
        }
    }

    /// `None` for a mime type we do not sync, an oversized payload, or bytes
    /// that are not a PNG.
    pub fn from_wire(mime: &str, data: &[u8]) -> Option<Self> {
        let base = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        let c = match base.as_str() {
            "text/plain" => Self::Text(String::from_utf8_lossy(data).into_owned()),
            "image/png" if data.starts_with(&PNG_MAGIC) => Self::Png(data.to_vec()),
            _ => return None,
        };
        c.within_limits().then_some(c)
    }
}

/// Loop prevention and limits; pure, so it is unit-tested.
#[derive(Default)]
pub struct SyncState {
    last: Option<u64>,
}

impl SyncState {
    /// Records what the clipboard held when the session began (never sent).
    pub fn seed(&mut self, c: &ClipContent) {
        self.last = Some(c.hash());
    }

    /// The local clipboard now holds `c`: returns whether to send it.
    pub fn local(&mut self, c: &ClipContent) -> bool {
        let h = c.hash();
        if self.last == Some(h) {
            return false;
        }
        self.last = Some(h); // also for oversized content: do not retry it
        c.within_limits()
    }

    /// `c` arrived from the phone and is about to be written locally.
    pub fn remote(&mut self, c: &ClipContent) {
        self.last = Some(c.hash());
    }
}

pub trait ClipboardBackend: Send {
    /// The current clipboard, or `None` if empty/unsupported.
    fn read(&mut self) -> Option<ClipContent>;
    fn write(&mut self, c: &ClipContent) -> Result<(), String>;
}

enum Cmd {
    Remote(ClipContent),
    Stop,
}

/// Worker loop: applies remote content, polls the local clipboard, and passes
/// outgoing messages to `send` (which returns false once the session is gone).
fn run_worker(
    mut backend: Box<dyn ClipboardBackend>,
    rx: mpsc::Receiver<Cmd>,
    mut send: impl FnMut(Message) -> bool,
    poll: Duration,
) {
    let mut state = SyncState::default();
    if let Some(c) = backend.read() {
        state.seed(&c);
    }
    loop {
        match rx.recv_timeout(poll) {
            Ok(Cmd::Remote(c)) => {
                state.remote(&c);
                if let Err(e) = backend.write(&c) {
                    log::warn!("Clipboard write failed: {e}");
                }
            }
            Ok(Cmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(c) = backend.read() {
                    if state.local(&c) && !send(c.to_message()) {
                        return;
                    }
                }
            }
        }
    }
}

/// Off Linux there is no clipboard backend yet: nothing is read or written.
#[cfg(not(target_os = "linux"))]
struct NoClipboard;

#[cfg(not(target_os = "linux"))]
impl ClipboardBackend for NoClipboard {
    fn read(&mut self) -> Option<ClipContent> {
        None
    }
    fn write(&mut self, _c: &ClipContent) -> Result<(), String> {
        Err("clipboard sync is not available on this platform".into())
    }
}

pub struct ClipboardService {
    tx: mpsc::Sender<Cmd>,
}

impl ClipboardService {
    pub fn start(ctx: &ServiceCtx) -> Self {
        #[cfg(target_os = "linux")]
        let backend: Box<dyn ClipboardBackend> = Box::new(WaylandClipboard::new());
        #[cfg(not(target_os = "linux"))]
        let backend: Box<dyn ClipboardBackend> = Box::new(NoClipboard);
        Self::with_backend(backend, ctx.out.clone(), POLL)
    }

    fn with_backend(backend: Box<dyn ClipboardBackend>, out: ServiceOut, poll: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("clipboard-sync".into())
            .spawn(move || run_worker(backend, rx, |m| out.blocking_send_bulk(m), poll));
        if let Err(e) = spawned {
            log::warn!("Clipboard worker did not start: {e}");
        }
        Self { tx }
    }
}

impl SessionService for ClipboardService {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn wants(&self, msg: &Message) -> bool {
        matches!(msg, Message::Clipboard { .. })
    }

    fn on_message(&mut self, msg: Message) {
        if let Message::Clipboard { mime, data } = msg {
            match ClipContent::from_wire(&mime, &data) {
                Some(c) => {
                    let _ = self.tx.send(Cmd::Remote(c));
                }
                None => log::debug!("Ignoring clipboard payload: {mime}, {} bytes", data.len()),
            }
        }
    }
}

impl Drop for ClipboardService {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
    }
}

/// The Wayland clipboard through `wl-clipboard-rs`.
#[cfg(target_os = "linux")]
pub struct WaylandClipboard {
    last_types: Vec<String>,
    last_image_read: Option<Instant>,
}

#[cfg(target_os = "linux")]
impl WaylandClipboard {
    pub fn new() -> Self {
        Self { last_types: Vec::new(), last_image_read: None }
    }
}

#[cfg(target_os = "linux")]
impl Default for WaylandClipboard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl ClipboardBackend for WaylandClipboard {
    fn read(&mut self) -> Option<ClipContent> {
        use std::io::Read;
        use wl_clipboard_rs::paste::{get_contents, get_mime_types, ClipboardType, MimeType, Seat};

        let mut types: Vec<String> =
            get_mime_types(ClipboardType::Regular, Seat::Unspecified).ok()?.into_iter().collect();
        types.sort();
        let changed = types != self.last_types;
        self.last_types = types.clone();

        if types.iter().any(|t| t == "image/png") {
            let due = changed || self.last_image_read.map_or(true, |t| t.elapsed() >= IMAGE_REREAD);
            if !due {
                return None;
            }
            self.last_image_read = Some(Instant::now());
            let (r, _) =
                get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Specific("image/png")).ok()?;
            let mut buf = Vec::new();
            // One byte over the cap is enough to know it is too big.
            r.take(MAX_IMAGE as u64 + 1).read_to_end(&mut buf).ok()?;
            return (!buf.is_empty()).then_some(ClipContent::Png(buf));
        }
        if !types.iter().any(|t| t.starts_with("text/plain") || t == "UTF8_STRING" || t == "STRING") {
            return None;
        }
        let (r, _) = get_contents(ClipboardType::Regular, Seat::Unspecified, MimeType::Text).ok()?;
        let mut buf = Vec::new();
        r.take(MAX_TEXT as u64 + 1).read_to_end(&mut buf).ok()?;
        (!buf.is_empty()).then(|| ClipContent::Text(String::from_utf8_lossy(&buf).into_owned()))
    }

    fn write(&mut self, c: &ClipContent) -> Result<(), String> {
        use wl_clipboard_rs::copy::{MimeType, Options, Source};
        let (data, mime) = match c {
            ClipContent::Text(t) => (t.clone().into_bytes(), MimeType::Text),
            ClipContent::Png(p) => (p.clone(), MimeType::Specific("image/png".into())),
        };
        // Serving stays in the foreground of its own thread: it ends when
        // another application (or the next sync) takes the selection over.
        std::thread::Builder::new()
            .name("clipboard-serve".into())
            .spawn(move || {
                let mut opts = Options::new();
                opts.foreground(true);
                if let Err(e) = opts.copy(Source::Bytes(data.into_boxed_slice()), mime) {
                    log::warn!("Clipboard copy failed: {e}");
                }
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn text(s: &str) -> ClipContent {
        ClipContent::Text(s.into())
    }

    #[test]
    fn loop_prevention() {
        let mut s = SyncState::default();
        s.seed(&text("old"));
        assert!(!s.local(&text("old")), "content at connect is not sent");
        assert!(s.local(&text("new")));
        assert!(!s.local(&text("new")), "not resent while unchanged");
        s.remote(&text("from phone"));
        assert!(!s.local(&text("from phone")), "no echo of what was just received");
        assert!(s.local(&text("new")), "a later local copy of older text is sent");
    }

    #[test]
    fn size_caps() {
        let mut s = SyncState::default();
        let mut big = PNG_MAGIC.to_vec();
        big.resize(MAX_IMAGE + 1, 0);
        let big = ClipContent::Png(big);
        assert!(!s.local(&big));
        assert!(!s.local(&big));
        let mut ok = PNG_MAGIC.to_vec();
        ok.resize(MAX_IMAGE, 0);
        assert!(s.local(&ClipContent::Png(ok)));
        assert!(!s.local(&text("")), "empty text is not sent");
    }

    #[test]
    fn wire_mapping() {
        assert_eq!(ClipContent::from_wire("text/plain;charset=utf-8", b"hi"), Some(text("hi")));
        assert_eq!(ClipContent::from_wire("image/png", b"not a png"), None);
        assert_eq!(ClipContent::from_wire("text/html", b"<b>"), None);
        let mut png = PNG_MAGIC.to_vec();
        png.extend_from_slice(&[1, 2, 3]);
        let c = ClipContent::from_wire("image/png", &png).unwrap();
        assert_eq!(c.to_message(), Message::Clipboard { mime: "image/png".into(), data: png });
        assert_eq!(text("é").to_message(), Message::Clipboard { mime: "text/plain".into(), data: "é".into() });
    }

    struct Fake(Arc<Mutex<Option<ClipContent>>>);
    impl ClipboardBackend for Fake {
        fn read(&mut self) -> Option<ClipContent> {
            self.0.lock().unwrap().clone()
        }
        fn write(&mut self, c: &ClipContent) -> Result<(), String> {
            *self.0.lock().unwrap() = Some(c.clone());
            Ok(())
        }
    }

    #[test]
    fn worker_sends_local_changes_but_never_echoes_remote_ones() {
        let clip = Arc::new(Mutex::new(Some(text("at connect"))));
        let sent = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel();
        let (c2, s2) = (clip.clone(), sent.clone());
        let t = std::thread::spawn(move || {
            run_worker(
                Box::new(Fake(c2)),
                rx,
                move |m| {
                    s2.lock().unwrap().push(m);
                    true
                },
                Duration::from_millis(10),
            )
        });
        std::thread::sleep(Duration::from_millis(60));
        assert!(sent.lock().unwrap().is_empty());
        *clip.lock().unwrap() = Some(text("copied here"));
        std::thread::sleep(Duration::from_millis(60));
        tx.send(Cmd::Remote(text("from phone"))).unwrap();
        std::thread::sleep(Duration::from_millis(80));
        tx.send(Cmd::Stop).unwrap();
        t.join().unwrap();
        assert_eq!(*clip.lock().unwrap(), Some(text("from phone")));
        assert_eq!(*sent.lock().unwrap(), vec![text("copied here").to_message()]);
    }

    /// Live: talks to the real Wayland clipboard. Run with
    /// `cargo test --lib live_clipboard -- --ignored --nocapture`.
    #[test]
    #[ignore]
    #[cfg(target_os = "linux")]
    fn live_clipboard_roundtrip() {
        let mut b = WaylandClipboard::new();
        let marker = format!("displayswarm-live-{}", std::process::id());
        b.write(&text(&marker)).expect("write");
        let mut got = None;
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(100));
            got = b.read();
            if got.as_ref() == Some(&text(&marker)) {
                break;
            }
        }
        println!("read back: {got:?}");
        assert_eq!(got, Some(text(&marker)));
    }
}
