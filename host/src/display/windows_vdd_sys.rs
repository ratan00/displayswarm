//! The Windows side of [`super::windows_vdd`]: SetupAPI for the driver's
//! device, the registry for its settings folder, the named pipe, and the
//! display-configuration APIs for its monitors. Type-checked from Linux only;
//! **none of it has run on Windows yet**.

use super::model::Mode;
use super::windows_vdd::{
    DisplayTarget, DriverInfo, PipeResult, VddSystem, DEFAULT_DIR, HARDWARE_ID, PIPE_NAME, REGISTRY_KEY,
    REGISTRY_PATH_VALUE,
};
use super::OutputRegion;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_Status, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo, SetupDiGetClassDevsW,
    SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW, SetupDiOpenDevRegKey, CM_DEVNODE_STATUS_FLAGS,
    CM_PROB, CR_SUCCESS, DICS_FLAG_GLOBAL, DIGCF_PRESENT, DIREG_DRV, DN_STARTED, GUID_DEVCLASS_DISPLAY, HDEVINFO,
    SPDRP_HARDWAREID, SP_DEVINFO_DATA,
};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig, SetDisplayConfig,
    DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
    DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ALL_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY,
    SDC_SAVE_TO_DATABASE, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, LUID};
use windows::Win32::Graphics::Gdi::{
    ChangeDisplaySettingsExW, CDS_NORESET, CDS_TYPE, CDS_UPDATEREGISTRY, DEVMODEW, DISPLAYCONFIG_PATH_ACTIVE,
    DISPLAYCONFIG_PATH_MODE_IDX_INVALID, DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
    DM_POSITION,
};
use windows::Win32::Foundation::{GetLastError, ERROR_FILE_NOT_FOUND};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::Win32::System::Registry::{RegCloseKey, RegGetValueW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_SZ};

/// How long to wait for the driver's answer. A `SETDISPLAYCOUNT` reload can
/// take a while; the driver's own control app allows 45 s.
const PIPE_ANSWER_TIMEOUT: Duration = Duration::from_secs(45);
const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(4);
/// `ERROR_PIPE_BUSY`: every instance of the pipe is in use; try again.
const ERROR_PIPE_BUSY: i32 = 231;
/// `CREATE_NO_WINDOW`, so `pnputil` does not flash a console.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct WinVdd;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// A `REG_SZ` value under `key\subkey`, or `None`.
fn reg_string(key: HKEY, subkey: Option<&str>, value: &str) -> Option<String> {
    let sub = subkey.map(wide);
    let sub_ptr = sub.as_ref().map_or(PCWSTR::null(), |s| PCWSTR(s.as_ptr()));
    let name = wide(value);
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    let rc = unsafe {
        RegGetValueW(
            key,
            sub_ptr,
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut len),
        )
    };
    (rc == ERROR_SUCCESS).then(|| from_wide(&buf)).filter(|s| !s.trim().is_empty())
}

