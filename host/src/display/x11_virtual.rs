//! The phone's virtual monitor on an X11 session: a spare RandR output lit with
//! a mode of the phone's size.
//!
//! The intel DDX's `VIRTUAL1` output exists for exactly this (a CRTC nobody
//! scans out, whose pixels live in the X framebuffer, so XShm capture reads
//! them like any other monitor's). Whether the modesetting driver, which most
//! current Intel and AMD setups use, or NVIDIA expose one: VERIFY; where none
//! exists Extend is refused with a reason and Mirror stays available.
//!
//! Creating the monitor blocks the caller (a few `xrandr` runs and up to
//! ~500 ms waiting for it to light); the capture factory runs on the session's
//! task, so that session stalls that long when Extend starts. A disconnected physical connector is also used
//! when there is no `VIRTUAL*` output (on by default; `DISPLAYSWARM_X11_ANY_OUTPUT=0` opts out;
//! UNVERIFIED beyond one Intel+NVIDIA machine: whether a driver lights a CRTC for a
//! connector with nothing plugged in varies).
//!
//! Steps (all through `xrandr`): `--newmode` a CVT mode named
//! `displayswarm-<device>-WxH-R`, `--addmode` it to the output, `--output ... --mode
//! ... --pos` right of the other monitors. Afterwards the layout is read again
//! and the output must really be lit at that size, or everything is undone and
//! an error returned. Teardown is the reverse: `--off`, `--delmode`, `--rmmode`.
//!
//! # Crash safety
//!
//! Before anything is created, a journal file is written to
//! `<state dir>/displayswarm/x11-virtual/<mode name>`, naming the output; it is
//! removed after the teardown. [`recover_leftovers`] (run at startup) tears down
//! whatever a crashed run left, and also removes any unused mode carrying the
//! host's prefix, so even a lost journal leaves no mode behind.

use super::randr::{self, RandrOutput, RandrState, MODE_PREFIX};
use super::{DisplayError, OutputRegion};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const JOURNAL_HEADER: &str = "displayswarm-x11-virtual 1";

/// Opt-out for disconnected physical connectors (see the module docs).
pub const ANY_OUTPUT_ENV: &str = "DISPLAYSWARM_X11_ANY_OUTPUT";

fn any_output_allowed() -> bool {
    // On by default: a disconnected connector lights fine on the modesetting
    // driver (checked on Intel+NVIDIA), and the driver's VIRTUAL outputs are
    // still preferred. `DISPLAYSWARM_X11_ANY_OUTPUT=0` turns it off.
    std::env::var(ANY_OUTPUT_ENV).map_or(true, |v| !(v == "0" || v.eq_ignore_ascii_case("false")))
}

/// Outputs the host has lit, and for which device, so the layout watcher can
/// recognise the phone's output by name.
static OWNERS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Whether `output` is the virtual monitor this process created for `device_id`.
pub fn is_owned_by(output: &str, device_id: &str) -> bool {
    OWNERS.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(o, d)| o == output && d == device_id)
}

fn is_driver_virtual(name: &str) -> bool {
    name.to_ascii_uppercase().starts_with("VIRTUAL")
}

/// An output that can become the phone's monitor: dark, not in use by the
/// host already, with a free CRTC that can drive it. The driver's own
/// `VIRTUAL*` outputs always qualify; a disconnected physical connector only
/// with `allow_physical`.
pub fn spare_output(state: &RandrState, allow_physical: bool) -> Option<&RandrOutput> {
    let owned = OWNERS.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(o, _)| o.clone()).collect::<Vec<_>>();
    let usable = |o: &&RandrOutput| {
        o.crtc.is_none()
            && !o.connected
            && !owned.contains(&o.name)
            && (is_driver_virtual(&o.name) || allow_physical)
            && o.possible_crtcs.iter().any(|c| !state.crtc_in_use(*c))
    };
    // The driver's virtual outputs first.
    state.outputs.iter().filter(usable).min_by_key(|o| !is_driver_virtual(&o.name))
}

/// Why no spare output exists here, or `Ok` with the output that would be
/// used. For [`super::env`], which checks for the `xrandr` tool itself.
///
/// An output this process has lit already counts: it is not spare while a
/// session shows it, but Extend clearly works here, and a role check during
/// that session (the phone re-sending its role, a rebuild) must not be refused.
pub fn probe(state: &RandrState) -> Result<String, String> {
    probe_state(state, any_output_allowed()).or_else(|why| {
        OWNERS.lock().unwrap_or_else(|e| e.into_inner()).first().map(|(o, _)| o.clone()).ok_or(why)
    })
}

