//! WhatsApp privacy blur (port of the standalone WAPriv tool).
//!
//! A click-through layered pane is kept directly above the WhatsApp Desktop window and shows a
//! blurred copy of it, captured with PrintWindow. The blur is rendered here rather than by DWM
//! acrylic: any acrylic/blur-behind window over WhatsApp makes its WebView2 input ignore clicks,
//! even when the window is WS_EX_TRANSPARENT. Optional spotlight leaves a clear hole around the
//! cursor; holding Right Ctrl reveals the whole window.

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    sync::atomic::{AtomicIsize, Ordering},
    thread,
};
use tauri::{AppHandle, Emitter};
use windows::{
    core::{w, BOOL},
    Win32::{
        Foundation::{CloseHandle, COLORREF, HWND, LPARAM, POINT, RECT, SIZE, WPARAM},
        Graphics::{
            Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
            Gdi::{
                CreateCompatibleDC, CreateDIBSection, DeleteObject, SelectObject, SetBrushOrgEx, SetStretchBltMode,
                StretchBlt, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, DIB_RGB_COLORS,
                HALFTONE, HBITMAP, HDC, SRCCOPY,
            },
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        System::{
            Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS},
            LibraryLoader::GetModuleHandleW,
        },
        UI::{
            Accessibility::{SetWinEventHook, HWINEVENTHOOK},
            Input::KeyboardAndMouse::{GetAsyncKeyState, RegisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, VK_RCONTROL},
            WindowsAndMessaging::*,
        },
    },
};

const CAPTURE_EVERY_TICKS: i32 = 3; // 30ms ticks → WhatsApp re-captured every 90ms (~25ms of work each)
const HOTKEY_BLUR: i32 = 1; // Ctrl+Alt+B
const HOTKEY_SPOTLIGHT: i32 = 2; // Ctrl+Alt+S

/// Same file and shape as standalone WAPriv, so existing settings carry over.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "PascalCase", default)]
pub struct Settings {
    enabled: bool,
    spotlight: bool,
    /// 0 = off, 100 = flat veil.
    blur: u8,
    hole_w: i32,
    hole_h: i32,
}

impl Settings {
    const DEFAULT: Settings = Settings { enabled: true, spotlight: true, blur: 60, hole_w: 560, hole_h: 200 };
}

impl Default for Settings {
    fn default() -> Self {
        Self::DEFAULT
    }
}

static SETTINGS: Mutex<Settings> = Mutex::new(Settings::DEFAULT);
static PANE: AtomicIsize = AtomicIsize::new(0);

fn settings_path() -> std::path::PathBuf {
    std::path::Path::new(&std::env::var("APPDATA").unwrap_or_default()).join("WAPriv").join("settings.json")
}

#[tauri::command]
pub fn wapriv() -> Settings {
    *SETTINGS.lock()
}

#[tauri::command]
pub fn set_wapriv(app: AppHandle, mut settings: Settings) {
    settings.blur = settings.blur.min(100);
    update(&app, settings);
}

fn update(app: &AppHandle, s: Settings) {
    *SETTINGS.lock() = s;
    let path = settings_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Err(e) = std::fs::write(&path, serde_json::to_string(&s).unwrap()) {
        eprintln!("wapriv: saving settings failed: {e}");
    }
    let _ = app.emit("wapriv", s);
    // Wake the pane thread so the change shows without waiting for the next tick.
    let pane = PANE.load(Ordering::Relaxed);
    if pane != 0 {
        let _ = unsafe { PostMessageW(Some(HWND(pane as _)), WM_APP, WPARAM(0), LPARAM(0)) };
    }
}

/// Starts the pane thread. `island` is DynamicBar's own topmost window, kept above the pane.
pub fn spawn(app: AppHandle, island: HWND) {
    if let Some(s) = std::fs::read_to_string(settings_path()).ok().and_then(|t| serde_json::from_str(&t).ok()) {
        *SETTINGS.lock() = s;
    }
    let island = island.0 as isize;
    thread::spawn(move || {
        if let Err(e) = unsafe { run(&app, HWND(island as _)) } {
            eprintln!("wapriv stopped: {e}");
        }
    });
}