/// The `Root\MttVDD` display device, if one is present.
fn find_driver() -> Option<DriverInfo> {
    let set: HDEVINFO =
        unsafe { SetupDiGetClassDevsW(Some(&GUID_DEVCLASS_DISPLAY), PCWSTR::null(), HWND::default(), DIGCF_PRESENT) }
            .ok()?;
    let mut found = None;
    let mut index = 0;
    loop {
        let mut data = SP_DEVINFO_DATA { cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
        if unsafe { SetupDiEnumDeviceInfo(set, index, &mut data) }.is_err() {
            break;
        }
        index += 1;
        let mut bytes = [0u8; 1024];
        let mut need = 0u32;
        if unsafe {
            SetupDiGetDeviceRegistryPropertyW(set, &data, SPDRP_HARDWAREID, None, Some(&mut bytes), Some(&mut need))
        }
        .is_err()
        {
            continue;
        }
        // REG_MULTI_SZ: UTF-16 strings separated by NULs.
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let ids = String::from_utf16_lossy(&units);
        if !ids.split('\0').any(|id| id.eq_ignore_ascii_case(HARDWARE_ID)) {
            continue;
        }
        let mut inst = [0u16; 256];
        let instance_id = match unsafe { SetupDiGetDeviceInstanceIdW(set, &data, Some(&mut inst), None) } {
            Ok(()) => from_wide(&inst),
            Err(_) => String::new(),
        };
        let mut status = CM_DEVNODE_STATUS_FLAGS(0);
        let mut problem = CM_PROB(0);
        let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, data.DevInst, 0) };
        let running = cr == CR_SUCCESS && status.0 & DN_STARTED.0 != 0 && problem.0 == 0;
        let (mut version, mut date) = (String::new(), String::new());
        if let Ok(key) = unsafe { SetupDiOpenDevRegKey(set, &data, DICS_FLAG_GLOBAL.0, 0, DIREG_DRV, KEY_READ.0) } {
            version = reg_string(key, None, "DriverVersion").unwrap_or_default();
            date = reg_string(key, None, "DriverDate").unwrap_or_default();
            unsafe {
                let _ = RegCloseKey(key);
            }
        }
        found = Some(DriverInfo { instance_id, version, date, running, problem: problem.0 });
        break;
    }
    unsafe {
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    found
}

/// Both halves of every display path Windows knows (active or not).
fn query_config() -> Option<(Vec<DISPLAYCONFIG_PATH_INFO>, Vec<DISPLAYCONFIG_MODE_INFO>)> {
    for _ in 0..3 {
        let (mut np, mut nm) = (0u32, 0u32);
        if unsafe { GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut np, &mut nm) } != ERROR_SUCCESS {
            return None;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        let rc = unsafe {
            QueryDisplayConfig(QDC_ALL_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
        };
        if rc == ERROR_SUCCESS {
            paths.truncate(np as usize);
            modes.truncate(nm as usize);
            return Some((paths, modes));
        }
        // ERROR_INSUFFICIENT_BUFFER: the configuration changed in between; ask again.
    }
    None
}

fn adapter_path(adapter: LUID) -> String {
    let mut req = DISPLAYCONFIG_ADAPTER_NAME::default();
    req.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
        size: std::mem::size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
        adapterId: adapter,
        id: 0,
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } == 0 {
        from_wide(&req.adapterDevicePath)
    } else {
        String::new()
    }
}

fn source_gdi_name(adapter: LUID, source_id: u32) -> Option<String> {
    let mut req = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
    req.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
        adapterId: adapter,
        id: source_id,
    };
    (unsafe { DisplayConfigGetDeviceInfo(&mut req.header) } == 0).then(|| from_wide(&req.viewGdiDeviceName))
}

/// Whether an adapter device path (`\\?\ROOT#DISPLAY#0000#{...}`) is the
/// device with this instance id (`ROOT\DISPLAY\0000`).
pub fn adapter_is_instance(adapter_path: &str, instance_id: &str) -> bool {
    !instance_id.is_empty() && adapter_path.to_ascii_uppercase().contains(&instance_id.replace('\\', "#").to_ascii_uppercase())
}

fn same_luid(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}

fn set_devmode(gdi_name: &str, dm: &DEVMODEW) -> Result<(), String> {
    let name = wide(gdi_name);
    let rc = unsafe {
        ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(dm), HWND::default(), CDS_UPDATEREGISTRY | CDS_NORESET, None)
    };
    if rc != DISP_CHANGE_SUCCESSFUL {
        return Err(format!("ChangeDisplaySettingsEx({gdi_name}) returned {}", rc.0));
    }
    // Apply everything staged with CDS_NORESET.
    let rc = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, HWND::default(), CDS_TYPE(0), None) };
    if rc != DISP_CHANGE_SUCCESSFUL {
        return Err(format!("applying the display change returned {}", rc.0));
    }
    Ok(())
}