fn probe_state(state: &RandrState, allow_physical: bool) -> Result<String, String> {
    spare_output(state, allow_physical).map(|o| o.name.clone()).ok_or_else(|| {
        "this X11 session's graphics driver has no spare RandR output (like VIRTUAL1) with a free CRTC for a virtual monitor"
            .into()
    })
}

/// The `xrandr` calls that create the monitor, in order.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatePlan {
    pub output: String,
    pub mode_name: String,
    pub region: OutputRegion,
    pub steps: Vec<Vec<String>>,
}

fn device_tag(device_id: &str) -> String {
    let tag: String = device_id.chars().filter(|c| c.is_ascii_alphanumeric()).take(8).collect();
    if tag.is_empty() { "phone".into() } else { tag.to_ascii_lowercase() }
}

pub fn plan_create(
    state: &RandrState,
    device_id: &str,
    width: u32,
    height: u32,
    refresh_mhz: u32,
    allow_physical: bool,
) -> Result<CreatePlan, String> {
    let output = probe_state(state, allow_physical)?;
    let mode_name = randr::mode_name(&device_tag(device_id), width, height, refresh_mhz);
    let x = state.right_edge(None);
    let (max_w, max_h) = state.max_screen;
    if x as u32 + width > max_w || height > max_h {
        return Err(format!(
            "the X screen can be at most {max_w}x{max_h}; a {width}x{height} monitor right of the others does not fit"
        ));
    }
    let mut steps = Vec::new();
    // A mode of the same name left by an earlier run: take it away first (it
    // may have other timings), unless something shows it.
    if state.mode_named(&mode_name).is_some() {
        if state.mode_in_use(&mode_name) {
            return Err(format!("the mode {mode_name} is in use by another output"));
        }
        steps.extend(delete_mode_steps(state, &mode_name));
    }
    let line = randr::cvt_rb(width, height, refresh_mhz);
    steps.push(randr::newmode_args(&mode_name, &line));
    steps.push(vec!["--addmode".into(), output.clone(), mode_name.clone()]);
    steps.push(vec!["--output".into(), output.clone(), "--mode".into(), mode_name.clone(), "--pos".into(), format!("{x}x0")]);
    Ok(CreatePlan { output, mode_name, region: OutputRegion { x, y: 0, width, height }, steps })
}

/// `--delmode` from every output that offers it, then `--rmmode`.
fn delete_mode_steps(state: &RandrState, mode_name: &str) -> Vec<Vec<String>> {
    let mut steps = Vec::new();
    if let Some(m) = state.mode_named(mode_name) {
        for o in state.outputs.iter().filter(|o| o.modes.contains(&m.id)) {
            steps.push(vec!["--delmode".into(), o.name.clone(), mode_name.into()]);
        }
    }
    steps.push(vec!["--rmmode".into(), mode_name.into()]);
    steps
}

/// The `xrandr` calls that tear the monitor down. Without a state (the X
/// server could not be read) every step is attempted.
pub fn plan_destroy(state: Option<&RandrState>, output: &str, mode_name: &str) -> Vec<Vec<String>> {
    let mut steps = Vec::new();
    let lit = state.map_or(true, |s| s.by_name(output).is_some_and(|o| o.crtc.is_some()));
    if lit {
        steps.push(vec!["--output".into(), output.into(), "--off".into()]);
    }
    match state {
        Some(s) if s.mode_named(mode_name).is_none() => {}
        Some(s) => steps.extend(delete_mode_steps(s, mode_name)),
        None => {
            steps.push(vec!["--delmode".into(), output.into(), mode_name.into()]);
            steps.push(vec!["--rmmode".into(), mode_name.into()]);
        }
    }
    steps
}

/// A lit virtual output; dropping it tears it down.
pub struct VirtualOutput {
    output: String,
    mode_name: String,
    device_id: String,
    journal: PathBuf,
}

impl VirtualOutput {
    /// Creates the phone's monitor. Blocking (a few `xrandr` calls).
    pub fn create(device_id: &str, width: u32, height: u32, refresh_mhz: u32) -> Result<Self, DisplayError> {
        let io = |detail: String| DisplayError::Io { context: "create the X11 virtual monitor".into(), detail };
        if !randr::xrandr_available() {
            return Err(io("the xrandr tool is not installed".into()));
        }
        let state = randr::state()?;
        let plan = plan_create(&state, device_id, width, height, refresh_mhz, any_output_allowed()).map_err(io)?;
        let journal = journal_dir().join(&plan.mode_name);
        write_text(&journal, &format!("{JOURNAL_HEADER}\noutput {}\n", plan.output))
            .map_err(|e| io(format!("journal {}: {e}", journal.display())))?;
        OWNERS.lock().unwrap_or_else(|e| e.into_inner()).push((plan.output.clone(), device_id.to_string()));
        // From here on the guard's drop undoes whatever part was done.
        let guard = VirtualOutput {
            output: plan.output.clone(),
            mode_name: plan.mode_name.clone(),
            device_id: device_id.to_string(),
            journal,
        };
        for step in &plan.steps {
            randr::apply(step)?;
        }
        // The driver may accept the calls and still not light the output.
        let lit = (0..10).find_map(|_| {
            let r = randr::state().ok().and_then(|s| s.by_name(&plan.output)?.region);
            if r.is_none() {
                std::thread::sleep(Duration::from_millis(50));
            }
            r
        });
        match lit {
            Some(r) if r.width == width && r.height == height => {
                log::info!("X11: virtual monitor {} lit at {r:?} (mode {})", plan.output, plan.mode_name);
                Ok(guard)
            }
            other => Err(io(format!("{} did not come up at {width}x{height} (got {other:?})", plan.output))),
        }
    }

