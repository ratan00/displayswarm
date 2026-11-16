//! Daemon logging: stderr (the journal under systemd) plus a ring buffer of
//! recent lines that the UI fetches over IPC. On Windows, where the programs
//! have no console, also a file: `%LOCALAPPDATA%\displayswarm\<program>.log`.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Lines kept.
pub const LOG_CAPACITY: usize = 2000;

/// The most recent log lines, oldest first. Cheap to clone (shared).
#[derive(Clone, Default)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
}

impl LogBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&self, line: String) {
        let mut q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= LOG_CAPACITY {
            q.pop_front();
        }
        q.push_back(line);
    }

    /// The last `max` lines, oldest first.
    pub fn tail(&self, max: usize) -> Vec<String> {
        let q = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        q.iter().skip(q.len().saturating_sub(max)).cloned().collect()
    }
}

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else if !field.name().starts_with("log.") {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}

impl<S: Subscriber> Layer<S> for LogBuffer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // Bridged `log` records carry their real target in the metadata's
        // `log.target` field; the tracing target is just "log".
        let mut v = MessageVisitor(String::new());
        event.record(&mut v);
        let level = match *meta.level() {
            Level::ERROR => "ERROR",
            Level::WARN => "WARN ",
            Level::INFO => "INFO ",
            Level::DEBUG => "DEBUG",
            Level::TRACE => "TRACE",
        };
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.push(format!("{:02}:{:02}:{:02} {level} {}", (secs / 3600) % 24, (secs / 60) % 60, secs % 60, v.0));
    }
}

/// Installs the global subscriber: stderr at `info` (or `RUST_LOG`'s plain
/// level), and everything down to `debug` into the ring buffer. Safe to call
/// once; later calls do nothing.
pub fn init(buffer: &LogBuffer) {
    let stderr_level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| v.parse::<tracing_subscriber::filter::LevelFilter>().ok())
        .unwrap_or(tracing_subscriber::filter::LevelFilter::INFO);
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr).with_filter(stderr_level);
    let ring = buffer.clone().with_filter(tracing_subscriber::filter::LevelFilter::DEBUG);
    let file = log_file().map(|f| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .with_filter(stderr_level)
    });
    let _ = tracing_subscriber::registry().with(stderr).with(ring).with(file).try_init();
}

/// Where this program's log file is (Windows only; elsewhere stderr reaches the journal).
pub fn log_file_path() -> Option<std::path::PathBuf> {
    if !cfg!(windows) {
        return None;
    }
    let base = std::env::var_os("LOCALAPPDATA").filter(|v| !v.is_empty())?;
    let stem = std::env::current_exe().ok()?.file_stem()?.to_string_lossy().into_owned();
    Some(std::path::PathBuf::from(base).join("displayswarm").join(format!("{stem}.log")))
}

/// Opens the log file fresh for this run, keeping the last run's as `.old.log`.
fn log_file() -> Option<std::fs::File> {
    let path = log_file_path()?;
    std::fs::create_dir_all(path.parent()?).ok()?;
    let _ = std::fs::rename(&path, path.with_extension("old.log"));
    std::fs::File::create(&path).ok()
}

/// What to tell a user who needs the daemon's own log.
pub fn where_to_look() -> String {
    if cfg!(windows) {
        let daemon = std::env::var_os("LOCALAPPDATA")
            .map(|b| std::path::PathBuf::from(b).join("displayswarm").join("displayswarm-hostd.log"));
        match daemon {
            Some(p) => format!("Its log is {}.", p.display()),
            None => "Run displayswarm-hostd.exe in a terminal to see why.".into(),
        }
    } else {
        "Run displayswarm-hostd in a terminal to see why.".into()
    }
}

/// Windows: a GUI-subsystem program has no console; when it was started from
/// one (a terminal, `--cli`), write to it again. Does nothing otherwise.
#[cfg(windows)]
pub fn attach_parent_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // Fails harmlessly when there is no parent console (started from Explorer).
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest_lines() {
        let b = LogBuffer::new();
        for i in 0..(LOG_CAPACITY + 10) {
            b.push(format!("line {i}"));
        }
        let all = b.tail(usize::MAX);
        assert_eq!(all.len(), LOG_CAPACITY);
        assert_eq!(all.first().unwrap(), "line 10");
        assert_eq!(b.tail(2), vec![format!("line {}", LOG_CAPACITY + 8), format!("line {}", LOG_CAPACITY + 9)]);
        assert!(b.tail(0).is_empty());
    }
}
