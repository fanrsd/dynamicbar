//! Windows integration: battery/network/system status, power actions, and background watchers
//! that push volume, mic, lock-key, media, notification, clipboard and download events to the UI.

use base64::{engine::general_purpose::STANDARD, Engine};
use parking_lot::Mutex;
use serde::Serialize;
use serde_json::json;
use std::{
    collections::HashSet,
    os::windows::process::CommandExt,
    process::Command,
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter};
use windows::{
    core::{Interface, Result, HSTRING, PWSTR},
    Foundation::Size,
    Media::Control::{
        GlobalSystemMediaTransportControlsSessionManager as MediaManager,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
    },
    Networking::Connectivity::NetworkInformation,
    Storage::Streams::{Buffer, DataReader, IRandomAccessStreamReference, InputStreamOptions},
    UI::Notifications::{
        KnownNotificationBindings,
        Management::{UserNotificationListener, UserNotificationListenerAccessStatus},
        NotificationKinds, UserNotification,
    },
    Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, FILETIME, HANDLE, LUID, POINT, RECT},
        Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST},
        Media::Audio::{
            eCapture, eCommunications, eConsole, eRender, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator,
            MMDeviceEnumerator,
        },
        NetworkManagement::{
            IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2},
            Ndis::IfOperStatusUp,
        },
        Security::{
            AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
            SE_SHUTDOWN_NAME, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
        },
        Storage::FileSystem::{
            CreateFileW, ReadDirectoryChangesW, FILE_ACTION_RENAMED_NEW_NAME, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE,
            FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::{
            Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_ALL, COINIT_MULTITHREADED},
            DataExchange::GetClipboardSequenceNumber,
            Power::{
                GetSystemPowerStatus, SetSuspendState, SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED,
                ES_SYSTEM_REQUIRED, SYSTEM_POWER_STATUS,
            },
            Registry::{RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, HKEY, HKEY_CURRENT_USER, KEY_READ, RRF_RT_REG_QWORD},
            Shutdown::LockWorkStation,
            SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX},
            Threading::{GetCurrentProcess, GetSystemTimes, OpenProcessToken},
        },
        UI::{
            Input::KeyboardAndMouse::{GetKeyState, VK_CAPITAL, VK_NUMLOCK},
            Shell::{SHGetKnownFolderPath, FOLDERID_Downloads, KF_FLAG_DEFAULT},
            WindowsAndMessaging::{GetClassNameW, GetCursorPos, GetForegroundWindow, GetWindowRect},
        },
    },
};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn cursor() -> (i32, i32) {
    let mut p = POINT::default();
    let _ = unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

/// True when the foreground window exactly covers its monitor (fullscreen video, games,
/// F11 browsers). Maximized windows don't match: they stop at the taskbar or overshoot
/// the monitor by their resize border.
pub fn fullscreen_app() -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        let mut class = [0u16; 32];
        let n = GetClassNameW(fg, &mut class) as usize;
        // The desktop itself is monitor-sized.
        if fg.is_invalid() || matches!(String::from_utf16_lossy(&class[..n]).as_str(), "Progman" | "WorkerW") {
            return false;
        }
        let mut r = RECT::default();
        let mut mi = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        GetWindowRect(fg, &mut r).is_ok()
            && GetMonitorInfoW(MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST), &mut mi).as_bool()
            && r == mi.rcMonitor
    }
}

#[derive(Serialize)]
pub struct Status {
    battery: Option<u8>,
    charging: bool,
    online: bool,
    wifi: bool,
}

#[tauri::command]
pub fn status() -> Status {
    let mut p = SYSTEM_POWER_STATUS::default();
    let ok = unsafe { GetSystemPowerStatus(&mut p) }.is_ok();
    // BatteryFlag 128 = no system battery; 255 = unknown percentage.
    let battery = (ok && p.BatteryFlag & 128 == 0 && p.BatteryLifePercent <= 100).then_some(p.BatteryLifePercent);
    let profile = NetworkInformation::GetInternetConnectionProfile().ok();
    Status {
        battery,
        charging: ok && p.ACLineStatus == 1,
        online: profile.is_some(),
        wifi: profile.and_then(|p| p.IsWlanConnectionProfile().ok()).unwrap_or(false),
    }
}

