//! Windows capability detection: asks the OS what the session can capture and
//! inject and whether a display driver can add a monitor for the phone, and
//! lets [`crate::capture::windows_probe`] decide what that means for the roles.
//! With the Virtual Display Driver installed that is Mirror, window and Extend;
//! without it Extend is refused with what to install. There is no layout
//! backend yet, so Phone-primary is refused either way.

use super::model::Capabilities;
use super::virtual_monitor::{select, OwnIddPlaceholder, ProviderKind, VirtualMonitorProvider};
use super::windows_vdd::{VddProvider, ANY_VERSION_ENV};
use super::windows_vdd_sys::WinVdd;
use std::sync::{Arc, OnceLock};
use crate::capture::windows_probe::{capabilities, Probe};
use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};

pub fn detect() -> Capabilities {
    capabilities(&probe())
}

/// The facts about this session. Each one falls back to "not available" when
/// its query fails, so a broken environment offers less rather than more.
fn probe() -> Probe {
    Probe {
        build: windows_build(),
        wgc_supported: windows::Graphics::Capture::GraphicsCaptureSession::IsSupported().unwrap_or(false),
        dxgi_output: has_dxgi_output(),
        monitors: unsafe { GetSystemMetrics(SM_CMONITORS) }.max(0) as usize,
        interactive_session: interactive_session(),
        virtual_monitor: choose_provider().map(|_| ()),
    }
}

/// The Virtual Display Driver provider, built once (cheap, no I/O).
fn vdd() -> &'static VddProvider<WinVdd> {
    static VDD: OnceLock<VddProvider<WinVdd>> = OnceLock::new();
    VDD.get_or_init(|| {
        let allow_any = std::env::var(ANY_VERSION_ENV).is_ok_and(|v| v == "1");
        VddProvider::new(WinVdd, allow_any)
    })
}

/// Startup: puts back the driver settings a crashed earlier run left changed
/// (blocks for a driver reload when there is something to restore).
pub fn recover_leftovers() {
    vdd().recover();
}

/// Which provider adds monitors here: our own driver (on hold, never
/// available yet), then the Virtual Display Driver. `Err` is the reason Extend
/// is refused.
fn choose_provider() -> Result<ProviderKind, String> {
    select(&[
        (ProviderKind::OwnIdd, OwnIddPlaceholder.availability()),
        (ProviderKind::Vdd, vdd().availability()),
    ])
}

/// The provider to add the phone's monitor with, or why there is none.
pub fn virtual_monitor_provider() -> Result<Arc<dyn VirtualMonitorProvider>, String> {
    Ok(match choose_provider()? {
        ProviderKind::OwnIdd => Arc::new(OwnIddPlaceholder),
        ProviderKind::Vdd => Arc::new(vdd().clone()),
    })
}

/// The build number from `RtlGetVersion`, which unlike `GetVersionEx` does not
/// lie to applications without a compatibility manifest.
fn windows_build() -> u32 {
    let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    if unsafe { RtlGetVersion(&mut info) }.is_ok() {
        info.dwBuildNumber
    } else {
        0
    }
}

fn has_dxgi_output() -> bool {
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return false };
    let mut i = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(i) } {
        if unsafe { adapter.EnumOutputs(0) }.is_ok() {
            return true;
        }
        i += 1;
    }
    false
}

/// Session 0 is for services: nothing there can capture or inject into the
/// user's desktop (W6 runs the host as a user-session process instead).
fn interactive_session() -> bool {
    let mut session = 0u32;
    match unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } {
        Ok(()) => session != 0,
        Err(_) => false,
    }
}
