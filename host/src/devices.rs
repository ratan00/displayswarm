//! Per-device identity and remembered role (Phase 3).
//!
//! A device is identified by `Hello::device_id` (a stable per-install id). The
//! host remembers each device's [`Role`] and role options across runs, so a
//! phone that plugs in again comes back in the mode the user last chose.
//!
//! Contract (owned by the session work, 3B): the store's on-disk format and
//! location are its business; everyone else uses only the API below.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::protocol::wire::{
    ROLE_EXTEND, ROLE_INPUT_PAD, ROLE_MIRROR, ROLE_MIRROR_WINDOW, ROLE_PHONE_PRIMARY, ROLE_TABLET,
};

/// What a connected device is used for. The wire values are the `ROLE_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// Mirror a host monitor the user picks, letterboxed.
    Mirror,
    /// A new virtual monitor at the phone's native size.
    Extend,
    /// Stream one application window.
    MirrorWindow,
    /// A virtual monitor that becomes the host's primary display.
    PhonePrimary,
    /// No video; pen and touch map to a chosen monitor.
    Tablet,
    /// No video; touchpad, keyboard and shortcut pad.
    InputPad,
}

impl Role {
    pub const ALL: [Role; 6] = [
        Role::Mirror,
        Role::Extend,
        Role::MirrorWindow,
        Role::PhonePrimary,
        Role::Tablet,
        Role::InputPad,
    ];

    /// `None` for an unknown value (including `ROLE_UNSET`).
    pub fn from_wire(v: u8) -> Option<Role> {
        Some(match v {
            ROLE_MIRROR => Role::Mirror,
            ROLE_EXTEND => Role::Extend,
            ROLE_MIRROR_WINDOW => Role::MirrorWindow,
            ROLE_PHONE_PRIMARY => Role::PhonePrimary,
            ROLE_TABLET => Role::Tablet,
            ROLE_INPUT_PAD => Role::InputPad,
            _ => return None,
        })
    }

    pub fn to_wire(self) -> u8 {
        match self {
            Role::Mirror => ROLE_MIRROR,
            Role::Extend => ROLE_EXTEND,
            Role::MirrorWindow => ROLE_MIRROR_WINDOW,
            Role::PhonePrimary => ROLE_PHONE_PRIMARY,
            Role::Tablet => ROLE_TABLET,
            Role::InputPad => ROLE_INPUT_PAD,
        }
    }

    /// Whether this role sends video to the phone.
    pub fn streams_video(self) -> bool {
        !matches!(self, Role::Tablet | Role::InputPad)
    }

    pub fn label(self) -> &'static str {
        match self {
            Role::Mirror => "Mirror",
            Role::Extend => "Extend",
            Role::MirrorWindow => "Mirror window",
            Role::PhonePrimary => "Phone as main screen",
            Role::Tablet => "Drawing tablet",
            Role::InputPad => "Input pad",
        }
    }
}

/// What the host remembers about one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRecord {
    /// `Hello::device_id`.
    pub device_id: String,
    /// `Hello::device_name`, refreshed on every connect.
    pub name: String,
    /// `None` until the user has chosen one.
    pub role: Option<Role>,
    /// Mirror / Tablet: the host monitor (connector name, e.g. `eDP-1`) to use;
    /// `None` lets the portal ask.
    pub target_output: Option<String>,
    /// PhonePrimary: turn the laptop panel off while the phone is primary.
    pub panel_off: bool,
    /// Unix seconds of the last connect.
    pub last_seen: u64,
    /// Per-device feature toggles (audio, mic, clipboard, ...).
    pub settings: DeviceSettings,
}

/// Per-device feature toggles, stored with the record. Missing fields in an
/// older `devices.json` take the defaults below.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    /// Play host audio on the phone (Phase 5).
    pub audio_out: bool,
    /// Make the host's default output the phone while connected (Phase 5).
    pub audio_default_sink: bool,
    /// Play whatever the laptop plays on its normal output, on this phone too.
    /// Any number of phones can do this at once; none of them needs to be
    /// picked as an output. Wins over `audio_default_sink`.
    pub audio_mirror: bool,
    /// Keep the virtual speaker and microphone on the laptop for the whole
    /// session, so the phone can switch audio on and off instantly. Off: they
    /// exist only while the phone has the service switched on.
    pub audio_always_ready: bool,
    /// Delay the laptop's own speakers to match the phone, so both sound
    /// together. Needs `audio_mirror`. Off by default: it reroutes the laptop's
    /// output through a delay while the phone plays.
    pub delay_host_audio: bool,
    /// Offer the phone's microphone as a host input (Phase 5).
    pub mic: bool,
    /// Sync the clipboard both ways (Phase 8).
    pub clipboard: bool,
    /// Lower fps/bitrate when the phone's battery is low or it runs hot (Phase 8).
    pub battery_saver: bool,
    /// Picture quality versus bandwidth.
    pub video_quality: VideoQuality,
    /// Frame rate cap; 0 streams at the host's target rate.
    pub max_fps: u32,
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self {
            audio_out: true,
            audio_default_sink: false,
            audio_mirror: false,
            audio_always_ready: true,
            delay_host_audio: false,
            mic: false,
            clipboard: true,
            battery_saver: true,
            video_quality: VideoQuality::Auto,
            max_fps: 0,
        }
    }
}

