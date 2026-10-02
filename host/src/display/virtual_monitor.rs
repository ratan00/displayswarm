//! Virtual monitors made by a display driver (Windows), behind one interface so
//! the capture side does not care which driver is installed.
//!
//! Providers, in the order they are tried (see [`select`]):
//!
//! 1. [`ProviderKind::OwnIdd`]: DisplaySwarm's own IddCx driver. **On hold**
//!    : nothing ships it yet, so it always reports
//!    "not installed". The control contract it will speak is fixed here
//!    ([`idd_contract`]) so the host side is ready when the driver exists.
//! 2. [`ProviderKind::Vdd`]: the open-source Virtual Display Driver
//!    (VirtualDrivers/Virtual-Display-Driver, MIT), installed by the user.
//!    See [`super::windows_vdd`].
//! 3. Neither: Mirror only, and Extend is refused with the reason [`select`]
//!    returns.
//!
//! Everything here is plain data and traits, so it builds and is tested on
//! every platform.

use super::model::{Availability, Mode};
use super::{DisplayError, OutputRegion};

/// Where a virtual monitor is right now. Re-read it with
/// [`VirtualMonitor::locate`] after a driver reload: the GDI name and the
/// position can change when the driver re-creates its monitors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorPlace {
    /// GDI device name, `\\.\DISPLAYn`: what capture and `ChangeDisplaySettingsEx` use.
    pub gdi_name: String,
    /// Rectangle in virtual-screen pixels.
    pub region: OutputRegion,
}

/// One monitor a provider added for a session. Dropping it gives the monitor
/// back (the provider decides when the driver actually removes it).
pub trait VirtualMonitor: Send {
    /// Where the monitor is now, or `None` while it is missing (the driver is
    /// reloading, or the user detached it).
    fn locate(&self) -> Option<MonitorPlace>;
    /// For logs: which provider and which of its monitors.
    fn describe(&self) -> String;
}

/// Something that can add a virtual monitor of a given mode.
pub trait VirtualMonitorProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;
    /// Whether [`add`](Self::add) can work now; a refusal says what to do.
    /// Cheap enough for capability detection (no driver reload, no pipe traffic).
    fn availability(&self) -> Availability;
    /// Adds a monitor at `mode` and waits until Windows shows it. Blocks for
    /// seconds (a driver reload): never call it on the async runtime.
    fn add(&self, mode: Mode) -> Result<Box<dyn VirtualMonitor>, DisplayError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OwnIdd,
    Vdd,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::OwnIdd => "DisplaySwarm Virtual Display",
            ProviderKind::Vdd => "Virtual Display Driver",
        }
    }
}

/// The first available provider in `candidates` (given in preference order),
/// or the reason Extend cannot work. The reason is the one from the most
/// useful refusal: a driver that is installed but unusable (wrong version,
/// stopped, no write access) says more than "not installed".
pub fn select(candidates: &[(ProviderKind, Availability)]) -> Result<ProviderKind, String> {
    if let Some((kind, _)) = candidates.iter().find(|(_, a)| a.is_yes()) {
        return Ok(*kind);
    }
    let installed_but_unusable = candidates
        .iter()
        .filter_map(|(_, a)| a.reason())
        .find(|why| !why.starts_with(NOT_INSTALLED_PREFIX));
    Err(installed_but_unusable.map(str::to_string).unwrap_or_else(|| NO_DRIVER_REASON.to_string()))
}

/// Refusals that only mean "this driver is not on the machine" start with this,
/// so [`select`] can prefer a more specific one.
pub const NOT_INSTALLED_PREFIX: &str = "Not installed: ";

/// What the user is told when no virtual-display driver is installed at all.
pub const NO_DRIVER_REASON: &str = "Extending the desktop onto the phone on Windows needs a virtual display driver. \
     Install the free Virtual Display Driver (winget install --id=VirtualDrivers.Virtual-Display-Driver -e, \
     or https://github.com/VirtualDrivers/Virtual-Display-Driver/releases) and reconnect; Mirror works without it.";

/// Our own driver, while it is on hold: never available.
pub struct OwnIddPlaceholder;

impl VirtualMonitorProvider for OwnIddPlaceholder {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OwnIdd
    }
    fn availability(&self) -> Availability {
        Availability::No(format!("{NOT_INSTALLED_PREFIX}DisplaySwarm's own display driver is not built yet."))
    }
    fn add(&self, _mode: Mode) -> Result<Box<dyn VirtualMonitor>, DisplayError> {
        Err(DisplayError::Unsupported { what: "adding a monitor through DisplaySwarm's own display driver".into() })
    }
}