#[tauri::command]
pub fn power_action(action: String) -> std::result::Result<(), String> {
    match action.as_str() {
        "lock" => unsafe { LockWorkStation() }.map_err(|e| e.to_string()),
        "sleep" => {
            enable_shutdown_privilege();
            // bhibernate=false → sleep (S3/modern standby), even when hibernation is enabled.
            if unsafe { SetSuspendState(false, false, false) } {
                Ok(())
            } else {
                Err("SetSuspendState failed".into())
            }
        }
        "restart" => spawn("shutdown", &["/r", "/t", "0"]),
        "shutdown" => spawn("shutdown", &["/s", "/t", "0"]),
        "screenshot" => spawn("explorer", &["ms-screenclip:"]),
        "wifi" => spawn("explorer", &["ms-settings:network-wifi"]),
        "bluetooth" => spawn("explorer", &["ms-settings:bluetooth"]),
        "settings" => spawn("explorer", &["ms-settings:"]),
        "downloads" => spawn("explorer", &["shell:Downloads"]),
        _ => Err(format!("unknown action: {action}")),
    }
}

fn spawn(cmd: &str, args: &[&str]) -> std::result::Result<(), String> {
    Command::new(cmd)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

/// Opens a file with its default app, or selects it in Explorer when `reveal`.
#[tauri::command]
pub fn open_path(path: String, reveal: bool) -> std::result::Result<(), String> {
    // raw_arg: explorer only understands `/select,"C:\a b"`, not Rust's whole-arg quoting.
    let arg = if reveal { format!("/select,\"{path}\"") } else { format!("\"{path}\"") };
    Command::new("explorer").raw_arg(arg).spawn().map(drop).map_err(|e| e.to_string())
}

#[derive(Serialize)]
pub struct Stats {
    cpu: u8,
    ram: u8,
    /// Bytes per second across physical adapters.
    down: u64,
    up: u64,
}

/// Previous (idle ticks, total ticks, rx bytes, tx bytes, when): rates are deltas between calls.
static LAST_STATS: Mutex<Option<(u64, u64, u64, u64, Instant)>> = Mutex::new(None);

#[tauri::command]
pub fn stats() -> Stats {
    let ticks = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
    let (mut idle, mut kernel, mut user) = Default::default();
    let _ = unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) };
    // Kernel time includes idle time.
    let (idle, total) = (ticks(idle), ticks(kernel) + ticks(user));
    let mut mem = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    let _ = unsafe { GlobalMemoryStatusEx(&mut mem) };
    let (rx, tx) = net_bytes();
    let now = Instant::now();
    let mut last = LAST_STATS.lock();
    let (cpu, down, up) = match *last {
        Some((i0, t0, r0, x0, at)) => {
            let secs = now.duration_since(at).as_secs_f64().max(0.001);
            let busy = 100u64.saturating_sub(idle.saturating_sub(i0) * 100 / total.saturating_sub(t0).max(1));
            (busy as u8, (rx.saturating_sub(r0) as f64 / secs) as u64, (tx.saturating_sub(x0) as f64 / secs) as u64)
        }
        None => (0, 0, 0),
    };
    *last = Some((idle, total, rx, tx, now));
    Stats { cpu, ram: mem.dwMemoryLoad as u8, down, up }
}