    pub fn output(&self) -> &str {
        &self.output
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        let state = randr::state().ok();
        for step in plan_destroy(state.as_ref(), &self.output, &self.mode_name) {
            if let Err(e) = randr::apply(&step) {
                log::warn!("X11: tearing down the virtual monitor: {}", e.summary());
            }
        }
        let _ = fs::remove_file(&self.journal);
        OWNERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(o, d)| !(o == &self.output && d == &self.device_id));
        log::info!("X11: virtual monitor {} removed", self.output);
    }
}

/// Tears down what a crashed run left: every journaled monitor, then every
/// unused mode with the host's prefix. Call once at startup, before any session.
pub fn recover_leftovers() {
    let dir = journal_dir();
    let journaled: Vec<(String, String)> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let mode = e.file_name().to_string_lossy().into_owned();
            let output = parse_journal(&fs::read_to_string(e.path()).ok()?)?;
            Some((mode, output))
        })
        .collect();
    let has_modes = |s: &RandrState| s.modes.iter().any(|m| m.name.starts_with(MODE_PREFIX));
    if journaled.is_empty() && !randr::state().is_ok_and(|s| has_modes(&s)) {
        return;
    }
    if !randr::xrandr_available() {
        log::warn!("X11: leftover virtual monitors found but xrandr is not installed; not cleaned up");
        return;
    }
    for (mode, output) in &journaled {
        log::warn!("X11: removing the virtual monitor {output} ({mode}) a previous run left behind");
        let state = randr::state().ok();
        for step in plan_destroy(state.as_ref(), output, mode) {
            let _ = randr::apply(&step);
        }
        let _ = fs::remove_file(dir.join(mode));
    }
    if let Ok(state) = randr::state() {
        for m in state.modes.iter().filter(|m| m.name.starts_with(MODE_PREFIX) && !state.mode_in_use(&m.name)) {
            log::warn!("X11: removing the leftover mode {}", m.name);
            for step in delete_mode_steps(&state, &m.name) {
                let _ = randr::apply(&step);
            }
        }
    }
}

