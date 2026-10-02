//! "Phone as main screen" (Role::PhonePrimary): make a monitor the host's
//! primary display, optionally turn the built-in panel off, and put the
//! previous layout back afterwards, even after a crash.
//!
//! Contract: the session calls [`make_primary`] once the virtual monitor's
//! region is known and keeps the guard for the life of the role; dropping the
//! guard restores the saved layout. [`recover_leftovers`] runs once at startup
//! and restores any layout a crashed run left behind.
//!
//! GNOME goes through `org.gnome.Mutter.DisplayConfig` ([`super::mutter`]),
//! KDE Plasma through `kscreen-doctor` ([`super::kscreen`]); see
//! [`super::env::current_desktop`]. wlroots (`wlr-randr`) can be added behind the
//! same entry points.
//!
//! # Crash safety
//!
//! Two independent layers:
//!
//! 1. The layout is applied with `ApplyMonitorsConfig` method **1 (temporary)**:
//!    Mutter never writes it to `monitors.xml`. When the process dies, the
//!    portal session closes and the virtual monitor disappears; Mutter then
//!    re-derives the layout from the stored configuration (or its default),
//!    so the panel comes back by itself. Method 2 (persistent) would instead
//!    make a crash leave "panel off, primary gone" on disk across logins.
//! 2. The previous layout is journaled to `<state dir>/displayswarm/primary-journal`
//!    before anything is changed and removed only after a successful restore,
//!    so [`recover_leftovers`] can put it back if Mutter's fallback was not
//!    enough (for example the crash left the virtual monitor alive).
//!
//! KWin has no temporary apply: it stores every layout, keyed by the set of
//! connected outputs. Restoring after the phone's virtual output is gone fixes
//! the layout without it, but KWin would bring "panel off" back the next time
//! that output appears. So the KDE journal is kept as a *pending* journal
//! ([`recover_for_output`]) and applied when that output shows up again.

use std::fs;
use std::path::{Path, PathBuf};

use super::kscreen::{self, SavedOutput};
use super::env;
use super::model::Desktop;
use super::mutter::{self, DisplayConfig, LogicalConfig, State};
use super::{DisplayError, OutputRegion};

const JOURNAL_HEADER: &str = "displayswarm-primary-journal 1";
const KSCREEN_JOURNAL_HEADER: &str = "displayswarm-primary-journal-kscreen 1";

/// Restores the saved monitor layout when dropped.
pub struct PrimaryGuard {
    journal: PathBuf,
    region: OutputRegion,
}

impl PrimaryGuard {
    /// Where the phone's monitor is now. It differs from the region passed to
    /// [`make_primary`] when the panel was turned off (the layout is shifted so
    /// its origin is 0,0); the session must hand this to
    /// `InputInjector::set_output_region`.
    pub fn region(&self) -> OutputRegion {
        self.region
    }
}

impl Drop for PrimaryGuard {
    fn drop(&mut self) {
        if let Err(e) = restore_journal(&self.journal) {
            log::warn!("Could not restore the monitor layout ({e}); it stays journaled for the next start");
        }
    }
}