fn net_bytes() -> (u64, u64) {
    unsafe {
        let mut t: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        if GetIfTable2(&mut t).is_err() {
            return (0, 0);
        }
        let rows = std::slice::from_raw_parts((*t).Table.as_ptr(), (*t).NumEntries as usize);
        // Bit 0 HardwareInterface, bit 1 FilterInterface: filter and virtual rows would double-count traffic.
        let sum = rows
            .iter()
            .filter(|r| r.InterfaceAndOperStatusFlags._bitfield & 3 == 1 && r.OperStatus == IfOperStatusUp)
            .fold((0, 0), |(rx, tx), r| (rx + r.InOctets, tx + r.OutOctets));
        FreeMibTable(t as _);
        sum
    }
}

/// Default mic mute state, flipped first when `toggle`. Mutes the console and communications
/// defaults so call apps bound to either go silent.
#[tauri::command]
pub fn mic(toggle: bool) -> std::result::Result<bool, String> {
    com_init();
    let run = || -> Result<bool> {
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let ep = |role| -> Result<IAudioEndpointVolume> { en.GetDefaultAudioEndpoint(eCapture, role)?.Activate(CLSCTX_ALL, None) };
            let muted = ep(eConsole)?.GetMute()?.as_bool() ^ toggle;
            if toggle {
                for role in [eConsole, eCommunications] {
                    ep(role)?.SetMute(muted, std::ptr::null())?;
                }
            }
            Ok(muted)
        }
    };
    run().map_err(|e| e.to_string())
}

/// Sync commands run on the main thread, which lives as long as the app, so the flag sticks.
#[tauri::command]
pub fn keep_awake(on: bool) {
    let flags = if on { ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED } else { ES_CONTINUOUS };
    unsafe { SetThreadExecutionState(flags) };
}

#[derive(Serialize)]
pub struct InUse {
    camera: Vec<String>,
    mic: Vec<String>,
}

/// Apps using the camera/mic right now, from the consent store behind the Windows privacy
/// indicator: an app is live while its LastUsedTimeStop is 0.
#[tauri::command]
pub fn inuse() -> InUse {
    InUse { camera: capability_users("webcam"), mic: capability_users("microphone") }
}

fn capability_users(kind: &str) -> Vec<String> {
    let base = format!(r"Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\{kind}");
    let packaged = subkeys(&base).into_iter().map(|k| (format!(r"{base}\{k}"), k));
    let desktop = subkeys(&format!(r"{base}\NonPackaged")).into_iter().map(|k| (format!(r"{base}\NonPackaged\{k}"), k));
    let mut apps: Vec<String> = packaged
        .chain(desktop)
        .filter(|(path, _)| qword(path, "LastUsedTimeStart") > 0 && qword(path, "LastUsedTimeStop") == 0)
        .map(|(_, k)| match k.rsplit_once('#') {
            // Desktop apps: C:#Program Files#Zoom#bin#Zoom.exe
            Some((_, exe)) => exe.trim_end_matches(".exe").to_owned(),
            // Store apps: Microsoft.WindowsCamera_8wekyb3d8bbwe
            None => k.split('_').next().and_then(|n| n.rsplit('.').next()).unwrap_or(&k).to_owned(),
        })
        .collect();
    apps.sort_unstable();
    apps.dedup();
    apps
}

fn subkeys(path: &str) -> Vec<String> {
    let mut out = vec![];
    unsafe {
        let mut key = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(path), None, KEY_READ, &mut key) != ERROR_SUCCESS {
            return out;
        }
        let mut name = [0u16; 512];
        for i in 0.. {
            let mut len = name.len() as u32;
            if RegEnumKeyExW(key, i, Some(PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None) != ERROR_SUCCESS {
                break;
            }
            out.push(String::from_utf16_lossy(&name[..len as usize]));
        }
        let _ = RegCloseKey(key);
    }
    out
}

fn qword(path: &str, value: &str) -> u64 {
    let (mut v, mut size) = (0u64, 8u32);
    let _ = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, &HSTRING::from(path), &HSTRING::from(value), RRF_RT_REG_QWORD, None, Some(&mut v as *mut u64 as _), Some(&mut size))
    };
    v
}

