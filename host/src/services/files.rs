//! File transfer, both ways, on the file channel.
//!
//! # Protocol
//!
//! * The sender offers a file: `FileOffer { id, size, name, mime }`.
//! * The receiver answers `FileControl { id, ACCEPT }` or `{ id, REJECT }`.
//! * The sender sends the bytes in order as `FileChunk { id, offset, data }`
//!   (at most [`CHUNK`] bytes each), then `FileControl { id, COMPLETE }`.
//! * The receiver sends `FileControl { id, ACK }` after every
//!   [`FILE_ACK_EVERY`] chunks it has written, so a sender with a small send
//!   queue (the phone's) can keep a bounded number of chunks in flight.
//! * Either side may send `FileControl { id, CANCEL }` at any time; a session
//!   that ends cancels everything.
//!
//! Ids are chosen by the sender. Files the host sends have [`HOST_ID_BIT`] set,
//! files the phone sends do not, so the two directions never collide.
//!
//! Chunks go through the session's bulk queue, which is sent after video one
//! fragment at a time: a large file never stalls the picture.
//!
//! # Receiving on the host
//!
//! Files from the phone (which is paired, or on a USB cable) are saved without
//! asking into `Downloads/DisplaySwarm`. The name is reduced to a plain file
//! name ([`sanitize_name`]), an existing file is never overwritten
//! ([`unique_path`]), and the bytes go to a hidden `.part` file that is renamed
//! only when the offered size has arrived exactly.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};

use super::{ServiceCtx, ServiceOut, SessionService};
use crate::protocol::wire::{FileOffer, Message, FILE_ACCEPT, FILE_ACK, FILE_ACK_EVERY, FILE_CANCEL, FILE_COMPLETE, FILE_REJECT};

/// Largest chunk sent.
pub const CHUNK: usize = 128 * 1024;
/// Largest file accepted from a phone.
pub const MAX_FILE: u64 = 8 * 1024 * 1024 * 1024;
/// Set in the ids of files the host sends.
pub const HOST_ID_BIT: u32 = 0x8000_0000;
/// Longest file name kept, in characters.
const MAX_NAME: usize = 180;

/// What happened to a transfer, for notifications.
#[derive(Debug, Clone, PartialEq)]
pub enum FileEvent {
    Received { device_id: String, name: String, path: PathBuf },
    Sent { device_id: String, name: String },
    Failed { device_id: String, name: String, reason: String },
}

type Listener = dyn Fn(FileEvent) + Send + Sync;

fn listener() -> &'static Mutex<Option<Arc<Listener>>> {
    static L: OnceLock<Mutex<Option<Arc<Listener>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(None))
}

/// Registers the daemon's handler for transfer events (any thread; keep it cheap).
pub fn set_listener(f: impl Fn(FileEvent) + Send + Sync + 'static) {
    *listener().lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
}

fn emit(ev: FileEvent) {
    match &ev {
        FileEvent::Received { name, path, .. } => log::info!("files: received {name:?} -> {}", path.display()),
        FileEvent::Sent { name, .. } => log::info!("files: sent {name:?}"),
        FileEvent::Failed { name, reason, .. } => log::warn!("files: {name:?} failed: {reason}"),
    }
    let l = listener().lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(l) = l {
        l(ev);
    }
}

/// A file name that is safe to create in a directory: no path, no control
/// characters, no characters Windows forbids, not hidden, not a reserved
/// Windows device name, and not too long. Never empty.
pub fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = base
        .chars()
        .map(|c| if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') { '_' } else { c })
        .collect();
    s = s.trim().trim_start_matches('.').trim_end_matches(['.', ' ']).to_string();
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    if s.chars().count() > MAX_NAME {
        // Keep the extension when shortening.
        let (stem, ext) = match s.rfind('.') {
            Some(i) if s.len() - i <= 16 => (s[..i].to_string(), s[i..].to_string()),
            _ => (s.clone(), String::new()),
        };
        s = stem.chars().take(MAX_NAME - ext.chars().count()).collect::<String>() + &ext;
    }
    if s.is_empty() {
        "file".into()
    } else {
        s
    }
}

/// `dir/name`, or `dir/name (2).ext`, ... if that exists.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..10_000).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !p.exists()).unwrap_or(first)
}

/// The user's Downloads folder (XDG on Linux, the profile's on Windows).
pub fn downloads_dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default();
    if !cfg!(windows) {
        let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config"));
        if let Ok(text) = std::fs::read_to_string(config.join("user-dirs.dirs")) {
            if let Some(p) = parse_user_dirs(&text, &home) {
                return p;
            }
        }
    }
    home.join("Downloads")
}

