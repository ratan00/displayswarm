//! Delays the laptop's own speakers so they sound together with the phone.
//!
//! The phone plays the laptop's audio a little later (capture, network, its
//! jitter buffer and output path). While it plays, the laptop's default output
//! is replaced by a virtual sink whose sound goes on to the real speakers
//! through a `pw-loopback` with a delay. The tap that feeds the phone listens
//! to that virtual sink, so the phone still gets the sound undelayed.
//!
//! The previous default output comes back when the [`HostDelay`] is dropped.

use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use super::sink::{read_default_sink_name, DefaultSinkGuard};

/// `node.name` of the delaying sink.
const NODE_NAME: &str = "displayswarm_delayed_speakers";

/// What the laptop's own output path (PipeWire period, the sound card) already
/// adds, so the loopback only has to add the rest.
const HOST_OUTPUT_US: u32 = 30_000;

/// The delay is applied in steps of this size; restarting the loopback costs a
/// short gap in the laptop's sound, so it only happens for a real change.
const STEP_US: u32 = 10_000;
/// Every restart recreates the delaying sink, which the desktop announces as a new device,
/// so only a clear change is worth it.
const RESTART_THRESHOLD_US: u32 = 50_000;

/// A running delay is changed at most this often.
const MIN_CHANGE_INTERVAL: Duration = Duration::from_secs(60);

/// The middle value of the latency reports seen lately, so one odd report does not move the delay.
pub fn median_us(recent: &[u32]) -> u32 {
    let mut v = recent.to_vec();
    v.sort_unstable();
    v.get(v.len() / 2).copied().unwrap_or(0)
}

/// Longest delay worth applying.
const MAX_DELAY_US: u32 = 500_000;

/// How long to wait for the virtual sink to show up.
const SINK_WAIT: Duration = Duration::from_secs(3);

/// The delay to add on the laptop for a phone that is `phone_latency_us` late.
/// With several phones in sync the shared playout delay is what everyone
/// aligns to, so that wins when it is set.
pub fn host_delay_us(phone_latency_us: u32, sync_delay_us: u32) -> u32 {
    let target = if sync_delay_us > 0 { sync_delay_us } else { phone_latency_us };
    let d = target.saturating_sub(HOST_OUTPUT_US).min(MAX_DELAY_US);
    d / STEP_US * STEP_US
}

/// Whether a running delay of `applied_us` should be replaced by `wanted_us`.
pub fn needs_restart(applied_us: u32, wanted_us: u32) -> bool {
    applied_us.abs_diff(wanted_us) >= RESTART_THRESHOLD_US
}

/// Arguments of `pw-loopback` for a delay of `delay_us` into `real_sink`.
pub fn loopback_args(delay_us: u32, real_sink: &str) -> Vec<String> {
    vec![
        "-n".into(),
        "displayswarm-speaker-delay".into(),
        "-m".into(),
        "[ FL, FR ]".into(),
        "-l".into(),
        "10".into(),
        "-d".into(),
        format!("{:.3}", delay_us as f64 / 1_000_000.0),
        "-i".into(),
        format!(
            "media.class=Audio/Sink node.name={NODE_NAME} node.description=DisplaySwarm-delayed-speakers audio.position=[FL,FR]"
        ),
        "-P".into(),
        real_sink.to_string(),
    ]
}

pub struct HostDelay {
    // Field order is drop order: give the default output back before the
    // delaying sink goes away.
    _default: DefaultSinkGuard,
    child: Child,
    real_sink: String,
    applied_us: u32,
    changed_at: Instant,
}