fn enable_shutdown_privilege() {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_err() {
            return;
        }
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid).is_ok() {
            let tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
            };
            let _ = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        }
        let _ = CloseHandle(token);
    }
}

#[derive(Serialize, Clone, PartialEq)]
pub struct Media {
    title: String,
    artist: String,
    playing: bool,
    art: Option<String>,
}

/// Last media state, so the UI can fetch it on load instead of waiting for the next change.
static MEDIA: Mutex<Option<Media>> = Mutex::new(None);

#[tauri::command]
pub fn media() -> Option<Media> {
    MEDIA.lock().clone()
}

#[tauri::command]
pub fn media_control(action: String) {
    // WinRT async calls block; keep them off the UI thread.
    thread::spawn(move || {
        com_init();
        let run = || -> Result<()> {
            let s = MediaManager::RequestAsync()?.join()?.GetCurrentSession()?;
            match action.as_str() {
                "toggle" => s.TryTogglePlayPauseAsync()?.join()?,
                "next" => s.TrySkipNextAsync()?.join()?,
                "prev" => s.TrySkipPreviousAsync()?.join()?,
                _ => return Ok(()),
            };
            Ok(())
        };
        if let Err(e) = run() {
            eprintln!("media_control: {e}");
        }
    });
}

pub fn spawn_watchers(app: &AppHandle) {
    let jobs: [(&str, fn(&AppHandle) -> Result<()>); 6] = [
        ("volume", watch_volume),
        ("keys", watch_keys),
        ("media", watch_media),
        ("notifications", watch_notifications),
        ("clipboard", watch_clipboard),
        ("downloads", watch_downloads),
    ];
    for (name, job) in jobs {
        let app = app.clone();
        thread::spawn(move || {
            com_init();
            if let Err(e) = job(&app) {
                eprintln!("{name} watcher stopped: {e}");
            }
        });
    }
}

fn com_init() {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// Stores `v`; true when it differs from an earlier value (the first sighting isn't a change).
fn changed<T: PartialEq>(last: &mut Option<T>, v: T) -> bool {
    let fire = last.as_ref().is_some_and(|l| *l != v);
    *last = Some(v);
    fire
}

#[derive(Serialize, Clone, Copy, PartialEq)]
pub struct Volume {
    level: u8,
    muted: bool,
}

/// Default output volume; applies `level` (0–100) and/or `muted` first when given.
#[tauri::command]
pub fn volume(level: Option<u8>, muted: Option<bool>) -> std::result::Result<Volume, String> {
    com_init();
    let run = || -> Result<Volume> {
        unsafe {
            let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let ep: IAudioEndpointVolume = en.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)?;
            if let Some(l) = level {
                ep.SetMasterVolumeLevelScalar(l.min(100) as f32 / 100.0, std::ptr::null())?;
            }
            if let Some(m) = muted {
                ep.SetMute(m, std::ptr::null())?;
            }
            Ok(Volume { level: (ep.GetMasterVolumeLevelScalar()? * 100.0).round() as u8, muted: ep.GetMute()?.as_bool() })
        }
    };
    run().map_err(|e| e.to_string())
}

// ponytail: 150ms polling; IAudioEndpointVolumeCallback if latency/CPU ever matters
fn watch_volume(app: &AppHandle) -> Result<()> {
    let en: IMMDeviceEnumerator = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    // Re-query the default devices each tick so device switches are followed.
    let read = || -> Result<Volume> {
        unsafe {
            let ep: IAudioEndpointVolume = en.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)?;
            Ok(Volume {
                level: (ep.GetMasterVolumeLevelScalar()? * 100.0).round() as u8,
                muted: ep.GetMute()?.as_bool(),
            })
        }
    };
    let mic_muted = || -> Result<bool> {
        unsafe {
            let ep: IAudioEndpointVolume = en.GetDefaultAudioEndpoint(eCapture, eConsole)?.Activate(CLSCTX_ALL, None)?;
            Ok(ep.GetMute()?.as_bool())
        }
    };
    let (mut volume, mut mic) = (read().ok(), mic_muted().ok());
    loop {
        thread::sleep(Duration::from_millis(150));
        if read().is_ok_and(|v| changed(&mut volume, v)) {
            let _ = app.emit("volume", volume);
        }
        if mic_muted().is_ok_and(|m| changed(&mut mic, m)) {
            let _ = app.emit("mic", mic);
        }
    }
}