/// `XDG_DOWNLOAD_DIR="$HOME/..."` from `user-dirs.dirs`.
pub fn parse_user_dirs(text: &str, home: &Path) -> Option<PathBuf> {
    let line = text.lines().map(str::trim).find(|l| l.starts_with("XDG_DOWNLOAD_DIR="))?;
    let value = line.split_once('=')?.1.trim().trim_matches('"');
    let path = match value.strip_prefix("$HOME") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(value),
    };
    path.is_absolute().then_some(path)
}

/// Where files from phones are saved.
pub fn inbox() -> PathBuf {
    downloads_dir().join("DisplaySwarm")
}

/// A MIME type for a file name, by extension; the phone uses it to pick an app.
pub fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "log" | "md" => "text/plain",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" | "opus" => "audio/ogg",
        "zip" => "application/zip",
        "apk" => "application/vnd.android.package-archive",
        "html" | "htm" => "text/html",
        "csv" => "text/csv",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

struct Incoming {
    name: String,
    size: u64,
    received: u64,
    chunks: u32,
    file: File,
    part: PathBuf,
    dir: PathBuf,
}

struct Outgoing {
    name: String,
    path: PathBuf,
    cancel: Arc<AtomicBool>,
}

enum Job {
    Phone(Message),
    Send(PathBuf),
    Stop,
}

/// The per-session file service.
pub struct FileService {
    tx: mpsc::Sender<Job>,
}

impl FileService {
    pub fn start(ctx: &ServiceCtx) -> Self {
        Self::with_inbox(ctx, inbox())
    }

    /// With files from the phone saved into `dir` (tests).
    pub fn with_inbox(ctx: &ServiceCtx, dir: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let worker = Worker {
            out: ctx.out.clone(),
            device_id: ctx.device_id.clone(),
            dir,
            incoming: HashMap::new(),
            outgoing: Arc::new(Mutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU32::new(1)),
        };
        if let Err(e) = std::thread::Builder::new().name("file-transfer".into()).spawn(move || worker.run(rx)) {
            log::warn!("File transfer worker did not start: {e}");
        }
        Self { tx }
    }
}

impl SessionService for FileService {
    fn name(&self) -> &'static str {
        "files"
    }
    fn wants(&self, msg: &Message) -> bool {
        matches!(msg, Message::FileOffer(_) | Message::FileChunk { .. } | Message::FileControl { .. })
    }
    fn on_message(&mut self, msg: Message) {
        let _ = self.tx.send(Job::Phone(msg));
    }
    fn send_file(&mut self, path: &Path) -> bool {
        self.tx.send(Job::Send(path.to_path_buf())).is_ok()
    }
}

impl Drop for FileService {
    fn drop(&mut self) {
        let _ = self.tx.send(Job::Stop);
    }
}

struct Worker {
    out: ServiceOut,
    device_id: String,
    dir: PathBuf,
    incoming: HashMap<u32, Incoming>,
    outgoing: Arc<Mutex<HashMap<u32, Outgoing>>>,
    next_id: Arc<AtomicU32>,
}

impl Worker {
    fn control(&self, id: u32, op: u8) {
        let _ = self.out.send(Message::FileControl { id, op });
    }

    fn fail_incoming(&mut self, id: u32, reason: &str, tell_phone: bool) {
        if let Some(inc) = self.incoming.remove(&id) {
            drop(inc.file);
            let _ = std::fs::remove_file(&inc.part);
            emit(FileEvent::Failed { device_id: self.device_id.clone(), name: inc.name, reason: reason.into() });
        }
        if tell_phone {
            self.control(id, FILE_CANCEL);
        }
    }

    fn run(mut self, rx: mpsc::Receiver<Job>) {
        while let Ok(job) = rx.recv() {
            match job {
                Job::Phone(Message::FileOffer(offer)) => self.offer(offer),
                Job::Phone(Message::FileChunk { id, offset, data }) => self.chunk(id, offset, &data),
                Job::Phone(Message::FileControl { id, op }) => self.on_control(id, op),
                Job::Phone(_) => {}
                Job::Send(path) => self.send(path),
                Job::Stop => break,
            }
        }
        // The session ended: drop partial files, stop senders.
        for id in self.incoming.keys().copied().collect::<Vec<_>>() {
            self.fail_incoming(id, "the device disconnected", false);
        }
        for (_, o) in self.outgoing.lock().unwrap_or_else(|e| e.into_inner()).drain() {
            o.cancel.store(true, Ordering::Relaxed);
        }
    }