impl HostDelay {
    /// Routes the laptop's output through a delay of `delay_us`.
    pub fn start(delay_us: u32) -> Result<Self, String> {
        let real_sink = read_default_sink_name().ok_or("cannot tell which output the laptop uses")?;
        if real_sink == NODE_NAME {
            return Err("the laptop's output is already the delaying sink".into());
        }
        let child = spawn(delay_us, &real_sink)?;
        let mut me_child = child;
        if !wait_for_sink(&mut me_child) {
            let _ = me_child.kill();
            let _ = me_child.wait();
            return Err("pw-loopback did not create the delaying sink".into());
        }
        let guard = DefaultSinkGuard::set(NODE_NAME).ok_or_else(|| {
            let _ = me_child.kill();
            let _ = me_child.wait();
            "could not make the delaying sink the default output".to_string()
        })?;
        log::info!("audio: laptop speakers delayed by {} ms (into {real_sink})", delay_us / 1000);
        Ok(Self { _default: guard, child: me_child, real_sink, applied_us: delay_us, changed_at: Instant::now() })
    }

    pub fn applied_us(&self) -> u32 {
        self.applied_us
    }

    /// Changes the delay if it moved enough to matter. The default output stays
    /// pointed at the delaying sink; only the loopback behind it restarts.
    pub fn set_delay(&mut self, delay_us: u32) {
        if !needs_restart(self.applied_us, delay_us) || self.changed_at.elapsed() < MIN_CHANGE_INTERVAL {
            return;
        }
        self.changed_at = Instant::now();
        let _ = self.child.kill();
        let _ = self.child.wait();
        match spawn(delay_us, &self.real_sink) {
            Ok(mut c) => {
                if !wait_for_sink(&mut c) {
                    log::warn!("audio: the delaying sink did not come back");
                }
                self.child = c;
                self.applied_us = delay_us;
                log::info!("audio: laptop speakers now delayed by {} ms", delay_us / 1000);
            }
            Err(e) => {
                // The default output points at a sink that is gone, so PipeWire falls back
                // to the real speakers; keep the old child so Drop has something to reap.
                log::warn!("audio: cannot change the speaker delay: {e}");
            }
        }
    }
}

impl Drop for HostDelay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(delay_us: u32, real_sink: &str) -> Result<Child, String> {
    let mut cmd = Command::new("pw-loopback");
    cmd.args(loopback_args(delay_us, real_sink)).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // If the daemon dies the loopback goes with it.
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| format!("cannot run pw-loopback: {e}"))
}

/// Polls until the delaying sink is registered (or the child died).
fn wait_for_sink(child: &mut Child) -> bool {
    let start = Instant::now();
    while start.elapsed() < SINK_WAIT {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return false;
        }
        if sink_exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn sink_exists() -> bool {
    Command::new("pactl")
        .args(["list", "short", "sinks"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some(NODE_NAME)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_is_the_phone_latency_minus_the_laptop_path_in_steps() {
        assert_eq!(host_delay_us(183_000, 0), 150_000);
        assert_eq!(host_delay_us(20_000, 0), 0, "a phone quicker than the laptop needs no delay");
        assert_eq!(host_delay_us(9_000_000, 0), MAX_DELAY_US);
    }

    #[test]
    fn one_odd_report_does_not_move_the_median() {
        assert_eq!(median_us(&[110_000, 120_000, 250_000, 115_000, 118_000]), 118_000);
        assert_eq!(median_us(&[]), 0);
    }

    #[test]
    fn the_shared_sync_delay_wins_when_set() {
        assert_eq!(host_delay_us(100_000, 210_000), 180_000);
    }

    #[test]
    fn small_changes_do_not_restart_the_loopback() {
        assert!(!needs_restart(150_000, 190_000));
        assert!(needs_restart(150_000, 200_000));
        assert!(needs_restart(180_000, 100_000));
    }

    #[test]
    fn loopback_arguments_carry_the_delay_and_the_real_sink() {
        let a = loopback_args(150_000, "alsa_output.pci");
        let at = |flag: &str| a.iter().position(|x| x == flag).map(|i| a[i + 1].clone());
        assert_eq!(at("-d").as_deref(), Some("0.150"));
        assert_eq!(at("-P").as_deref(), Some("alsa_output.pci"));
        assert!(at("-i").unwrap().contains("media.class=Audio/Sink"));
    }
}