fn parse_journal(text: &str) -> Option<String> {
    let mut lines = text.lines();
    (lines.next()? == JOURNAL_HEADER).then_some(())?;
    lines.find_map(|l| l.strip_prefix("output ")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn journal_dir() -> PathBuf {
    journal_dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn journal_dir_from(xdg: Option<std::ffi::OsString>, home: Option<std::ffi::OsString>) -> PathBuf {
    let base = match xdg.filter(|s| !s.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(home.unwrap_or_default()).join(".local/state"),
    };
    base.join("displayswarm").join("x11-virtual")
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
    use super::*;
    use crate::display::randr::fixtures::{PANEL_AND_SPARE, WITH_VIRTUAL_AND_MIRROR};
    use crate::display::randr::parse_verbose;

    const DEV: &str = "e41699b8-ee27-4f36-9239-95ffd2f6cd31";

    #[test]
    fn the_drivers_virtual_output_is_the_spare() {
        let s = parse_verbose(PANEL_AND_SPARE).unwrap();
        assert_eq!(probe_state(&s, false).unwrap(), "VIRTUAL1");
        // Without it, a disconnected HDMI port only counts when allowed.
        let mut no_virtual = s.clone();
        no_virtual.outputs.retain(|o| o.name != "VIRTUAL1");
        assert!(probe_state(&no_virtual, false).unwrap_err().contains("spare RandR output"));
        assert_eq!(probe_state(&no_virtual, true).unwrap(), "HDMI-1");
    }

    #[test]
    fn a_lit_or_crtc_less_output_is_not_spare() {
        let s = parse_verbose(WITH_VIRTUAL_AND_MIRROR).unwrap();
        assert!(probe_state(&s, false).is_err(), "VIRTUAL1 is in use");
        let mut busy = parse_verbose(PANEL_AND_SPARE).unwrap();
        // Its only CRTC is taken by another output.
        busy.outputs[1].crtc = Some(3);
        assert!(probe_state(&busy, false).is_err());
    }

    #[test]
    fn creation_plan_places_the_monitor_right_of_the_others() {
        let s = parse_verbose(PANEL_AND_SPARE).unwrap();
        let p = plan_create(&s, DEV, 1600, 720, 60_000, false).unwrap();
        assert_eq!(p.output, "VIRTUAL1");
        assert_eq!(p.mode_name, "displayswarm-e41699b8-1600x720-60");
        assert_eq!(p.region, OutputRegion { x: 1920, y: 0, width: 1600, height: 720 });
        assert_eq!(p.steps.len(), 3);
        assert_eq!(p.steps[0][..2], ["--newmode", "displayswarm-e41699b8-1600x720-60"]);
        assert_eq!(p.steps[1], ["--addmode", "VIRTUAL1", "displayswarm-e41699b8-1600x720-60"]);
        assert_eq!(p.steps[2], ["--output", "VIRTUAL1", "--mode", "displayswarm-e41699b8-1600x720-60", "--pos", "1920x0"]);
    }

    #[test]
    fn a_leftover_mode_of_the_same_name_is_removed_first() {
        let text = WITH_VIRTUAL_AND_MIRROR.replace("displayswarm-e41699b8-1600x720", "displayswarm-e41699b8-1280x800-60");
        let mut s = parse_verbose(&text).unwrap();
        // Dark again, but the mode still exists and is still attached.
        let v = s.outputs.iter_mut().find(|o| o.name == "VIRTUAL1").unwrap();
        v.crtc = None;
        v.region = None;
        v.current = None;
        let p = plan_create(&s, DEV, 1280, 800, 60_000, false).unwrap();
        assert_eq!(p.steps[0], ["--delmode", "VIRTUAL1", "displayswarm-e41699b8-1280x800-60"]);
        assert_eq!(p.steps[1], ["--rmmode", "displayswarm-e41699b8-1280x800-60"]);
        assert_eq!(p.steps[2][0], "--newmode");
    }

    #[test]
    fn a_monitor_beyond_the_maximum_screen_is_refused() {
        let mut s = parse_verbose(PANEL_AND_SPARE).unwrap();
        s.max_screen = (2560, 2560);
        assert!(plan_create(&s, DEV, 1600, 720, 60_000, false).unwrap_err().contains("at most"));
    }

    #[test]
    fn teardown_turns_off_then_deletes_then_removes() {
        let text = WITH_VIRTUAL_AND_MIRROR.replace("displayswarm-e41699b8-1600x720", "displayswarm-e41699b8-1600x720-60");
        let s = parse_verbose(&text).unwrap();
        let steps = plan_destroy(Some(&s), "VIRTUAL1", "displayswarm-e41699b8-1600x720-60");
        assert_eq!(
            steps,
            vec![
                vec!["--output", "VIRTUAL1", "--off"],
                vec!["--delmode", "VIRTUAL1", "displayswarm-e41699b8-1600x720-60"],
                vec!["--rmmode", "displayswarm-e41699b8-1600x720-60"],
            ]
        );
        // Nothing left to remove: nothing to run.
        assert!(plan_destroy(Some(&parse_verbose(PANEL_AND_SPARE).unwrap()), "VIRTUAL1", "displayswarm-x").is_empty());
        // Unreadable server: try everything.
        assert_eq!(plan_destroy(None, "VIRTUAL1", "m").len(), 3);
    }

    #[test]
    fn journal_round_trips_and_rejects_foreign_files() {
        assert_eq!(parse_journal(&format!("{JOURNAL_HEADER}\noutput VIRTUAL1\n")).as_deref(), Some("VIRTUAL1"));
        assert_eq!(parse_journal("something else\noutput VIRTUAL1\n"), None);
        assert_eq!(parse_journal(&format!("{JOURNAL_HEADER}\n")), None);
        assert_eq!(
            journal_dir_from(Some("/x".into()), Some("/home/u".into())),
            PathBuf::from("/x/displayswarm/x11-virtual")
        );
        assert_eq!(journal_dir_from(None, Some("/home/u".into())), PathBuf::from("/home/u/.local/state/displayswarm/x11-virtual"));
    }

    #[test]
    fn ownership_is_by_output_and_device() {
        OWNERS.lock().unwrap().push(("VIRTUAL9".into(), DEV.into()));
        assert!(is_owned_by("VIRTUAL9", DEV));
        assert!(!is_owned_by("VIRTUAL9", "other"));
        assert!(!is_owned_by("VIRTUAL1", DEV));
        OWNERS.lock().unwrap().retain(|(o, _)| o != "VIRTUAL9");
    }
}