/// Makes the logical monitor at `region` primary; with `panel_off`, disables
/// the built-in panel too. The previous layout is journaled before any change.
///
/// Call it after the virtual monitor exists (its region is known from the
/// portal stream). Blocking D-Bus calls, a few milliseconds each.
pub fn make_primary(region: OutputRegion, panel_off: bool) -> Result<PrimaryGuard, DisplayError> {
    let path = journal_path();
    if path.exists() {
        // A previous run died mid-role. Put its layout back first so the new
        // journal records the real "before", not a half-applied layout.
        recover_leftovers();
    }
    if env::current_desktop() == Desktop::Kde {
        return make_primary_kscreen(&path, region, panel_off);
    }
    let cfg = DisplayConfig::connect()?;

    let mut last_err = None;
    for attempt in 0..3 {
        // Re-read every attempt: the serial changes whenever the layout does
        // (the virtual monitor appearing, a hotplug).
        let state = cfg.state()?;
        let (new_cfgs, new_region) = mutter::plan_primary(&state, region, panel_off)
            .map_err(|d| DisplayError::Io { context: "phone as main screen".into(), detail: d })?;
        let virtual_connector = find_virtual_connector(&state, region);
        write_journal(&path, &Journal::from_state(&state, virtual_connector)).map_err(|e| {
            DisplayError::Io { context: "journal the monitor layout".into(), detail: format!("{}: {e}", path.display()) }
        })?;
        match cfg.apply(&state, &new_cfgs, mutter::METHOD_TEMPORARY) {
            Ok(()) => {
                log::info!("Phone made primary at {new_region:?} (panel_off={panel_off})");
                return Ok(PrimaryGuard { journal: path, region: new_region });
            }
            Err(e) => {
                log::warn!("ApplyMonitorsConfig attempt {} failed: {e}", attempt + 1);
                let _ = fs::remove_file(&path); // nothing changed, nothing to restore
                last_err = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        }
    }
    Err(last_err.expect("loop ran"))
}

fn make_primary_kscreen(path: &Path, region: OutputRegion, panel_off: bool) -> Result<PrimaryGuard, DisplayError> {
    let io = |context: &str, detail: String| DisplayError::Io { context: context.into(), detail };
    let state = kscreen::state()?;
    let (args, new_region) =
        kscreen::plan_primary(&state, region, panel_off).map_err(|d| io("phone as main screen", d))?;
    let target = kscreen::find_by_region(&state, region).map(|o| o.name.clone()).unwrap_or_default();
    // An older pending journal describes an older layout; this one supersedes it.
    let _ = fs::remove_file(pending_path(path));
    let journal = KScreenJournal { outputs: kscreen::save(&state) };
    write_text(path, &journal.to_text())
        .map_err(|e| io("journal the monitor layout", format!("{}: {e}", path.display())))?;
    let guard = PrimaryGuard { journal: path.to_path_buf(), region: new_region };
    kscreen::apply(&args)?; // on error the guard's drop puts back whatever changed
    // KWin applies asynchronously; wait until the phone's output is primary.
    for _ in 0..20 {
        if kscreen::state()?.by_name(&target).is_some_and(|o| o.priority == 1) {
            log::info!("Phone made primary at {new_region:?} (panel_off={panel_off})");
            return Ok(guard);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(io("phone as main screen", format!("KWin did not make {target} primary")))
}

/// Restores a layout journaled by a run that did not clean up. Safe to call
/// when there is nothing to restore.
pub fn recover_leftovers() {
    let path = journal_path();
    if !path.exists() {
        return;
    }
    log::warn!("Found a leftover monitor-layout journal at {}, restoring", path.display());
    if let Err(e) = restore_journal(&path) {
        log::warn!("Leftover layout not restored: {e}");
    }
}

/// KDE: the output `name` has just appeared. If a pending journal (see the
/// module docs) mentions it, KWin has just re-applied the layout it stored for
/// this set of outputs, "phone as main screen" included; put the journaled
/// layout back.
pub fn recover_for_output(name: &str) {
    let pending = pending_path(&journal_path());
    let Ok(text) = fs::read_to_string(&pending) else { return };
    let Ok(journal) = KScreenJournal::parse(&text) else {
        let _ = fs::remove_file(&pending);
        return;
    };
    if !journal.outputs.iter().any(|o| o.name == name) {
        return;
    }
    log::warn!("{name} came back with a layout from an interrupted phone-as-main-screen; restoring");
    match restore_kscreen(&journal) {
        Ok(_) => {
            let _ = fs::remove_file(&pending);
        }
        Err(e) => log::warn!("Layout for {name} not restored: {e}"),
    }
}

fn pending_path(journal: &Path) -> PathBuf {
    journal.with_extension("pending")
}

/// Applies a KDE journal to the outputs present now. Returns whether every
/// journaled output was present.
fn restore_kscreen(journal: &KScreenJournal) -> Result<bool, String> {
    let state = kscreen::state().map_err(|e| e.summary())?;
    let all_present = journal.outputs.iter().all(|o| state.by_name(&o.name).is_some());
    if let Some(args) = kscreen::plan_restore(&state, &journal.outputs) {
        kscreen::apply(&args).map_err(|e| e.summary())?;
    }
    Ok(all_present)
}

fn find_virtual_connector(state: &State, region: OutputRegion) -> Option<String> {
    let i = mutter::find_by_region(state, region)?;
    state.logicals[i].monitors.first().map(|(c, _)| c.clone())
}

/// Reads the journal, re-reads the live state (the serial has changed since),
/// applies the saved layout minus vanished monitors, then deletes the journal.
fn restore_journal(path: &Path) -> Result<(), String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    if text.starts_with(KSCREEN_JOURNAL_HEADER) {
        let journal = match KScreenJournal::parse(&text) {
            Ok(j) => j,
            Err(e) => {
                log::warn!("Discarding unreadable journal {}: {e}", path.display());
                let _ = fs::remove_file(path);
                return Ok(());
            }
        };
        let all_present = restore_kscreen(&journal)?;
        if all_present {
            let _ = fs::remove_file(path);
        } else {
            // Keep it for when the missing (virtual) output shows up again.
            let _ = fs::rename(path, pending_path(path));
        }
        log::info!("Monitor layout restored");
        return Ok(());
    }
    let journal = match Journal::parse(&text) {
        Ok(j) => j,
        Err(e) => {
            log::warn!("Discarding unreadable journal {}: {e}", path.display());
            let _ = fs::remove_file(path);
            return Ok(());
        }
    };
    let cfg = DisplayConfig::connect().map_err(|e| e.summary())?;
    let mut last = String::new();
    for _ in 0..3 {
        let state = cfg.state().map_err(|e| e.summary())?;
        let Some(plan) = mutter::plan_restore(&state, &journal.logicals) else {
            // None of the saved monitors exist any more; nothing sensible to apply.
            let _ = fs::remove_file(path);
            return Ok(());
        };
        match cfg.apply(&state, &plan, mutter::METHOD_TEMPORARY) {
            Ok(()) => {
                let _ = fs::remove_file(path);
                log::info!("Monitor layout restored");
                return Ok(());
            }
            Err(e) => {
                last = e.summary();
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
        }
    }
    Err(last)
}

// ---------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------

/// The layout before we touched it. Plain tab-separated lines so a person can
/// read it and no serde format has to stay stable:
///
/// ```text
/// displayswarm-primary-journal 1
/// serial<TAB>7
/// virtual<TAB>Meta-0            (optional)
/// lm<TAB>x<TAB>y<TAB>scale<TAB>transform<TAB>primary(0|1)
/// mon<TAB>connector<TAB>mode    (belongs to the preceding lm)
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Journal {
    pub serial: u32,
    pub virtual_connector: Option<String>,
    pub logicals: Vec<LogicalConfig>,
}

impl Journal {
    pub fn from_state(state: &State, virtual_connector: Option<String>) -> Self {
        Self { serial: state.serial, virtual_connector, logicals: state.logicals.clone() }
    }

    pub fn to_text(&self) -> String {
        let mut s = format!("{JOURNAL_HEADER}\nserial\t{}\n", self.serial);
        if let Some(v) = &self.virtual_connector {
            s.push_str(&format!("virtual\t{v}\n"));
        }
        for l in &self.logicals {
            s.push_str(&format!("lm\t{}\t{}\t{}\t{}\t{}\n", l.x, l.y, l.scale, l.transform, l.primary as u8));
            for (c, m) in &l.monitors {
                s.push_str(&format!("mon\t{c}\t{m}\n"));
            }
        }
        s
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some(JOURNAL_HEADER) {
            return Err("bad header".into());
        }
        let mut j = Journal { serial: 0, virtual_connector: None, logicals: Vec::new() };
        for line in lines.filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split('\t').collect();
            let num = |s: &str| s.parse::<i64>().map_err(|e| format!("{line:?}: {e}"));
            match f.as_slice() {
                ["serial", s] => j.serial = num(s)? as u32,
                ["virtual", v] => j.virtual_connector = Some((*v).into()),
                ["lm", x, y, scale, tr, p] => j.logicals.push(LogicalConfig {
                    x: num(x)? as i32,
                    y: num(y)? as i32,
                    scale: scale.parse().map_err(|e| format!("{line:?}: {e}"))?,
                    transform: num(tr)? as u32,
                    primary: *p == "1",
                    monitors: Vec::new(),
                }),
                ["mon", c, m] => j
                    .logicals
                    .last_mut()
                    .ok_or_else(|| "mon before lm".to_string())?
                    .monitors
                    .push(((*c).into(), (*m).into())),
                _ => return Err(format!("unrecognised line {line:?}")),
            }
        }
        if j.logicals.is_empty() {
            return Err("no logical monitors".into());
        }
        Ok(j)
    }
}

/// The KDE journal: the settings [`kscreen::plan_primary`] changes, per
/// output name. Lines `out<TAB>name<TAB>enabled(0|1)<TAB>x<TAB>y<TAB>priority`.
#[derive(Debug, Clone, PartialEq)]
pub struct KScreenJournal {
    pub outputs: Vec<SavedOutput>,
}

impl KScreenJournal {
    pub fn to_text(&self) -> String {
        let mut s = format!("{KSCREEN_JOURNAL_HEADER}\n");
        for o in &self.outputs {
            s.push_str(&format!("out\t{}\t{}\t{}\t{}\t{}\n", o.name, o.enabled as u8, o.x, o.y, o.priority));
        }
        s
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some(KSCREEN_JOURNAL_HEADER) {
            return Err("bad header".into());
        }
        let mut outputs = Vec::new();
        for line in lines.filter(|l| !l.is_empty()) {
            let num = |s: &str| s.parse::<i64>().map_err(|e| format!("{line:?}: {e}"));
            match line.split('\t').collect::<Vec<_>>().as_slice() {
                ["out", name, en, x, y, p] => outputs.push(SavedOutput {
                    name: (*name).into(),
                    enabled: *en == "1",
                    x: num(x)? as i32,
                    y: num(y)? as i32,
                    priority: num(p)? as u32,
                }),
                _ => return Err(format!("unrecognised line {line:?}")),
            }
        }
        if outputs.is_empty() {
            return Err("no outputs".into());
        }
        Ok(Self { outputs })
    }
}

/// Journal location: `$XDG_STATE_HOME/displayswarm/primary-journal`, else
/// `~/.local/state/displayswarm/primary-journal`.
pub fn journal_path() -> PathBuf {
    journal_path_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn journal_path_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    let base = match xdg.filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(home.unwrap_or_default()).join(".local/state"),
    };
    base.join("displayswarm").join("primary-journal")
}

fn write_journal(path: &Path, journal: &Journal) -> std::io::Result<()> {
    write_text(path, &journal.to_text())
}

/// Atomic write (temp file + rename) so a crash cannot leave half a journal.
fn write_text(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text)?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::super::mutter::fixtures::panel_plus_virtual;
    use super::*;

    #[test]
    fn journal_round_trips() {
        let s = panel_plus_virtual();
        let j = Journal::from_state(&s, Some("Meta-0".into()));
        let back = Journal::parse(&j.to_text()).unwrap();
        assert_eq!(back, j);
    }

    #[test]
    fn fractional_scale_round_trips_exactly() {
        let mut s = panel_plus_virtual();
        s.logicals[0].scale = 1.6666666269302368;
        let j = Journal::from_state(&s, None);
        assert_eq!(Journal::parse(&j.to_text()).unwrap(), j);
    }

    #[test]
    fn corrupt_journals_are_rejected() {
        assert!(Journal::parse("").is_err());
        assert!(Journal::parse("nonsense\n").is_err());
        assert!(Journal::parse(&format!("{JOURNAL_HEADER}\nserial\t1\n")).is_err(), "no monitors");
        assert!(Journal::parse(&format!("{JOURNAL_HEADER}\nmon\ta\tb\n")).is_err());
        assert!(Journal::parse(&format!("{JOURNAL_HEADER}\nlm\tx\t0\t1\t0\t1\n")).is_err());
    }

    #[test]
    fn journal_path_prefers_xdg_state_home() {
        let p = journal_path_from(Some("/x/state".into()), Some("/home/u".into()));
        assert_eq!(p, PathBuf::from("/x/state/displayswarm/primary-journal"));
        let p = journal_path_from(None, Some("/home/u".into()));
        assert_eq!(p, PathBuf::from("/home/u/.local/state/displayswarm/primary-journal"));
        let p = journal_path_from(Some("".into()), Some("/home/u".into()));
        assert_eq!(p, PathBuf::from("/home/u/.local/state/displayswarm/primary-journal"));
    }

    #[test]
    fn kscreen_journal_round_trips() {
        let state = kscreen::parse_state(kscreen::fixtures::THREE_OUTPUTS).unwrap();
        let j = KScreenJournal { outputs: kscreen::save(&state) };
        assert_eq!(KScreenJournal::parse(&j.to_text()).unwrap(), j);
        assert!(j.to_text().contains("out\tVirtual-virtual-xdp-kde-org.kde.konsole\t1\t3840\t0\t3\n"));
        assert!(KScreenJournal::parse(&j.to_text().replace(KSCREEN_JOURNAL_HEADER, JOURNAL_HEADER)).is_err());
        assert!(KScreenJournal::parse(&format!("{KSCREEN_JOURNAL_HEADER}\n")).is_err(), "no outputs");
        assert!(KScreenJournal::parse(&format!("{KSCREEN_JOURNAL_HEADER}\nout\ta\t1\tx\t0\t1\n")).is_err());
    }

    #[test]
    fn write_then_read_from_disk() {
        let dir = std::env::temp_dir().join(format!("displayswarm-journal-test-{}", std::process::id()));
        let path = dir.join("displayswarm").join("primary-journal");
        let j = Journal::from_state(&panel_plus_virtual(), Some("Meta-0".into()));
        write_journal(&path, &j).unwrap();
        assert_eq!(Journal::parse(&fs::read_to_string(&path).unwrap()).unwrap(), j);
        let _ = fs::remove_dir_all(&dir);
    }
}