/// The control contract between the host and DisplaySwarm's own IddCx driver
/// ("DisplaySwarm Virtual Display"). The driver is on hold; this pins the wire
/// format both sides will share, so a future `displayswarm_idd.h` is written from
/// it (the same numbers, sizes and field order) and not the other way round.
///
/// Transport: `DeviceIoControl` on the device interface
/// [`INTERFACE_GUID`], which the driver registers with
/// `WdfDeviceCreateDeviceInterface` and serves from `EvtIddCxDeviceIoControl`
/// (IddCx routes IOCTLs to that callback). Every request and reply starts with
/// `version: u32, size: u32`; the driver rejects a version it does not know.
pub mod idd_contract {
    /// `{6c1b3c8e-5f1a-4d0b-9a57-3e0d6a8f2b10}`, the device interface of the
    /// DisplaySwarm driver (generated for this project).
    pub const INTERFACE_GUID: (u32, u16, u16, [u8; 8]) =
        (0x6c1b_3c8e, 0x5f1a, 0x4d0b, [0x9a, 0x57, 0x3e, 0x0d, 0x6a, 0x8f, 0x2b, 0x10]);
    pub const PROTOCOL_VERSION: u32 = 1;

    const FILE_DEVICE_VIDEO: u32 = 0x23;
    const METHOD_BUFFERED: u32 = 0;
    const FILE_READ_DATA: u32 = 1;
    const FILE_WRITE_DATA: u32 = 2;

