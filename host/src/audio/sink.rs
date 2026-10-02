//! The PipeWire virtual output ("DisplaySwarm - <device>") and the helper that makes
//! it the default output while a phone is connected.
//!
//! The sink is a stream in `Audio/Sink` mode: applications play into it like
//! into any sound card and the samples arrive in [`VirtualSink`]'s callback. The
//! PipeWire objects live on their own thread (they are not `Send`); dropping
//! the handle stops the thread, which removes the node.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use super::codec::SAMPLE_RATE;
use crate::protocol::host_monotonic_us;

/// Called with interleaved stereo f32 samples and the host-clock time at which
/// they ended. Runs on the PipeWire thread: keep it short.
pub type PcmCallback = Box<dyn FnMut(&[f32], u64) + Send>;

pub struct VirtualSink {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    node_name: String,
}

/// `node.name` for a device: PipeWire node names are plain identifiers.
pub fn node_name(prefix: &str, device_id: &str) -> String {
    let id: String = device_id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    format!("{prefix}_{id}")
}

/// What the PipeWire node is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkMode {
    /// An output devices and apps can pick ("DisplaySwarm - <phone>").
    Output,
    /// A tap on whatever the laptop's default output currently is, so several
    /// phones can each play the same sound without any routing by the user.
    MirrorDefault,
}

impl VirtualSink {
    /// Creates the sink and waits until PipeWire accepted it.
    pub fn create(node_name: &str, description: &str, on_pcm: PcmCallback) -> Result<Self, String> {
        Self::create_mode(node_name, description, SinkMode::Output, on_pcm)
    }

    pub fn create_mode(node_name: &str, description: &str, mode: SinkMode, on_pcm: PcmCallback) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel();
        let (name, desc, stop2) = (node_name.to_string(), description.to_string(), stop.clone());
        let thread = std::thread::Builder::new()
            .name("displayswarm-audio-sink".into())
            .spawn(move || {
                if let Err(e) = run(&name, &desc, mode, on_pcm, &stop2, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { stop, thread: Some(thread), node_name: node_name.to_string() }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::SeqCst);
                let _ = thread.join();
                Err("PipeWire did not answer in time".into())
            }
        }
    }

    pub fn node_name(&self) -> &str {
        &self.node_name
    }
}

impl Drop for VirtualSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(
    name: &str,
    description: &str,
    mode: SinkMode,
    mut on_pcm: PcmCallback,
    stop: &Arc<AtomicBool>,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopBox::new(None).map_err(|e| format!("PipeWire main loop: {e}"))?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None).map_err(|e| format!("PipeWire context: {e}"))?;
    let core = context.connect(None).map_err(|e| format!("cannot reach the PipeWire daemon: {e}"))?;

    let props = match mode {
        SinkMode::Output => pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_CLASS => "Audio/Sink",
            *pw::keys::NODE_NAME => name,
            *pw::keys::NODE_DESCRIPTION => description,
            *pw::keys::APP_NAME => "DisplaySwarm",
            "audio.position" => "FL,FR",
            "node.virtual" => "true",
            "node.latency" => "256/48000",
        },
        // A plain capture stream of the default output's monitor.
        SinkMode::MirrorDefault => pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::NODE_NAME => name,
            *pw::keys::NODE_DESCRIPTION => description,
            *pw::keys::APP_NAME => "DisplaySwarm",
            "stream.capture.sink" => "true",
            "audio.position" => "FL,FR",
            // A quarter of the default 1024-frame period: the tap is the first link of the phone's delay.
            "node.latency" => "256/48000",
        },
    };
    let stream = pw::stream::StreamBox::new(&core, name, props).map_err(|e| format!("PipeWire stream: {e}"))?;

    let stop_cb = stop.clone();
    let ready_cb = ready.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |_s, _u, _old, new| match new {
            pw::stream::StreamState::Error(err) => {
                log::error!("audio sink: {err}");
                let _ = ready_cb.send(Err(err));
                stop_cb.store(true, Ordering::SeqCst);
            }
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                // The node is registered; Paused is its idle state.
                let _ = ready_cb.send(Ok(()));
            }
            _ => {}
        })
        .process(move |stream, _u| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let (offset, size) = {
                let c = data.chunk();
                (c.offset() as usize, c.size() as usize)
            };
            let Some(bytes) = data.data() else { return };
            let end = (offset + size).min(bytes.len());
            if offset >= end {
                return;
            }
            let pcm: Vec<f32> = bytes[offset..end]
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            on_pcm(&pcm, host_monotonic_us());
        })
        .register()
        .map_err(|e| format!("PipeWire listener: {e}"))?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(2);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    let bytes: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .map_err(|e| format!("format pod: {e:?}"))?
    .0
    .into_inner();
    let mut params = [Pod::from_bytes(&bytes).ok_or("format pod")?];

    // No RT_PROCESS: the callback encodes, which is fine on the main loop
    // thread but not on the realtime one.
    let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
    stream
        .connect(spa::utils::Direction::Input, None, flags, &mut params)
        .map_err(|e| format!("PipeWire connect: {e}"))?;

    let lp = mainloop.loop_();
    while !stop.load(Ordering::Relaxed) {
        lp.iterate(pw::loop_::Timeout::Finite(Duration::from_millis(50)));
    }
    Ok(())
}