    fn offer(&mut self, offer: FileOffer) {
        let name = sanitize_name(&offer.name);
        if offer.id & HOST_ID_BIT != 0 || self.incoming.contains_key(&offer.id) {
            log::warn!("files: refusing offer {:#x} (bad or duplicate id)", offer.id);
            return self.control(offer.id, FILE_REJECT);
        }
        if offer.size > MAX_FILE {
            emit(FileEvent::Failed { device_id: self.device_id.clone(), name, reason: "too large".into() });
            return self.control(offer.id, FILE_REJECT);
        }
        let opened = std::fs::create_dir_all(&self.dir).and_then(|_| {
            let part = unique_path(&self.dir, &format!(".{name}.part"));
            std::fs::OpenOptions::new().write(true).create_new(true).open(&part).map(|f| (f, part))
        });
        match opened {
            Ok((file, part)) => {
                log::info!("files: receiving {name:?} ({} bytes) from {}", offer.size, self.device_id);
                self.incoming.insert(offer.id, Incoming { name, size: offer.size, received: 0, chunks: 0, file, part, dir: self.dir.clone() });
                self.control(offer.id, FILE_ACCEPT);
            }
            Err(e) => {
                emit(FileEvent::Failed { device_id: self.device_id.clone(), name, reason: format!("cannot save: {e}") });
                self.control(offer.id, FILE_REJECT);
            }
        }
    }

    fn chunk(&mut self, id: u32, offset: u64, data: &[u8]) {
        let Some(inc) = self.incoming.get_mut(&id) else { return };
        if offset != inc.received || inc.received + data.len() as u64 > inc.size {
            return self.fail_incoming(id, "the data arrived out of order or too long", true);
        }
        if let Err(e) = inc.file.write_all(data) {
            return self.fail_incoming(id, &format!("cannot write: {e}"), true);
        }
        inc.received += data.len() as u64;
        inc.chunks += 1;
        if inc.chunks % FILE_ACK_EVERY == 0 {
            self.control(id, FILE_ACK);
        }
    }

    fn on_control(&mut self, id: u32, op: u8) {
        if id & HOST_ID_BIT != 0 {
            // About a file we send.
            let mut out = self.outgoing.lock().unwrap_or_else(|e| e.into_inner());
            match op {
                FILE_ACCEPT => {
                    if let Some(o) = out.get(&id) {
                        let (path, cancel, name) = (o.path.clone(), o.cancel.clone(), o.name.clone());
                        let (sink, outgoing, device_id) = (self.out.clone(), self.outgoing.clone(), self.device_id.clone());
                        let spawned = std::thread::Builder::new()
                            .name("file-send".into())
                            .spawn(move || stream_file(id, &path, &name, &device_id, &sink, &cancel, &outgoing));
                        if spawned.is_err() {
                            out.remove(&id);
                        }
                    }
                }
                FILE_REJECT | FILE_CANCEL => {
                    if let Some(o) = out.remove(&id) {
                        o.cancel.store(true, Ordering::Relaxed);
                        let reason = if op == FILE_REJECT { "the phone declined it" } else { "cancelled on the phone" };
                        emit(FileEvent::Failed { device_id: self.device_id.clone(), name: o.name, reason: reason.into() });
                    }
                }
                _ => {}
            }
            return;
        }
        match op {
            FILE_COMPLETE => {
                let Some(inc) = self.incoming.remove(&id) else { return };
                if inc.received != inc.size {
                    self.incoming.insert(id, inc);
                    return self.fail_incoming(id, "the transfer ended early", true);
                }
                let Incoming { name, file, part, dir, .. } = inc;
                let flushed = file.sync_all();
                drop(file);
                let target = unique_path(&dir, &name);
                match flushed.and_then(|_| std::fs::rename(&part, &target)) {
                    Ok(()) => emit(FileEvent::Received { device_id: self.device_id.clone(), name, path: target }),
                    Err(e) => {
                        let _ = std::fs::remove_file(&part);
                        emit(FileEvent::Failed { device_id: self.device_id.clone(), name, reason: e.to_string() });
                    }
                }
            }
            FILE_CANCEL => self.fail_incoming(id, "cancelled on the phone", false),
            _ => {}
        }
    }