unsafe fn run(app: &AppHandle, island: HWND) -> windows::core::Result<()> {
    let hinst = GetModuleHandleW(None)?;
    let class = w!("DynamicBarWAPrivPane");
    RegisterClassW(&WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst.into(), lpszClassName: class, ..Default::default() });
    let pane = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class,
        w!(""),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinst.into()),
        None,
    )?;
    PANE.store(pane.0 as isize, Ordering::Relaxed);
    let mods = MOD_CONTROL | MOD_ALT | MOD_NOREPEAT;
    if RegisterHotKey(Some(pane), HOTKEY_BLUR, mods, 'B' as u32).is_err()
        || RegisterHotKey(Some(pane), HOTKEY_SPOTLIGHT, mods, 'S' as u32).is_err()
    {
        eprintln!("wapriv: Ctrl+Alt+B / Ctrl+Alt+S already taken (standalone WAPriv still running?)");
    }
    SetTimer(Some(pane), 1, 30, None);
    // Re-stack immediately when the foreground window changes instead of waiting for the next tick.
    SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(on_foreground), 0, 0, WINEVENT_OUTOFCONTEXT);

    let mut blur = Blur::new(pane, island);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        match msg.message {
            WM_HOTKEY => {
                let mut s = *SETTINGS.lock();
                if msg.wParam.0 as i32 == HOTKEY_BLUR {
                    s.enabled = !s.enabled;
                } else {
                    s.spotlight = !s.spotlight;
                }
                update(app, s);
            }
            WM_TIMER | WM_APP => blur.tick(),
            _ => {
                DispatchMessageW(&msg);
            }
        }
    }
    Ok(())
}

unsafe extern "system" fn on_foreground(_: HWINEVENTHOOK, _: u32, _: HWND, _: i32, _: i32, _: u32, _: u32) {
    let _ = PostMessageW(Some(HWND(PANE.load(Ordering::Relaxed) as _)), WM_APP, WPARAM(0), LPARAM(0));
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> windows::Win32::Foundation::LRESULT {
    DefWindowProcW(h, m, w, l)
}

/// A top-down 32bpp DIB selected into its own memory DC, reused across frames.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    bits: *mut u32,
    w: i32,
    h: i32,
}

impl Dib {
    unsafe fn new() -> Self {
        Dib { dc: CreateCompatibleDC(None), bmp: HBITMAP::default(), bits: std::ptr::null_mut(), w: 0, h: 0 }
    }

    unsafe fn ensure(&mut self, w: i32, h: i32) {
        if (self.w, self.h) == (w, h) {
            return;
        }
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(bmp) = CreateDIBSection(None, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else { return };
        SelectObject(self.dc, bmp.into());
        if !self.bmp.is_invalid() {
            let _ = DeleteObject(self.bmp.into());
        }
        (self.bmp, self.bits, self.w, self.h) = (bmp, bits.cast(), w, h);
    }

    unsafe fn pixels(&mut self) -> &mut [u32] {
        std::slice::from_raw_parts_mut(self.bits, (self.w * self.h) as usize)
    }
}

struct Blur {
    pane: HWND,
    island: HWND,
    wa: HWND,
    search_cooldown: i32,
    capture_countdown: i32,
    capture: Dib, // PrintWindow target, full window size
    small: Dib,   // downscaled frame
    surface: Dib, // what the layered window shows
    blurred: Vec<u32>,
    blurred_size: (i32, i32),
    blurred_pct: u8,
    last_hole: Option<RECT>,
}

impl Blur {
    unsafe fn new(pane: HWND, island: HWND) -> Self {
        Blur {
            pane,
            island,
            wa: HWND::default(),
            search_cooldown: 0,
            capture_countdown: 0,
            capture: Dib::new(),
            small: Dib::new(),
            surface: Dib::new(),
            blurred: Vec::new(),
            blurred_size: (0, 0),
            blurred_pct: 0,
            last_hole: None,
        }
    }