/// How the video is rate-controlled.
///
/// Constant quality holds every frame at one QP: a static desktop costs almost
/// nothing and the picture never degrades, but motion takes what it needs.
/// A bitrate target fits a link of known capacity, adapting to it, at the cost
/// of blurring whatever does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoQuality {
    /// An adaptive bitrate up to the configured target, on any link.
    #[default]
    Auto,
    /// Constant quality, near the limit of 4:2:0 H.264, on any link.
    Max,
    /// Constant quality at a coarser QP: in between Auto and Maximum.
    Balanced,
    /// An adaptive bitrate capped at a quarter of the target, on any link: low quality.
    DataSaver,
}

/// QP for [`VideoQuality::Balanced`].
pub const BALANCED_QP: u32 = 22;

impl VideoQuality {
    /// The wire value of `SetQuality`.
    pub fn to_wire(self) -> u8 {
        match self {
            VideoQuality::Auto => 0,
            VideoQuality::Max => 1,
            VideoQuality::Balanced => 2,
            VideoQuality::DataSaver => 3,
        }
    }

    pub fn from_wire(v: u8) -> Option<Self> {
        Some(match v {
            0 => VideoQuality::Auto,
            1 => VideoQuality::Max,
            2 => VideoQuality::Balanced,
            3 => VideoQuality::DataSaver,
            _ => return None,
        })
    }

    /// How the encoder is driven in this mode. The same on USB and Wi-Fi.
    /// `target_kbps` is the configured bitrate; `wired_qp` the "maximum" QP
    /// (see `encoder::wired_qp`).
    pub fn plan(self, target_kbps: u32, wired_qp: u32) -> EncodePlan {
        match self {
            // The bitrate follows the link, up to the configured target.
            VideoQuality::Auto => EncodePlan { qp: None, adaptive: true, kbps: target_kbps },
            // Constant quality, the sharpest this encoder does.
            VideoQuality::Max => EncodePlan { qp: Some(wired_qp), adaptive: false, kbps: target_kbps },
            // Constant quality, coarser: in between.
            VideoQuality::Balanced => EncodePlan { qp: Some(BALANCED_QP.max(wired_qp)), adaptive: false, kbps: target_kbps },
            // The bitrate follows the link but never rises above a quarter of the target.
            VideoQuality::DataSaver => EncodePlan { qp: None, adaptive: true, kbps: (target_kbps / 4).max(DATA_SAVER_MIN_KBPS) },
        }
    }
}

/// Floor of the Data saver ceiling (kbps), matching the adaptive controller's minimum.
pub const DATA_SAVER_MIN_KBPS: u32 = 2_500;

/// The encoder settings a [`VideoQuality`] stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodePlan {
    /// Constant-quality QP, or `None` to encode at `kbps`.
    pub qp: Option<u32>,
    /// Let the bitrate controller move `kbps` (it is the ceiling).
    pub adaptive: bool,
    pub kbps: u32,
}

impl DeviceSettings {
    /// Frames per second to stream at, given the host's target.
    pub fn fps(&self, target_fps: u32) -> u32 {
        match self.max_fps {
            0 => target_fps,
            cap => cap.min(target_fps).max(1),
        }
    }
}

impl DeviceRecord {
    pub fn new(device_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            device_id: device_id.into(),
            name: name.into(),
            role: None,
            target_output: None,
            panel_off: false,
            last_seen: 0,
            settings: DeviceSettings::default(),
        }
    }
}