    /// `CTL_CODE(DeviceType, Function, Method, Access)` from `winioctl.h`.
    pub const fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
        (device << 16) | (access << 14) | (function << 2) | method
    }

    /// In: [`AddMonitor`]. Out: [`MonitorReply`]. Plugs a monitor in (`IddCxMonitorCreate` + `IddCxMonitorArrival`).
    pub const IOCTL_ADD_MONITOR: u32 = ctl_code(FILE_DEVICE_VIDEO, 0x900, METHOD_BUFFERED, FILE_READ_DATA | FILE_WRITE_DATA);
    /// In: [`RemoveMonitor`]. Out: nothing. `IddCxMonitorDeparture` for that monitor only.
    pub const IOCTL_REMOVE_MONITOR: u32 =
        ctl_code(FILE_DEVICE_VIDEO, 0x901, METHOD_BUFFERED, FILE_READ_DATA | FILE_WRITE_DATA);
    /// In: header only. Out: [`DriverInfo`].
    pub const IOCTL_GET_INFO: u32 = ctl_code(FILE_DEVICE_VIDEO, 0x902, METHOD_BUFFERED, FILE_READ_DATA);

    /// Plug in a monitor offering exactly this mode (plus a few safe fallbacks).
    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AddMonitor {
        pub version: u32,
        pub size: u32,
        pub width: u32,
        pub height: u32,
        /// Refresh as a fraction, as IddCx wants it (`vSyncFreq`): 60/1, 90/1, 60000/1001.
        pub refresh_num: u32,
        pub refresh_den: u32,
        /// Caller's key (a hash of the phone's device id), so a restarted host
        /// can find and remove the monitors its previous run left behind.
        pub owner_tag: u64,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RemoveMonitor {
        pub version: u32,
        pub size: u32,
        pub monitor_id: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MonitorReply {
        pub version: u32,
        pub size: u32,
        pub monitor_id: u32,
        /// The `ConnectorIndex` given to `IddCxMonitorCreate`; the host finds
        /// the monitor with it through `DisplayConfigGetDeviceInfo`.
        pub connector_index: u32,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct DriverInfo {
        pub version: u32,
        pub size: u32,
        pub driver_version: u32,
        /// `IddCxGetVersion()` as the driver saw it (0x1500 = IddCx 1.5, ...).
        pub iddcx_version: u32,
        pub max_monitors: u32,
        pub live_monitors: u32,
    }

    impl AddMonitor {
        pub fn new(width: u32, height: u32, refresh_mhz: u32, owner_tag: u64) -> Self {
            let (refresh_num, refresh_den) = super::refresh_fraction(refresh_mhz);
            AddMonitor {
                version: PROTOCOL_VERSION,
                size: std::mem::size_of::<Self>() as u32,
                width,
                height,
                refresh_num,
                refresh_den,
                owner_tag,
            }
        }
    }
}

/// A refresh rate in millihertz as the smallest fraction: 60000 -> 60/1,
/// 59940 -> 2997/50, 119880 -> 2997/25. Zero is taken as 60 Hz.
pub fn refresh_fraction(refresh_mhz: u32) -> (u32, u32) {
    let mhz = if refresh_mhz == 0 { 60_000 } else { refresh_mhz };
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 {
            a
        } else {
            gcd(b, a % b)
        }
    }
    let g = gcd(mhz, 1000);
    (mhz / g, 1000 / g)
}

/// A mode for logs and messages: `1600x720@90Hz`, `1920x1080@59.94Hz`.
pub fn mode_label(mode: Mode) -> String {
    format!("{}x{}@{}Hz", mode.width, mode.height, hz_text(mode.refresh_mhz))
}

/// Millihertz as the shortest decimal: 60000 -> "60", 59940 -> "59.94".
pub fn hz_text(refresh_mhz: u32) -> String {
    let mhz = if refresh_mhz == 0 { 60_000 } else { refresh_mhz };
    let whole = mhz / 1000;
    let frac = mhz % 1000;
    if frac == 0 {
        whole.to_string()
    } else {
        let s = format!("{whole}.{frac:03}");
        s.trim_end_matches('0').to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::idd_contract::*;
    use super::*;

    #[test]
    fn the_first_available_provider_wins() {
        let yes = Availability::Yes;
        let not_here = Availability::No(format!("{NOT_INSTALLED_PREFIX}x"));
        assert_eq!(
            select(&[(ProviderKind::OwnIdd, yes.clone()), (ProviderKind::Vdd, yes.clone())]),
            Ok(ProviderKind::OwnIdd)
        );
        assert_eq!(select(&[(ProviderKind::OwnIdd, not_here.clone()), (ProviderKind::Vdd, yes)]), Ok(ProviderKind::Vdd));
    }

    #[test]
    fn with_nothing_usable_the_most_specific_reason_is_given() {
        let not_here = Availability::No(format!("{NOT_INSTALLED_PREFIX}x"));
        let why = select(&[(ProviderKind::OwnIdd, not_here.clone()), (ProviderKind::Vdd, not_here.clone())]).unwrap_err();
        assert_eq!(why, NO_DRIVER_REASON);
        assert!(why.contains("winget") && why.contains("Mirror"));
        let old = Availability::No("Virtual Display Driver 22.1 is older than supported.".into());
        let why = select(&[(ProviderKind::OwnIdd, not_here), (ProviderKind::Vdd, old)]).unwrap_err();
        assert!(why.contains("older"), "{why}");
        assert!(select(&[]).is_err());
    }

    #[test]
    fn the_own_driver_is_on_hold() {
        let p = OwnIddPlaceholder;
        assert!(p.availability().reason().unwrap().starts_with(NOT_INSTALLED_PREFIX));
        assert!(p.add(Mode { width: 1, height: 1, refresh_mhz: 60_000 }).is_err());
    }

    #[test]
    fn refresh_rates_become_fractions_and_text() {
        assert_eq!(refresh_fraction(60_000), (60, 1));
        assert_eq!(refresh_fraction(90_000), (90, 1));
        assert_eq!(refresh_fraction(120_000), (120, 1));
        assert_eq!(refresh_fraction(59_940), (2997, 50));
        assert_eq!(refresh_fraction(0), (60, 1));
        assert_eq!(hz_text(60_000), "60");
        assert_eq!(hz_text(59_940), "59.94");
        assert_eq!(hz_text(119_880), "119.88");
        assert_eq!(hz_text(90_500), "90.5");
        assert_eq!(mode_label(Mode { width: 1600, height: 720, refresh_mhz: 120_000 }), "1600x720@120Hz");
    }

    #[test]
    fn ioctl_codes_follow_ctl_code() {
        // CTL_CODE(FILE_DEVICE_VIDEO, 0x900, METHOD_BUFFERED, FILE_READ_DATA|FILE_WRITE_DATA)
        assert_eq!(IOCTL_ADD_MONITOR, 0x0023_E400);
        assert_eq!(IOCTL_REMOVE_MONITOR, 0x0023_E404);
        assert_eq!(IOCTL_GET_INFO, 0x0023_6408);
        // Function codes >= 0x800 are the vendor range.
        assert!((IOCTL_ADD_MONITOR >> 2) & 0xFFF >= 0x800);
    }

    #[test]
    fn contract_structs_have_fixed_sizes() {
        // These sizes are part of the wire contract with the driver.
        assert_eq!(std::mem::size_of::<AddMonitor>(), 32);
        assert_eq!(std::mem::size_of::<RemoveMonitor>(), 16);
        assert_eq!(std::mem::size_of::<MonitorReply>(), 16);
        assert_eq!(std::mem::size_of::<DriverInfo>(), 24);
        let a = AddMonitor::new(1600, 720, 59_940, 7);
        assert_eq!((a.version, a.size, a.refresh_num, a.refresh_den), (PROTOCOL_VERSION, 32, 2997, 50));
    }
}