    unsafe fn tick(&mut self) {
        let s = *SETTINGS.lock();
        if !IsWindow(Some(self.wa)).as_bool() {
            self.search_cooldown -= 1;
            if self.search_cooldown <= 0 {
                self.wa = find_whatsapp();
                self.search_cooldown = 15;
            }
        }
        let wa = self.wa;
        let show = s.enabled
            && s.blur > 0
            && IsWindow(Some(wa)).as_bool()
            && IsWindowVisible(wa).as_bool()
            && !IsIconic(wa).as_bool()
            && GetAsyncKeyState(VK_RCONTROL.0 as i32) >= 0;
        if !show {
            self.place(None, false);
            self.capture_countdown = 0; // fresh capture as soon as the pane comes back
            return;
        }

        let mut win = RECT::default(); // frame bounds without invisible borders
        let mut full = RECT::default(); // PrintWindow captures this, invisible borders included
        if DwmGetWindowAttribute(wa, DWMWA_EXTENDED_FRAME_BOUNDS, &mut win as *mut _ as _, size_of::<RECT>() as u32).is_err()
            || GetWindowRect(wa, &mut full).is_err()
        {
            return;
        }
        let mut c = POINT::default();
        let _ = GetCursorPos(&mut c);
        let hole = s.spotlight.then(|| intersect(win, rect_around(c, s.hole_w, s.hole_h))).flatten();

        // While WhatsApp is active nothing else belongs above it, so go topmost: clicks can't raise WhatsApp over the blur.
        self.place(Some(win), GetForegroundWindow() == wa);

        // ponytail: fixed 90ms re-capture (~11fps); capture only on WhatsApp repaint if CPU use matters.
        self.capture_countdown -= 1;
        let capture = self.capture_countdown <= 0;
        if capture {
            self.capture_countdown = CAPTURE_EVERY_TICKS;
        }
        let (fw, fh) = (win.right - win.left, win.bottom - win.top);
        if fw <= 0 || fh <= 0 {
            return;
        }
        let hole = hole.map(|h| RECT { left: h.left - win.left, top: h.top - win.top, right: h.right - win.left, bottom: h.bottom - win.top });
        if capture || self.blurred_size != (fw, fh) || self.blurred_pct != s.blur {
            let frame = (win.left - full.left, win.top - full.top, fw, fh);
            self.reblur(wa, (full.right - full.left, full.bottom - full.top), frame, s.blur);
        } else if hole == self.last_hole {
            return;
        }
        self.last_hole = hole;
        self.present(fw, fh, hole);
    }

    /// Captures WhatsApp, downscales (HALFTONE = area average) and upscales bilinearly into `blurred`.
    /// 1-100%: blur radius grows (downscale factor 1..34); above 70% a dark veil fades in until 100% is flat.
    unsafe fn reblur(&mut self, wa: HWND, (ww, wh): (i32, i32), (fx, fy, fw, fh): (i32, i32, i32, i32), pct: u8) {
        self.capture.ensure(ww, wh);
        let _ = PrintWindow(wa, self.capture.dc, PRINT_WINDOW_FLAGS(2)); // PW_RENDERFULLCONTENT: WebView2/DComp content
        let div = 1 + pct as i32 / 3;
        let (sw, sh) = ((fw / div).max(1), (fh / div).max(1));
        self.small.ensure(sw, sh);
        SetStretchBltMode(self.small.dc, HALFTONE);
        let _ = SetBrushOrgEx(self.small.dc, 0, 0, None);
        let _ = StretchBlt(self.small.dc, 0, 0, sw, sh, Some(self.capture.dc), fx, fy, fw, fh, SRCCOPY);

        let veil = if pct <= 70 { 0 } else { (pct as u32 - 70) * 255 / 30 };
        self.blurred.resize((fw * fh) as usize, 0);
        bilinear(self.small.pixels(), sw as usize, sh as usize, &mut self.blurred, fw as usize, fh as usize, veil);
        self.blurred_size = (fw, fh);
        self.blurred_pct = pct;
    }

    /// Shows the blurred frame with `hole` (pane coords) left fully transparent.
    unsafe fn present(&mut self, fw: i32, fh: i32, hole: Option<RECT>) {
        self.surface.ensure(fw, fh);
        let px = self.surface.pixels();
        if px.len() != self.blurred.len() {
            return;
        }
        px.copy_from_slice(&self.blurred);
        if let Some(h) = hole {
            for y in h.top.max(0)..h.bottom.min(fh) {
                let row = (y * fw) as usize;
                px[row + h.left.max(0) as usize..row + h.right.min(fw) as usize].fill(0);
            }
        }
        let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
        let _ = UpdateLayeredWindow(
            self.pane,
            None,
            None,
            Some(&SIZE { cx: fw, cy: fh }),
            Some(self.surface.dc),
            Some(&POINT::default()),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        );
    }