/// Persistent device records. Cheap to clone the records out; writes save at once.
///
/// # On-disk format
///
/// One JSON file, `$XDG_CONFIG_HOME/displayswarm/devices.json` (fallback
/// `~/.config/displayswarm/devices.json`): `{"version":1,"devices":[{...}]}`.
/// Roles are stored as their wire value (`Role::to_wire`); an unknown value
/// loads as "no role chosen". `serde_json` is already a dependency of the crate.
/// Saves write a temp file next to it and rename over it, so a crash never
/// leaves a half-written file. An unreadable or corrupt file is moved aside to
/// `devices.json.corrupt`, logged, and the store starts empty.
pub struct DeviceStore {
    /// `None` = in memory only (used when the config directory is unusable).
    path: Option<PathBuf>,
    records: Vec<DeviceRecord>,
}

#[derive(Serialize, Deserialize)]
struct StoredFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    devices: Vec<StoredRecord>,
}

#[derive(Serialize, Deserialize)]
struct StoredRecord {
    device_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    role: Option<u8>,
    #[serde(default)]
    target_output: Option<String>,
    #[serde(default)]
    panel_off: bool,
    #[serde(default)]
    last_seen: u64,
    #[serde(default)]
    settings: DeviceSettings,
}

impl From<&DeviceRecord> for StoredRecord {
    fn from(r: &DeviceRecord) -> Self {
        Self {
            device_id: r.device_id.clone(),
            name: r.name.clone(),
            role: r.role.map(Role::to_wire),
            target_output: r.target_output.clone(),
            panel_off: r.panel_off,
            last_seen: r.last_seen,
            settings: r.settings.clone(),
        }
    }
}