fn watch_keys(app: &AppHandle) -> Result<()> {
    let keys = [(VK_CAPITAL, "Caps Lock"), (VK_NUMLOCK, "Num Lock")];
    let mut last = [None; 2];
    loop {
        thread::sleep(Duration::from_millis(150));
        for ((vk, name), last) in keys.iter().zip(&mut last) {
            let on = unsafe { GetKeyState(vk.0 as i32) } & 1 != 0;
            if changed(last, on) {
                let _ = app.emit("lock", json!({ "key": name, "on": on }));
            }
        }
    }
}

/// Browsers download into a temp name and rename it on completion, so a rename *into* a
/// non-temp name is the "finished" signal. Files copied straight into the folder don't toast.
fn watch_downloads(app: &AppHandle) -> Result<()> {
    const TEMP: [&str; 5] = [".crdownload", ".part", ".partial", ".download", ".tmp"];
    let dir = unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None)?;
        let dir = p.to_string();
        CoTaskMemFree(Some(p.0 as _));
        dir?
    };
    let h = unsafe {
        CreateFileW(
            &HSTRING::from(&dir),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )?
    };
    let mut buf = vec![0u32; 16 * 1024]; // FILE_NOTIFY_INFORMATION needs DWORD alignment
    loop {
        let mut n = 0;
        unsafe {
            ReadDirectoryChangesW(h, buf.as_mut_ptr() as _, (buf.len() * 4) as u32, false, FILE_NOTIFY_CHANGE_FILE_NAME, Some(&mut n), None, None)?
        };
        let mut at = buf.as_ptr() as *const u8;
        // n == 0: the buffer overflowed and the batch was dropped.
        while n > 0 {
            let info = unsafe { &*(at as *const FILE_NOTIFY_INFORMATION) };
            let name = String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(info.FileName.as_ptr(), info.FileNameLength as usize / 2)
            });
            let lower = name.to_lowercase();
            if info.Action == FILE_ACTION_RENAMED_NEW_NAME && !TEMP.iter().any(|t| lower.ends_with(t)) {
                let _ = app.emit("download", json!({ "path": format!(r"{dir}\{name}"), "name": name }));
            }
            if info.NextEntryOffset == 0 {
                break;
            }
            at = unsafe { at.add(info.NextEntryOffset as usize) };
        }
    }
}

