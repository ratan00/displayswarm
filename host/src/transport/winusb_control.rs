//! Direct WinUSB access to a phone's ADB interface, for the AOA mode switch.
//!
//! `nusb` on Windows only claims interfaces of devices whose driver is WinUSB or
//! USBCCGP. Some phones (Samsung: `dg_ssudbus`) bind their own bus driver to
//! the composite device, so `nusb` rejects every interface with "incompatible
//! driver" even though the ADB child interface *is* bound to WinUSB (through the
//! MS OS compatible ID the phone's adb function reports). This module opens
//! that child directly by its device-interface path and sends the AOA vendor
//! requests (device recipient) through the WinUSB handle.
//!
//! The child is found by the adb device-interface GUID (the one Google's USB
//! driver and the phone's MS OS descriptor register) and matched to the phone by
//! `vid_xxxx&pid_xxxx` in the path. Two identical phones therefore resolve to
//! the first one; the next poll handles the other once the first has switched.

use std::ffi::c_void;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows::Win32::Devices::Usb::{
    WinUsb_ControlTransfer, WinUsb_Free, WinUsb_Initialize, WinUsb_SetPipePolicy, PIPE_TRANSFER_TIMEOUT,
    WINUSB_INTERFACE_HANDLE, WINUSB_SETUP_PACKET,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// The adb interface GUID (`ANDROID_USB_CLASS_ID`), registered by WinUSB for the
/// ADB child of a phone.
const ADB_INTERFACE_GUID: GUID = GUID::from_u128(0xF72FE0D4_CBCB_407D_8814_9ED673D0DD6B);

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// An open WinUSB handle on a phone's ADB interface.
pub struct WinUsbControl {
    file: HANDLE,
    winusb: WINUSB_INTERFACE_HANDLE,
}

// SAFETY: WinUSB handles are thread-safe.
unsafe impl Send for WinUsbControl {}
unsafe impl Sync for WinUsbControl {}

impl Drop for WinUsbControl {
    fn drop(&mut self) {
        unsafe {
            let _ = WinUsb_Free(self.winusb);
            let _ = CloseHandle(self.file);
        }
    }
}

/// Lower-cased `vid_xxxx&pid_xxxx` as it appears in device paths.
fn vid_pid_key(vid: u16, pid: u16) -> String {
    format!("vid_{vid:04x}&pid_{pid:04x}")
}

/// Device-interface paths (lower-cased) of the present adb interfaces.
fn adb_interface_paths() -> Result<Vec<String>, String> {
    let set = unsafe {
        SetupDiGetClassDevsW(Some(&ADB_INTERFACE_GUID), PCWSTR::null(), HWND::default(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE)
    }
    .map_err(|e| format!("SetupDiGetClassDevs: {e}"))?;
    let mut paths = Vec::new();
    let mut index = 0;
    loop {
        let mut data = SP_DEVICE_INTERFACE_DATA { cbSize: std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
        if unsafe { SetupDiEnumDeviceInterfaces(set, None, &ADB_INTERFACE_GUID, index, &mut data) }.is_err() {
            break;
        }
        index += 1;
        let mut need = 0u32;
        // First call only sizes the buffer (it fails with ERROR_INSUFFICIENT_BUFFER).
        let _ = unsafe { SetupDiGetDeviceInterfaceDetailW(set, &data, None, 0, Some(&mut need), None) };
        if need == 0 {
            continue;
        }
        // u64 elements keep the buffer 8-byte aligned for the detail struct.
        let mut buf = vec![0u64; (need as usize).div_ceil(8)];
        let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
        unsafe { (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        if unsafe { SetupDiGetDeviceInterfaceDetailW(set, &data, Some(detail), need, None, None) }.is_err() {
            continue;
        }
        let path = unsafe { PCWSTR((*detail).DevicePath.as_ptr()).to_string() };
        if let Ok(p) = path {
            paths.push(p.to_lowercase());
        }
    }
    unsafe {
        let _ = SetupDiDestroyDeviceInfoList(set);
    }
    Ok(paths)
}

impl WinUsbControl {
    /// Opens the WinUSB-bound ADB interface of the phone `vid:pid`.
    pub fn open_adb_interface(vid: u16, pid: u16) -> Result<Self, String> {
        let key = vid_pid_key(vid, pid);
        let path = adb_interface_paths()?
            .into_iter()
            .find(|p| p.contains(&key))
            .ok_or_else(|| "no WinUSB-bound ADB interface".to_string())?;
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let file = unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                HANDLE::default(),
            )
        }
        // ERROR_ACCESS_DENIED / ERROR_SHARING_VIOLATION when the adb server holds it.
        .map_err(|e| e.to_string())?;
        let mut winusb = WINUSB_INTERFACE_HANDLE::default();
        if let Err(e) = unsafe { WinUsb_Initialize(file, &mut winusb) } {
            unsafe {
                let _ = CloseHandle(file);
            }
            return Err(format!("WinUsb_Initialize: {e}"));
        }
        let timeout_ms: u32 = 1000;
        unsafe {
            let _ = WinUsb_SetPipePolicy(
                winusb,
                0,
                PIPE_TRANSFER_TIMEOUT,
                4,
                &timeout_ms as *const u32 as *const c_void,
            );
        }
        Ok(Self { file, winusb })
    }

    /// Vendor request with device recipient. `data_in` is the buffer read into
    /// for an IN request; for OUT the payload goes in `data_out`.
    fn transfer(&self, request_type: u8, request: u8, index: u16, buf: &mut [u8]) -> Result<u32, String> {
        let setup = WINUSB_SETUP_PACKET {
            RequestType: request_type,
            Request: request,
            Value: 0,
            Index: index,
            Length: buf.len() as u16,
        };
        let mut done = 0u32;
        let slice = if buf.is_empty() { None } else { Some(&mut *buf) };
        unsafe { WinUsb_ControlTransfer(self.winusb, setup, slice, Some(&mut done), None) }
            .map_err(|e| e.to_string())?;
        Ok(done)
    }

    pub fn vendor_in(&self, request: u8, length: u16) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; length as usize];
        let n = self.transfer(0xC0, request, 0, &mut buf)?;
        buf.truncate(n as usize);
        Ok(buf)
    }

    pub fn vendor_out(&self, request: u8, index: u16, data: &[u8]) -> Result<(), String> {
        let mut buf = data.to_vec();
        self.transfer(0x40, request, index, &mut buf).map(|_| ())
    }
}