    /// Shows the pane over WhatsApp (topmost while `topmost`), or hides it when `rect` is None.
    unsafe fn place(&mut self, rect: Option<RECT>, topmost: bool) {
        let pane = self.pane;
        let Some(r) = rect else {
            if IsWindowVisible(pane).as_bool() {
                let _ = ShowWindow(pane, SW_HIDE);
            }
            return;
        };
        // Owned by WhatsApp, Windows itself keeps the pane above it. Raising it ourselves only works while
        // DynamicBar holds foreground rights (SetWindowPos silently ignores the z-order otherwise).
        if GetWindow(pane, GW_OWNER).unwrap_or_default() != self.wa {
            SetWindowLongPtrW(pane, GWLP_HWNDPARENT, self.wa.0 as isize);
        }
        let is_top = is_topmost(pane);
        let mut cur = RECT::default();
        let _ = GetWindowRect(pane, &mut cur);
        // Windows ignores HWND_TOP from a background thread, so the pane can only be stacked by inserting it
        // below some visible window. When nothing non-topmost sits above WhatsApp (it's the top app, e.g. while
        // DynamicBar itself is foreground), there is no such window: go topmost instead.
        let above = visible_above(self.wa, pane);
        let topmost = topmost || above.is_invalid() || is_topmost(above);
        let stacked = if topmost { is_top } else { !is_top && visible_above(self.wa, HWND::default()) == pane };
        // Only touch the window when something changed.
        if stacked && IsWindowVisible(pane).as_bool() && cur == r {
            return;
        }
        let mut flags = SWP_NOACTIVATE | SWP_SHOWWINDOW;
        let after = if topmost {
            if is_top {
                flags |= SWP_NOZORDER; // re-asserting every tick would fight other topmost windows
            }
            HWND_TOPMOST
        } else {
            // Dropping below WhatsApp first clears topmost without flashing above the newly active app.
            if is_top {
                let _ = SetWindowPos(pane, Some(self.wa), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
            }
            above
        };
        let _ = SetWindowPos(pane, Some(after), r.left, r.top, r.right - r.left, r.bottom - r.top, flags);
        if !topmost && visible_above(self.wa, HWND::default()) != pane {
            // Windows sometimes silently ignores the insert (pane stays under WhatsApp, blur invisible). Seen
            // when the pane was far down the z-order; a hop through the topmost band and back unsticks it.
            let z = SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE;
            let _ = SetWindowPos(pane, Some(HWND_TOPMOST), 0, 0, 0, 0, z);
            let _ = SetWindowPos(pane, Some(HWND_NOTOPMOST), 0, 0, 0, 0, z);
            let _ = SetWindowPos(pane, Some(above), 0, 0, 0, 0, z);
        }
        if topmost && !is_top {
            // The pane just jumped to the top of the topmost band; put the island back above it.
            let _ = SetWindowPos(self.island, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE);
        }
    }
}

/// Bilinear upscale of a BGRA buffer with edge clamping, optional dark veil, opaque output.
fn bilinear(src: &[u32], sw: usize, sh: usize, dst: &mut [u32], dw: usize, dh: usize, veil: u32) {
    // Source sample position (fixed point, 8 fractional bits) for each output column/row.
    let axis = |d: usize, s: usize| -> Vec<(usize, usize, u32)> {
        (0..d)
            .map(|i| {
                let p = (((i as f32 + 0.5) * s as f32 / d as f32 - 0.5).max(0.0) * 256.0) as usize;
                let i0 = (p >> 8).min(s - 1);
                (i0, (i0 + 1).min(s - 1), (p & 255) as u32)
            })
            .collect()
    };
    let (xs, ys) = (axis(dw, sw), axis(dh, sh));
    let ch = |p: u32, shift: u32| (p >> shift) & 255;
    for (y, &(y0, y1, fy)) in ys.iter().enumerate() {
        let (r0, r1) = (&src[y0 * sw..][..sw], &src[y1 * sw..][..sw]);
        let out = &mut dst[y * dw..][..dw];
        for (o, &(x0, x1, fx)) in out.iter_mut().zip(&xs) {
            let mut px = 0xFF00_0000;
            for shift in [0, 8, 16] {
                let top = ch(r0[x0], shift) * (256 - fx) + ch(r0[x1], shift) * fx;
                let bottom = ch(r1[x0], shift) * (256 - fx) + ch(r1[x1], shift) * fx;
                let c = (top * (256 - fy) + bottom * fy) >> 16;
                let c = (c * (255 - veil) + 17 * veil) / 255;
                px |= c << shift;
            }
            *o = px;
        }
    }
}

fn rect_around(c: POINT, w: i32, h: i32) -> RECT {
    RECT { left: c.x - w / 2, top: c.y - h / 2, right: c.x - w / 2 + w, bottom: c.y - h / 2 + h }
}

/// First visible window above `h` in z-order, other than `skip`. Skips WinUI WhatsApp's hidden owned
/// IME windows: inserting the pane below one of those drops it under WhatsApp.
unsafe fn visible_above(h: HWND, skip: HWND) -> HWND {
    let mut h = GetWindow(h, GW_HWNDPREV).unwrap_or_default();
    while !h.is_invalid() && (h == skip || !IsWindowVisible(h).as_bool()) {
        h = GetWindow(h, GW_HWNDPREV).unwrap_or_default();
    }
    h
}

fn intersect(a: RECT, b: RECT) -> Option<RECT> {
    let r = RECT { left: a.left.max(b.left), top: a.top.max(b.top), right: a.right.min(b.right), bottom: a.bottom.min(b.bottom) };
    (r.left < r.right && r.top < r.bottom).then_some(r)
}

unsafe fn is_topmost(h: HWND) -> bool {
    GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0
}

/// Largest visible, unowned, titled top-level window of a WhatsApp* process.
unsafe fn find_whatsapp() -> HWND {
    struct Find {
        pids: HashSet<u32>,
        best: HWND,
        area: i64,
    }
    unsafe extern "system" fn each(h: HWND, l: LPARAM) -> BOOL {
        let f = &mut *(l.0 as *mut Find);
        let mut pid = 0;
        GetWindowThreadProcessId(h, Some(&mut pid));
        let mut r = RECT::default();
        if f.pids.contains(&pid)
            && IsWindowVisible(h).as_bool()
            && GetWindow(h, GW_OWNER).map_or(true, |o| o.is_invalid())
            && GetWindowTextLengthW(h) > 0
            && GetWindowRect(h, &mut r).is_ok()
        {
            let area = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
            if area > f.area {
                (f.best, f.area) = (h, area);
            }
        }
        true.into()
    }

    let mut f = Find { pids: HashSet::new(), best: HWND::default(), area: 0 };
    let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return f.best };
    let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
    let mut ok = Process32FirstW(snap, &mut e).is_ok();
    while ok {
        let n = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
        if String::from_utf16_lossy(&e.szExeFile[..n]).to_ascii_lowercase().starts_with("whatsapp") {
            f.pids.insert(e.th32ProcessID);
        }
        ok = Process32NextW(snap, &mut e).is_ok();
    }
    let _ = CloseHandle(snap);
    if !f.pids.is_empty() {
        let _ = EnumWindows(Some(each), LPARAM(&mut f as *mut Find as isize));
    }
    f.best
}

#[cfg(test)]
mod tests {
    use super::bilinear;

    #[test]
    fn bilinear_upscale() {
        // Black → white, 2px → 6px: edges clamp to the source pixels, middle ramps monotonically.
        let mut out = vec![0; 6];
        bilinear(&[0xFF00_0000, 0xFFFF_FFFF], 2, 1, &mut out, 6, 1, 0);
        let blue: Vec<u32> = out.iter().map(|p| p & 255).collect();
        assert_eq!((blue[0], blue[5]), (0, 255));
        assert!(blue.windows(2).all(|w| w[0] <= w[1]), "{blue:?}");
        assert!(out.iter().all(|p| p >> 24 == 255), "output must be opaque");
        // Full veil flattens everything to #111111.
        bilinear(&[0xFFFF_FFFF, 0xFF00_0000], 2, 1, &mut out, 6, 1, 255);
        assert!(out.iter().all(|&p| p == 0xFF11_1111), "{out:x?}");
    }
}
