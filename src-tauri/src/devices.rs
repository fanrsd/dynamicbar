//! USB drives (arrival toast + safe eject) and display brightness: DDC/CI for external monitors,
//! WMI for the built-in laptop panel.

use parking_lot::Mutex;
use serde::Serialize;
use std::{thread, time::Duration};
use tauri::{AppHandle, Emitter};
use windows::{
    core::{w, BOOL, BSTR, HSTRING, PCWSTR},
    Win32::{
        Devices::{
            DeviceAndDriverInstallation::{
                CM_Get_Parent, CM_Request_Device_EjectW, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
                SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
                PNP_VETO_TYPE, SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
            },
            Display::{
                DestroyPhysicalMonitors, GetMonitorBrightness, GetNumberOfPhysicalMonitorsFromHMONITOR,
                GetPhysicalMonitorsFromHMONITOR, SetMonitorBrightness, PHYSICAL_MONITOR,
            },
        },
        Foundation::{CloseHandle, HANDLE, LPARAM, RECT},
        Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR},
        Storage::FileSystem::{
            BusTypeUsb, CreateFileW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW, FILE_FLAGS_AND_ATTRIBUTES,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            Com::{
                CoCreateInstance, CoInitializeEx, CoSetProxyBlanket, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, EOAC_NONE,
                RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE,
            },
            Ioctl::{
                PropertyStandardQuery, StorageDeviceProperty, GUID_DEVINTERFACE_DISK, IOCTL_STORAGE_GET_DEVICE_NUMBER,
                IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY,
            },
            Variant::VARIANT,
            Wmi::{
                IWbemClassObject, IWbemLocator, IWbemServices, WbemLocator, WBEM_FLAG_FORWARD_ONLY,
                WBEM_FLAG_RETURN_IMMEDIATELY, WBEM_GENERIC_FLAG_TYPE, WBEM_INFINITE,
            },
            IO::DeviceIoControl,
        },
    },
};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;

#[derive(Serialize, Clone, PartialEq)]
pub struct Drive {
    letter: char,
    name: String,
}