fn watch_media(app: &AppHandle) -> Result<()> {
    let mgr = MediaManager::RequestAsync()?.join()?;
    loop {
        let prev = MEDIA.lock().clone();
        let cur = read_media(&mgr, prev.as_ref()).ok();
        if cur != prev {
            *MEDIA.lock() = cur.clone();
            let _ = app.emit("media", cur);
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn read_media(mgr: &MediaManager, prev: Option<&Media>) -> Result<Media> {
    let s = mgr.GetCurrentSession()?;
    let p = s.TryGetMediaPropertiesAsync()?.join()?;
    let (title, artist) = (p.Title()?.to_string(), p.Artist()?.to_string());
    if title.is_empty() {
        return Err(windows::core::Error::empty());
    }
    let playing = s.GetPlaybackInfo()?.PlaybackStatus()? == PlaybackStatus::Playing;
    // Thumbnails are big; only re-read when the track changes (or the last read failed).
    let art = match prev {
        Some(m) if m.title == title && m.artist == artist && m.art.is_some() => m.art.clone(),
        _ => p.Thumbnail().ok().and_then(|t| read_image(&t).ok()),
    };
    Ok(Media { title, artist, playing, art })
}

fn read_image(r: &IRandomAccessStreamReference) -> Result<String> {
    let s = r.OpenReadAsync()?.join()?;
    let n = s.Size()? as u32;
    let buf = s.ReadAsync(&Buffer::Create(n)?, n, InputStreamOptions::None)?.join()?;
    let mut bytes = vec![0; buf.Length()? as usize];
    DataReader::FromBuffer(&buf)?.ReadBytes(&mut bytes)?;
    let ct = s.ContentType()?.to_string();
    let ct = if ct.starts_with("image/") { ct } else { "image/png".into() };
    Ok(format!("data:{ct};base64,{}", STANDARD.encode(bytes)))
}

#[derive(Serialize, Clone)]
pub struct Notification {
    pub app: String,
    pub title: String,
    pub body: String,
    pub icon: Option<String>,
    /// AppUserModelId of the source app, so the UI can launch it (`shell:AppsFolder\<id>`).
    pub aumid: Option<String>,
}

/// Needs "Notification access" for the app in Windows privacy settings; stops quietly if denied.
fn watch_notifications(app: &AppHandle) -> Result<()> {
    let l = UserNotificationListener::Current()?;
    if l.RequestAccessAsync()?.join()? != UserNotificationListenerAccessStatus::Allowed {
        eprintln!("notification access not granted");
        return Ok(());
    }
    let ids = || -> Result<(HashSet<u32>, Vec<UserNotification>)> {
        let list: Vec<UserNotification> = l.GetNotificationsAsync(NotificationKinds::Toast)?.join()?.into_iter().collect();
        Ok((list.iter().filter_map(|n| n.Id().ok()).collect(), list))
    };
    let mut seen = ids()?.0;
    loop {
        thread::sleep(Duration::from_secs(1));
        let Ok((now, list)) = ids() else { continue };
        for n in list.iter().filter(|n| n.Id().is_ok_and(|id| !seen.contains(&id))) {
            if let Ok(p) = read_notification(n) {
                let _ = app.emit("notification", p);
            }
        }
        seen = now;
    }
}

fn read_notification(n: &UserNotification) -> Result<Notification> {
    let app_info = n.AppInfo().ok();
    let info = app_info.as_ref().and_then(|a| a.DisplayInfo().ok());
    let binding = n.Notification()?.Visual()?.GetBinding(&KnownNotificationBindings::ToastGeneric()?)?;
    let texts: Vec<String> = binding
        .GetTextElements()?
        .into_iter()
        .filter_map(|t| t.Text().ok().map(|s| s.to_string()))
        .collect();
    Ok(Notification {
        app: info.as_ref().and_then(|i| i.DisplayName().ok()).map_or("Notification".into(), |s| s.to_string()),
        title: texts.first().cloned().unwrap_or_default(),
        body: texts.get(1..).unwrap_or_default().join(" "),
        icon: info
            .and_then(|i| i.GetLogo(Size { Width: 64.0, Height: 64.0 }).ok())
            .and_then(|r| r.cast::<IRandomAccessStreamReference>().ok())
            .and_then(|r| read_image(&r).ok()),
        aumid: app_info.and_then(|a| a.AppUserModelId().ok()).map(|s| s.to_string()).filter(|s| !s.is_empty()),
    })
}

fn watch_clipboard(app: &AppHandle) -> Result<()> {
    let mut last = unsafe { GetClipboardSequenceNumber() };
    let mut clip = arboard::Clipboard::new().ok();
    loop {
        thread::sleep(Duration::from_millis(400));
        let seq = unsafe { GetClipboardSequenceNumber() };
        if seq == last {
            continue;
        }
        last = seq;
        if clip.is_none() {
            clip = arboard::Clipboard::new().ok();
        }
        let Some(text) = clip.as_mut().and_then(|c| c.get_text().ok()) else { continue };
        let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !text.is_empty() {
            let _ = app.emit("clipboard", text.chars().take(120).collect::<String>());
        }
    }
}