impl VddSystem for WinVdd {
    fn driver(&self) -> Option<DriverInfo> {
        find_driver()
    }

    fn settings_dir(&self) -> PathBuf {
        reg_string(HKEY_LOCAL_MACHINE, Some(REGISTRY_KEY), REGISTRY_PATH_VALUE)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_DIR))
    }

    fn read(&self, path: &Path) -> std::io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn write(&self, path: &Path, text: &str) -> std::io::Result<()> {
        // Next to the target, then renamed over it (MoveFileEx replace).
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".displayswarm-tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }

    fn remove(&self, path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    fn writable(&self, path: &Path) -> bool {
        // The file itself (opened for writing, not truncated) and its folder
        // (for the temporary file and the backup).
        let file_ok = std::fs::OpenOptions::new().write(true).open(path).is_ok();
        let mut probe = path.as_os_str().to_owned();
        probe.push(".displayswarm-probe");
        let dir_ok = std::fs::write(&probe, b"").is_ok();
        let _ = std::fs::remove_file(&probe);
        file_ok && dir_ok
    }

    fn pipe_present(&self) -> bool {
        // Returns at once: true when an instance is free, false with
        // ERROR_SEM_TIMEOUT when the pipe exists but is busy, false with
        // ERROR_FILE_NOT_FOUND when there is no such pipe.
        let name = wide(PIPE_NAME);
        if unsafe { WaitNamedPipeW(PCWSTR(name.as_ptr()), 1) }.as_bool() {
            return true;
        }
        let err = unsafe { GetLastError() };
        err != ERROR_FILE_NOT_FOUND
    }

    fn pipe(&self, command: &str) -> PipeResult {
        let deadline = Instant::now() + PIPE_CONNECT_TIMEOUT;
        let mut pipe = loop {
            match std::fs::OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
                Ok(p) => break p,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return PipeResult::NoPipe,
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return PipeResult::Failed(format!("opening {PIPE_NAME}: {e}")),
            }
        };
        // One UTF-16LE message, no terminator (the driver reads one message).
        let bytes: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();
        if let Err(e) = pipe.write_all(&bytes) {
            return PipeResult::Failed(format!("writing to {PIPE_NAME}: {e}"));
        }
        // The driver answers and disconnects; read until then, on a thread so
        // a hung driver cannot hang the caller.
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::Builder::new().name("displayswarm-vdd-pipe".into()).spawn(move || {
            let mut out = Vec::new();
            let mut buf = [0u8; 512];
            // A read error after the write is the driver disconnecting (or its
            // host restarting on a reload): the command was delivered.
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n]);
            }
            let _ = tx.send(out);
        });
        if reader.is_err() {
            return PipeResult::Sent(String::new());
        }
        match rx.recv_timeout(PIPE_ANSWER_TIMEOUT) {
            Ok(bytes) => PipeResult::Sent(decode_reply(&bytes)),
            Err(_) => {
                log::warn!("The Virtual Display Driver did not finish answering {command:?} within {PIPE_ANSWER_TIMEOUT:?}");
                PipeResult::Sent(String::new())
            }
        }
    }

    fn restart_device(&self, instance_id: &str) -> Result<(), String> {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("pnputil")
            .args(["/restart-device", instance_id])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map_err(|e| format!("pnputil: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "pnputil /restart-device failed ({}; administrator rights are needed): {}",
                out.status,
                String::from_utf8_lossy(&out.stdout).trim()
            ))
        }
    }

    fn targets(&self) -> Vec<DisplayTarget> {
        let instance = find_driver().map(|d| d.instance_id).unwrap_or_default();
        let Some((paths, modes)) = query_config() else { return Vec::new() };
        let mut out: Vec<(LUID, DisplayTarget)> = Vec::new();
        for p in &paths {
            let t = &p.targetInfo;
            let active = p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0;
            if !active && !t.targetAvailable.as_bool() {
                continue;
            }
            let (gdi_name, region) = if active {
                let name = source_gdi_name(p.sourceInfo.adapterId, p.sourceInfo.id);
                let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx };
                let region = modes
                    .get(idx as usize)
                    .filter(|m| idx != DISPLAYCONFIG_PATH_MODE_IDX_INVALID && m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)
                    .map(|m| {
                        let s = unsafe { m.Anonymous.sourceMode };
                        OutputRegion { x: s.position.x, y: s.position.y, width: s.width, height: s.height }
                    });
                (name, region)
            } else {
                (None, None)
            };
            let entry = DisplayTarget {
                vdd: adapter_is_instance(&adapter_path(t.adapterId), &instance),
                target_id: t.id,
                active,
                gdi_name,
                region,
            };
            // QDC_ALL_PATHS lists a target once per possible source; keep the
            // active path when there is one.
            match out.iter_mut().find(|(a, e)| same_luid(*a, t.adapterId) && e.target_id == t.id) {
                Some((_, e)) if !e.active && entry.active => *e = entry,
                Some(_) => {}
                None => out.push((t.adapterId, entry)),
            }
        }
        out.into_iter().map(|(_, e)| e).collect()
    }

    fn attach(&self, target_id: u32) -> Result<(), String> {
        let instance = find_driver().map(|d| d.instance_id).ok_or("the driver is gone")?;
        let (paths, modes) = query_config().ok_or("QueryDisplayConfig failed")?;
        let mut active: Vec<DISPLAYCONFIG_PATH_INFO> =
            paths.iter().filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0).copied().collect();
        // A path to our target from a source nothing else on that adapter uses.
        let candidate = paths.iter().find(|p| {
            p.targetInfo.id == target_id
                && adapter_is_instance(&adapter_path(p.targetInfo.adapterId), &instance)
                && !active
                    .iter()
                    .any(|a| same_luid(a.sourceInfo.adapterId, p.sourceInfo.adapterId) && a.sourceInfo.id == p.sourceInfo.id)
        });
        let mut path = *candidate.ok_or("no free display source for the new monitor")?;
        path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
        path.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
        path.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
        active.push(path);
        let rc = unsafe {
            SetDisplayConfig(
                Some(&active),
                Some(&modes),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(format!("SetDisplayConfig returned {rc}"))
        }
    }

    fn set_mode(&self, gdi_name: &str, mode: Mode, position: Option<(i32, i32)>) -> Result<(), String> {
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
        dm.dmPelsWidth = mode.width;
        dm.dmPelsHeight = mode.height;
        // DEVMODE only takes whole hertz; 59.94 becomes 60.
        dm.dmDisplayFrequency = (mode.refresh_mhz + 500) / 1000;
        if let Some((x, y)) = position {
            dm.dmFields |= DM_POSITION;
            dm.Anonymous1.Anonymous2.dmPosition.x = x;
            dm.Anonymous1.Anonymous2.dmPosition.y = y;
        }
        set_devmode(gdi_name, &dm)
    }

    fn detach(&self, gdi_name: &str) -> Result<(), String> {
        // A zero-sized mode at the origin takes a monitor off the desktop.
        let mut dm = DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_POSITION;
        set_devmode(gdi_name, &dm)
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d)
    }
}

/// The driver answers in UTF-8 (`PONG`, log lines) except `GETSETTINGS`,
/// which is UTF-16LE.
fn decode_reply(bytes: &[u8]) -> String {
    let utf16 = bytes.len() >= 2 && bytes.len() % 2 == 0 && bytes.iter().skip(1).step_by(2).all(|&b| b == 0);
    if utf16 {
        let units: Vec<u16> = bytes.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        from_wide(&units)
    } else {
        String::from_utf8_lossy(bytes).trim_end_matches('\0').to_string()
    }
}