// --- default output -------------------------------------------------------

const DEFAULT_KEY: &str = "default.configured.audio.sink";

/// Makes a node the configured default output and puts the previous choice
/// back on drop. Uses `pw-metadata` (what `wpctl set-default` writes too).
pub struct DefaultSinkGuard {
    /// JSON value that was configured before, if any.
    previous: Option<String>,
}

impl DefaultSinkGuard {
    pub fn set(node_name: &str) -> Option<Self> {
        let previous = read_configured_default();
        let value = format!("{{\"name\":\"{node_name}\"}}");
        if run_metadata(&["0", DEFAULT_KEY, &value, "Spa:String:JSON"]) {
            log::info!("audio: {node_name} is now the default output");
            Some(Self { previous })
        } else {
            log::warn!("audio: pw-metadata failed; the default output stays as it was");
            None
        }
    }
}

impl Drop for DefaultSinkGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => run_metadata(&["0", DEFAULT_KEY, v, "Spa:String:JSON"]),
            None => run_metadata(&["-d", "0", DEFAULT_KEY]),
        };
    }
}

/// The node name of the output the laptop is playing on right now.
pub fn read_default_sink_name() -> Option<String> {
    let out = Command::new("pw-metadata").args(["-n", "default", "0", "default.audio.sink"]).output().ok()?;
    let v = parse_metadata_value(&String::from_utf8_lossy(&out.stdout), "default.audio.sink")?;
    let start = v.find("\"name\":\"")? + 8;
    let rest = &v[start..];
    Some(rest[..rest.find('"')?].to_string())
}

fn run_metadata(args: &[&str]) -> bool {
    Command::new("pw-metadata")
        .arg("-n")
        .arg("default")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn read_configured_default() -> Option<String> {
    let out = Command::new("pw-metadata").args(["-n", "default", "0", DEFAULT_KEY]).output().ok()?;
    parse_metadata_value(&String::from_utf8_lossy(&out.stdout), DEFAULT_KEY)
}

/// Pulls `value:'...'` out of the `update: id:0 key:'K' value:'V' type:'T'`
/// line `pw-metadata` prints.
pub fn parse_metadata_value(output: &str, key: &str) -> Option<String> {
    let needle = format!("key:'{key}' value:'");
    let line = output.lines().find(|l| l.contains(&needle))?;
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.rfind("' type:")?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_names_are_identifiers() {
        assert_eq!(node_name("displayswarm_sink", "AB:cd-12"), "displayswarm_sink_AB_cd_12");
    }

    #[test]
    fn metadata_output_is_parsed() {
        let out = "Found \"default\" metadata 40\nupdate: id:0 key:'default.configured.audio.sink' value:'{\"name\":\"alsa_output.pci-0000\"}' type:'Spa:String:JSON'\n";
        assert_eq!(
            parse_metadata_value(out, DEFAULT_KEY).as_deref(),
            Some("{\"name\":\"alsa_output.pci-0000\"}")
        );
        assert_eq!(parse_metadata_value("Found \"default\" metadata 40\n", DEFAULT_KEY), None);
    }
}