    fn send(&mut self, path: PathBuf) {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let size = match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => m.len(),
            Ok(_) => return emit(FileEvent::Failed { device_id: self.device_id.clone(), name, reason: "not a file".into() }),
            Err(e) => return emit(FileEvent::Failed { device_id: self.device_id.clone(), name, reason: e.to_string() }),
        };
        let id = HOST_ID_BIT | (self.next_id.fetch_add(1, Ordering::Relaxed) & !HOST_ID_BIT);
        self.outgoing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, Outgoing { name: name.clone(), path, cancel: Arc::new(AtomicBool::new(false)) });
        log::info!("files: offering {name:?} ({size} bytes) to {}", self.device_id);
        let _ = self.out.send(Message::FileOffer(FileOffer { id, size, mime: mime_for(&name).into(), name }));
    }
}

/// Sends `path` as file `id` once the phone accepted it.
fn stream_file(
    id: u32,
    path: &Path,
    name: &str,
    device_id: &str,
    out: &ServiceOut,
    cancel: &AtomicBool,
    outgoing: &Mutex<HashMap<u32, Outgoing>>,
) {
    let result = (|| -> Result<(), String> {
        let mut f = File::open(path).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; CHUNK];
        let mut offset = 0u64;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".into());
            }
            let n = f.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if !out.blocking_send_bulk(Message::FileChunk { id, offset, data: buf[..n].to_vec() }) {
                return Err("the device disconnected".into());
            }
            offset += n as u64;
        }
        if !out.blocking_send_bulk(Message::FileControl { id, op: FILE_COMPLETE }) {
            return Err("the device disconnected".into());
        }
        Ok(())
    })();
    outgoing.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    match result {
        Ok(()) => emit(FileEvent::Sent { device_id: device_id.into(), name: name.into() }),
        Err(reason) => {
            if reason != "cancelled" {
                let _ = out.send(Message::FileControl { id, op: FILE_CANCEL });
            }
            emit(FileEvent::Failed { device_id: device_id.into(), name: name.into(), reason });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::devices::{DeviceSettings, Role};
    use crate::services::QualityHandle;
    use std::time::Duration;
    use tokio::sync::mpsc as tmpsc;

    #[test]
    fn names_are_reduced_to_a_safe_file_name() {
        assert_eq!(sanitize_name("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name(r"C:\Windows\system32\x.dll"), "x.dll");
        assert_eq!(sanitize_name(".bashrc"), "bashrc", "not hidden");
        assert_eq!(sanitize_name("a\u{0}b\nc"), "a_b_c");
        assert_eq!(sanitize_name("what?.txt"), "what_.txt");
        assert_eq!(sanitize_name("CON.txt"), "_CON.txt");
        assert_eq!(sanitize_name("com1"), "_com1");
        assert_eq!(sanitize_name("trailing. . "), "trailing");
        assert_eq!(sanitize_name(""), "file");
        assert_eq!(sanitize_name("/"), "file");
        let long = format!("{}.pdf", "x".repeat(400));
        let s = sanitize_name(&long);
        assert!(s.ends_with(".pdf") && s.chars().count() == MAX_NAME, "{}", s.len());
    }

    #[test]
    fn existing_files_are_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("ds-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a.txt"));
        std::fs::write(dir.join("a.txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (2).txt"));
        std::fs::write(dir.join("a (2).txt"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.txt"), dir.join("a (3).txt"));
        std::fs::write(dir.join("noext"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "noext"), dir.join("noext (2)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_download_folder_comes_from_user_dirs() {
        let home = Path::new("/home/amy");
        assert_eq!(parse_user_dirs("XDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n", home), Some(home.join("Téléchargements")));
        assert_eq!(parse_user_dirs("XDG_DOWNLOAD_DIR=\"/data/dl\"", home), Some(PathBuf::from("/data/dl")));
        assert_eq!(parse_user_dirs("XDG_DESKTOP_DIR=\"$HOME/Desktop\"", home), None);
        assert_eq!(mime_for("Report.PDF"), "application/pdf");
        assert_eq!(mime_for("noext"), "application/octet-stream");
    }

    struct Session {
        svc: FileService,
        ctrl: tmpsc::UnboundedReceiver<Message>,
        bulk: tmpsc::Receiver<Message>,
        dir: PathBuf,
    }

    fn session(tag: &str) -> Session {
        let (ctrl_tx, ctrl) = tmpsc::unbounded_channel();
        let (bulk_tx, bulk) = tmpsc::channel(4);
        let ctx = ServiceCtx {
            device_id: "phone".into(),
            device_name: "Phone".into(),
            features: crate::protocol::wire::FEATURE_FILES,
            settings: DeviceSettings::default(),
            role: Role::Mirror,
            out: ServiceOut::new(ctrl_tx, bulk_tx),
            quality: QualityHandle::detached(),
        };
        let dir = std::env::temp_dir().join(format!("ds-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Session { svc: FileService::with_inbox(&ctx, dir.clone()), ctrl, bulk, dir }
    }

    fn next_ctrl(s: &mut Session) -> Message {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(m) = s.ctrl.try_recv() {
                return m;
            }
            assert!(std::time::Instant::now() < deadline, "no control message");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(std::time::Instant::now() < deadline, "{} never appeared", path.display());
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_file_from_the_phone_is_saved_whole_and_never_partially() {
        let mut s = session("in");
        let body: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
        s.svc.on_message(Message::FileOffer(FileOffer { id: 7, size: body.len() as u64, name: "../x/report.pdf".into(), mime: "application/pdf".into() }));
        assert_eq!(next_ctrl(&mut s), Message::FileControl { id: 7, op: FILE_ACCEPT });
        for (i, c) in body.chunks(1000).enumerate() {
            s.svc.on_message(Message::FileChunk { id: 7, offset: (i * 1000) as u64, data: c.to_vec() });
        }
        let acks = (0..(300 / FILE_ACK_EVERY)).map(|_| next_ctrl(&mut s)).collect::<Vec<_>>();
        assert!(acks.iter().all(|m| *m == Message::FileControl { id: 7, op: FILE_ACK }), "one ACK per 8 chunks");
        assert!(!s.dir.join("report.pdf").exists(), "not before COMPLETE");
        s.svc.on_message(Message::FileControl { id: 7, op: FILE_COMPLETE });
        wait_for(&s.dir.join("report.pdf"));
        assert_eq!(std::fs::read(s.dir.join("report.pdf")).unwrap(), body);
        assert_eq!(std::fs::read_dir(&s.dir).unwrap().count(), 1, "no .part left behind");
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[test]
    fn out_of_order_or_oversized_data_cancels_and_leaves_nothing() {
        let mut s = session("bad");
        s.svc.on_message(Message::FileOffer(FileOffer { id: 1, size: 10, name: "a.bin".into(), mime: String::new() }));
        assert_eq!(next_ctrl(&mut s), Message::FileControl { id: 1, op: FILE_ACCEPT });
        s.svc.on_message(Message::FileChunk { id: 1, offset: 5, data: vec![0; 5] });
        assert_eq!(next_ctrl(&mut s), Message::FileControl { id: 1, op: FILE_CANCEL });
        s.svc.on_message(Message::FileOffer(FileOffer { id: 2, size: 4, name: "b.bin".into(), mime: String::new() }));
        assert_eq!(next_ctrl(&mut s), Message::FileControl { id: 2, op: FILE_ACCEPT });
        s.svc.on_message(Message::FileChunk { id: 2, offset: 0, data: vec![0; 5] });
        assert_eq!(next_ctrl(&mut s), Message::FileControl { id: 2, op: FILE_CANCEL });
        s.svc.on_message(Message::FileOffer(FileOffer { id: 3, size: MAX_FILE + 1, name: "c".into(), mime: String::new() }));
        assert_eq!(next_ctrl(&mut s), Message::FileControl { id: 3, op: FILE_REJECT });
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(std::fs::read_dir(&s.dir).map(|d| d.count()).unwrap_or(0), 0);
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[test]
    fn a_file_sent_to_the_phone_is_offered_then_streamed_after_accept() {
        let mut s = session("out");
        let src = s.dir.with_extension("src.txt");
        let body: Vec<u8> = (0..(CHUNK * 2 + 10)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &body).unwrap();
        assert!(s.svc.send_file(&src));
        let Message::FileOffer(offer) = next_ctrl(&mut s) else { panic!("expected an offer") };
        assert!(offer.id & HOST_ID_BIT != 0);
        assert_eq!(offer.size, body.len() as u64);
        assert_eq!(offer.mime, "text/plain");
        s.svc.on_message(Message::FileControl { id: offer.id, op: FILE_ACCEPT });
        let mut got = Vec::new();
        loop {
            match s.bulk.blocking_recv().unwrap() {
                Message::FileChunk { id, offset, data } => {
                    assert_eq!((id, offset), (offer.id, got.len() as u64));
                    got.extend_from_slice(&data);
                }
                Message::FileControl { id, op } => {
                    assert_eq!((id, op), (offer.id, FILE_COMPLETE));
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, body);
        let _ = std::fs::remove_file(&src);
    }
}