impl From<StoredRecord> for DeviceRecord {
    fn from(r: StoredRecord) -> Self {
        Self {
            device_id: r.device_id,
            name: r.name,
            role: r.role.and_then(Role::from_wire),
            target_output: r.target_output,
            panel_off: r.panel_off,
            last_seen: r.last_seen,
            settings: r.settings,
        }
    }
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl DeviceStore {
    /// Where the default store lives: `$XDG_CONFIG_HOME/displayswarm/devices.json`,
    /// falling back to `~/.config`, or `None` when neither is known.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|h| !h.is_empty())
                    .map(|h| PathBuf::from(h).join(".config"))
            })?;
        Some(base.join("displayswarm").join("devices.json"))
    }

    /// The store in the user's config directory (created on first save).
    pub fn open_default() -> std::io::Result<DeviceStore> {
        match Self::default_path() {
            Some(p) => Self::open(&p),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no config directory (neither XDG_CONFIG_HOME nor HOME is set)",
            )),
        }
    }

    /// A store that is never written to disk (fallback, tests).
    pub fn in_memory() -> DeviceStore {
        DeviceStore { path: None, records: Vec::new() }
    }

    /// A store at an explicit path (tests). A missing file is an empty store;
    /// a corrupt one is logged, moved aside and treated as empty.
    pub fn open(path: &Path) -> std::io::Result<DeviceStore> {
        let mut store = DeviceStore { path: Some(path.to_path_buf()), records: Vec::new() };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(store),
            Err(e) => {
                log::warn!("Cannot read the device store {}: {e}; starting empty", path.display());
                return Ok(store);
            }
        };
        match serde_json::from_str::<StoredFile>(&text) {
            Ok(file) => {
                for r in file.devices {
                    if r.device_id.is_empty() || store.records.iter().any(|x| x.device_id == r.device_id) {
                        continue;
                    }
                    store.records.push(r.into());
                }
            }
            Err(e) => {
                let aside = path.with_extension("json.corrupt");
                log::warn!(
                    "The device store {} is corrupt ({e}); moving it to {} and starting empty",
                    path.display(),
                    aside.display()
                );
                let _ = std::fs::rename(path, &aside);
            }
        }
        Ok(store)
    }

    pub fn get(&self, device_id: &str) -> Option<DeviceRecord> {
        self.records.iter().find(|r| r.device_id == device_id).cloned()
    }

    pub fn all(&self) -> Vec<DeviceRecord> {
        self.records.clone()
    }

    /// Inserts or replaces the record and saves.
    pub fn put(&mut self, record: DeviceRecord) -> std::io::Result<()> {
        match self.records.iter_mut().find(|r| r.device_id == record.device_id) {
            Some(slot) => *slot = record,
            None => self.records.push(record),
        }
        self.save()
    }

    pub fn forget(&mut self, device_id: &str) -> std::io::Result<()> {
        let before = self.records.len();
        self.records.retain(|r| r.device_id != device_id);
        if self.records.len() == before {
            return Ok(());
        }
        self.save()
    }

    /// Atomic save: temp file in the same directory, fsync, rename.
    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let file = StoredFile { version: 1, devices: self.records.iter().map(StoredRecord::from).collect() };
        let json = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("displayswarm-devices-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("nested").join("devices.json")
    }

    #[test]
    fn role_wire_round_trip() {
        for r in Role::ALL {
            assert_eq!(Role::from_wire(r.to_wire()), Some(r));
        }
        assert_eq!(Role::from_wire(crate::protocol::wire::ROLE_UNSET), None);
    }

    #[test]
    fn store_round_trip_and_forget() {
        let path = temp_path("roundtrip");
        let mut store = DeviceStore::open(&path).unwrap();
        assert!(store.all().is_empty());

        let mut a = DeviceRecord::new("dev-a", "Galaxy");
        a.role = Some(Role::Extend);
        a.target_output = Some("eDP-1".into());
        a.panel_off = true;
        a.last_seen = 1234;
        store.put(a.clone()).unwrap();
        store.put(DeviceRecord::new("dev-b", "Pixel")).unwrap();

        let reopened = DeviceStore::open(&path).unwrap();
        assert_eq!(reopened.get("dev-a"), Some(a.clone()));
        assert_eq!(reopened.get("dev-b").unwrap().role, None);
        assert_eq!(reopened.all().len(), 2);

        // Replacing keeps one record.
        let mut a2 = a.clone();
        a2.role = Some(Role::Tablet);
        store.put(a2.clone()).unwrap();
        let reopened = DeviceStore::open(&path).unwrap();
        assert_eq!(reopened.all().len(), 2);
        assert_eq!(reopened.get("dev-a").unwrap().role, Some(Role::Tablet));

        store.forget("dev-a").unwrap();
        store.forget("never-seen").unwrap();
        let reopened = DeviceStore::open(&path).unwrap();
        assert_eq!(reopened.get("dev-a"), None);
        assert!(reopened.get("dev-b").is_some());
        assert!(!path.with_extension("json.tmp").exists(), "temp file is renamed away");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn corrupt_file_starts_empty_and_is_kept_aside() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ this is not json").unwrap();
        let mut store = DeviceStore::open(&path).unwrap();
        assert!(store.all().is_empty());
        assert!(path.with_extension("json.corrupt").exists());
        // And it is usable afterwards.
        store.put(DeviceRecord::new("d", "n")).unwrap();
        assert_eq!(DeviceStore::open(&path).unwrap().all().len(), 1);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn unknown_role_value_loads_as_unset() {
        let path = temp_path("unknownrole");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"version":1,"devices":[{"device_id":"x","role":77}]}"#).unwrap();
        let store = DeviceStore::open(&path).unwrap();
        assert_eq!(store.get("x").unwrap().role, None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn in_memory_store_never_touches_disk() {
        let mut s = DeviceStore::in_memory();
        s.put(DeviceRecord::new("a", "b")).unwrap();
        assert_eq!(s.all().len(), 1);
    }
}

#[cfg(test)]
mod video_quality_tests {
    use super::*;

    #[test]
    fn every_mode_means_the_same_on_any_link() {
        let auto = VideoQuality::Auto.plan(20_000, 16);
        assert_eq!((auto.qp, auto.adaptive, auto.kbps), (None, true, 20_000));
        let max = VideoQuality::Max.plan(20_000, 16);
        assert_eq!((max.qp, max.adaptive), (Some(16), false));
        let bal = VideoQuality::Balanced.plan(20_000, 16);
        assert_eq!((bal.qp, bal.adaptive), (Some(BALANCED_QP), false));
        let ds = VideoQuality::DataSaver.plan(20_000, 16);
        assert_eq!((ds.qp, ds.adaptive, ds.kbps), (None, true, 5_000));
        assert_eq!(VideoQuality::DataSaver.plan(4_000, 16).kbps, DATA_SAVER_MIN_KBPS);
    }

    #[test]
    fn fps_cap_only_lowers() {
        let s = DeviceSettings { max_fps: 30, ..DeviceSettings::default() };
        assert_eq!(s.fps(60), 30);
        assert_eq!(DeviceSettings { max_fps: 120, ..s.clone() }.fps(60), 60);
        assert_eq!(DeviceSettings::default().fps(60), 60);
    }

    #[test]
    fn old_settings_json_gets_the_defaults_and_names_are_stable() {
        let s: DeviceSettings = serde_json::from_str(r#"{"audio_out":false}"#).unwrap();
        assert_eq!(s.video_quality, VideoQuality::Auto);
        assert_eq!(s.max_fps, 0);
        let v: DeviceSettings = serde_json::from_str(r#"{"video_quality":"data_saver","max_fps":30}"#).unwrap();
        assert_eq!(v.video_quality, VideoQuality::DataSaver);
    }
}