#[tauri::command]
pub fn drives() -> Vec<Drive> {
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| (b'A' + i) as char)
        .filter_map(|l| {
            // Skips network/optical drives before touching them; those can stall.
            let kind = unsafe { GetDriveTypeW(&HSTRING::from(format!(r"{l}:\"))) };
            if kind != DRIVE_REMOVABLE && kind != DRIVE_FIXED || !on_usb(l) {
                return None;
            }
            // Fails for an empty card-reader slot: nothing to show or eject.
            let mut label = [0u16; 261];
            unsafe { GetVolumeInformationW(&HSTRING::from(format!(r"{l}:\")), Some(&mut label), None, None, None, None) }.ok()?;
            let label = String::from_utf16_lossy(&label[..label.iter().position(|&c| c == 0).unwrap_or(0)]);
            Some(Drive { letter: l, name: if label.is_empty() { "USB Drive".into() } else { label } })
        })
        .collect()
}

pub fn watch_usb(app: AppHandle) {
    thread::spawn(move || {
        let mut last = drives();
        loop {
            thread::sleep(Duration::from_secs(1));
            let now = drives();
            for d in now.iter().filter(|d| !last.contains(d)) {
                let _ = app.emit("usb", d);
            }
            last = now;
        }
    });
}

fn open_volume(letter: char) -> windows::core::Result<HANDLE> {
    // Zero access rights: enough for IOCTL queries, doesn't need admin or block other users.
    unsafe {
        CreateFileW(
            &HSTRING::from(format!(r"\\.\{letter}:")),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
    }
}

fn on_usb(letter: char) -> bool {
    let Ok(h) = open_volume(letter) else { return false };
    let q = STORAGE_PROPERTY_QUERY { PropertyId: StorageDeviceProperty, QueryType: PropertyStandardQuery, ..Default::default() };
    let mut d = STORAGE_DEVICE_DESCRIPTOR::default();
    let mut n = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&q as *const _ as _),
            size_of_val(&q) as u32,
            Some(&mut d as *mut _ as _),
            size_of_val(&d) as u32,
            Some(&mut n),
            None,
        )
    }
    .is_ok();
    let _ = unsafe { CloseHandle(h) };
    ok && d.BusType == BusTypeUsb
}

fn device_number(h: HANDLE) -> Option<(u32, u32)> {
    let mut d = STORAGE_DEVICE_NUMBER::default();
    let mut n = 0;
    unsafe {
        DeviceIoControl(h, IOCTL_STORAGE_GET_DEVICE_NUMBER, None, 0, Some(&mut d as *mut _ as _), size_of_val(&d) as u32, Some(&mut n), None)
    }
    .ok()?;
    Some((d.DeviceType, d.DeviceNumber))
}

/// Device-manager node of the disk holding `number` (from IOCTL_STORAGE_GET_DEVICE_NUMBER).
fn disk_node(number: (u32, u32)) -> Option<u32> {
    unsafe {
        let set = SetupDiGetClassDevsW(Some(&GUID_DEVINTERFACE_DISK), PCWSTR::null(), None, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE).ok()?;
        let mut found = None;
        for i in 0.. {
            let mut iface = SP_DEVICE_INTERFACE_DATA { cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInterfaces(set, None, &GUID_DEVINTERFACE_DISK, i, &mut iface).is_err() {
                break;
            }
            // Detail = cbSize + inline UTF-16 path; u32 buffer keeps it aligned.
            let mut buf = [0u32; 512];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            let mut dev = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiGetDeviceInterfaceDetailW(set, &iface, Some(detail), size_of_val(&buf) as u32, None, Some(&mut dev)).is_err() {
                continue;
            }
            let path = PCWSTR((*detail).DevicePath.as_ptr());
            let Ok(h) = CreateFileW(path, 0, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_FLAGS_AND_ATTRIBUTES(0), None) else {
                continue;
            };
            let n = device_number(h);
            let _ = CloseHandle(h);
            if n == Some(number) {
                found = Some(dev.DevInst);
                break;
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        found
    }
}

/// "Safely remove": asks PnP to eject the disk's USB parent, which flushes caches and fails
/// (veto) while some app still holds a file open on it.
#[tauri::command(async)]
pub fn eject(letter: char) -> Result<(), String> {
    let h = open_volume(letter).map_err(|e| e.to_string())?;
    let number = device_number(h);
    let _ = unsafe { CloseHandle(h) };
    let disk = number.and_then(disk_node).ok_or("disk not found")?;
    let mut parent = 0;
    if unsafe { CM_Get_Parent(&mut parent, disk, 0) } != CR_SUCCESS {
        return Err("no parent device".into());
    }
    // Explorer and indexers often hold a handle for a moment after the last write; retry briefly.
    for _ in 0..3 {
        let mut veto = PNP_VETO_TYPE(0);
        let mut name = [0u16; 260];
        if unsafe { CM_Request_Device_EjectW(parent, Some(&mut veto), Some(&mut name), 0) } == CR_SUCCESS && veto.0 == 0 {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(400));
    }
    Err("drive is in use".into())
}

#[derive(Serialize)]
pub struct Monitor {
    /// Index into the physical-monitor enumeration, or `BUILTIN`, for `set_brightness`.
    id: usize,
    name: String,
    min: u32,
    max: u32,
    value: u32,
}

/// Pseudo monitor id for the built-in panel, which has no DDC/CI and goes through WMI instead.
/// Small on purpose: `usize::MAX` doesn't survive the round trip through a JS number.
const BUILTIN: usize = 999;

fn wmi() -> windows::core::Result<IWbemServices> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
        let none = BSTR::new();
        let svc = locator.ConnectServer(&BSTR::from(r"ROOT\WMI"), &none, &none, &none, 0, &none, None)?;
        // 10 = RPC_C_AUTHN_WINNT, 0 = RPC_C_AUTHZ_NONE
        CoSetProxyBlanket(&svc, 10, 0, None, RPC_C_AUTHN_LEVEL_CALL, RPC_C_IMP_LEVEL_IMPERSONATE, None, EOAC_NONE)?;
        Ok(svc)
    }
}

/// First instance of a WMI class, plus the requested property.
fn wmi_first(svc: &IWbemServices, class: &str, prop: PCWSTR) -> windows::core::Result<(IWbemClassObject, VARIANT)> {
    unsafe {
        let flags = WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY;
        let rows = svc.ExecQuery(&BSTR::from("WQL"), &BSTR::from(format!("SELECT * FROM {class}")), flags, None)?;
        let (mut row, mut n) = ([None], 0);
        rows.Next(WBEM_INFINITE, &mut row, &mut n).ok()?;
        let obj = row[0].take().ok_or_else(windows::core::Error::empty)?;
        let mut v = VARIANT::default();
        obj.Get(prop, 0, &mut v, None, None)?;
        Ok((obj, v))
    }
}

fn builtin() -> Option<Monitor> {
    let (_, v) = wmi_first(&wmi().ok()?, "WmiMonitorBrightness", w!("CurrentBrightness")).ok()?;
    Some(Monitor { id: BUILTIN, name: "Built-in Display".into(), min: 0, max: 100, value: u32::try_from(&v).ok()? })
}

fn set_builtin(value: u8) -> windows::core::Result<()> {
    let svc = wmi()?;
    let (_, path) = wmi_first(&svc, "WmiMonitorBrightnessMethods", w!("__PATH"))?;
    unsafe {
        let mut class = None;
        svc.GetObject(&BSTR::from("WmiMonitorBrightnessMethods"), WBEM_GENERIC_FLAG_TYPE(0), None, Some(&mut class), None)?;
        let mut sig = None;
        class.ok_or_else(windows::core::Error::empty)?.GetMethod(w!("WmiSetBrightness"), 0, &mut sig, std::ptr::null_mut())?;
        let args = sig.ok_or_else(windows::core::Error::empty)?.SpawnInstance(0)?;
        args.Put(w!("Timeout"), 0, &VARIANT::from(0i32), 0)?;
        args.Put(w!("Brightness"), 0, &VARIANT::from(value), 0)?;
        svc.ExecMethod(&BSTR::try_from(&path)?, &BSTR::from("WmiSetBrightness"), WBEM_GENERIC_FLAG_TYPE(0), None, &args, None, None)
    }
}

/// DDC/CI commands are slow (tens of ms) and monitors drop overlapping ones.
static DDC: Mutex<()> = Mutex::new(());

fn with_monitors<T>(f: impl FnOnce(&[PHYSICAL_MONITOR]) -> T) -> T {
    unsafe extern "system" fn collect(h: HMONITOR, _: HDC, _: *mut RECT, out: LPARAM) -> BOOL {
        unsafe { (*(out.0 as *mut Vec<HMONITOR>)).push(h) };
        true.into()
    }
    let _guard = DDC.lock();
    let mut handles: Vec<HMONITOR> = vec![];
    let _ = unsafe { EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut handles as *mut _ as isize)) };
    let pms: Vec<PHYSICAL_MONITOR> = handles
        .into_iter()
        .flat_map(|h| unsafe {
            let mut n = 0;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(h, &mut n).is_err() {
                return vec![];
            }
            let mut v = vec![PHYSICAL_MONITOR::default(); n as usize];
            if GetPhysicalMonitorsFromHMONITOR(h, &mut v).is_err() {
                return vec![];
            }
            v
        })
        .collect();
    let out = f(&pms);
    let _ = unsafe { DestroyPhysicalMonitors(&pms) };
    out
}

/// The built-in panel (WMI) plus monitors that answer DDC/CI brightness queries.
#[tauri::command(async)]
pub fn monitors() -> Vec<Monitor> {
    let ddc: Vec<Monitor> = with_monitors(|pms| {
        pms.iter()
            .enumerate()
            .filter_map(|(id, pm)| {
                let (mut min, mut value, mut max) = (0, 0, 0);
                let ok = unsafe { GetMonitorBrightness(pm.hPhysicalMonitor, &mut min, &mut value, &mut max) } != 0;
                let d = pm.szPhysicalMonitorDescription; // copy: PHYSICAL_MONITOR is packed
                let name = match String::from_utf16_lossy(&d[..d.iter().position(|&c| c == 0).unwrap_or(d.len())]) {
                    n if n.is_empty() || n == "Generic PnP Monitor" => "External Display".to_owned(),
                    n => n,
                };
                (ok && max > min).then_some(Monitor { id, name, min, max, value })
            })
            .collect()
    });
    builtin().into_iter().chain(ddc).collect()
}

#[tauri::command(async)]
pub fn set_brightness(id: usize, value: u32) -> Result<(), String> {
    if id == BUILTIN {
        return set_builtin(value.min(100) as u8).map_err(|e| e.to_string());
    }
    with_monitors(|pms| {
        let pm = pms.get(id).ok_or("monitor gone")?;
        (unsafe { SetMonitorBrightness(pm.hPhysicalMonitor, value) } != 0).then_some(()).ok_or("monitor rejected DDC/CI".into())
    })
}
