#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
mod devices;
mod sys;
mod wapriv;

use parking_lot::Mutex;
use std::{thread, time::Duration};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, PhysicalPosition, State, WebviewWindow,
};

#[derive(Default, Clone, Copy)]
struct Hit {
    /// Island rect in CSS px relative to the window: [x, y, w, h].
    rect: [f64; 4],
    /// Island anchored to the bottom (above the taskbar) instead of the top.
    bottom: bool,
    /// Physical y of the screen edge the island is anchored to.
    edge: i32,
}

struct Island(Mutex<Hit>);

#[tauri::command]
fn set_island(state: State<Island>, x: f64, y: f64, w: f64, h: f64) {
    state.0.lock().rect = [x, y, w, h];
}

#[tauri::command]
fn set_position(window: WebviewWindow, bottom: bool) -> Result<(), String> {
    place(&window, bottom).map_err(|e| e.to_string())
}

#[tauri::command]
fn username() -> String {
    std::env::var("USERNAME").unwrap_or_default()
}

/// Keeps the island out of screenshots, recordings and screen shares while it stays visible locally.
#[tauri::command]
fn set_capture_hidden(window: WebviewWindow, hidden: bool) -> Result<(), String> {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE};
    let hwnd = window.hwnd().map_err(|e| e.to_string())?;
    unsafe { SetWindowDisplayAffinity(hwnd, if hidden { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE }) }.map_err(|e| e.to_string())
}

/// Centers the window horizontally; pins it to the screen top or just above the taskbar.
fn place(win: &WebviewWindow, bottom: bool) -> tauri::Result<()> {
    let Some(m) = win.current_monitor()?.or(win.primary_monitor()?) else {
        return Ok(());
    };
    let size = win.outer_size()?;
    let wa = m.work_area();
    let x = m.position().x + (m.size().width as i32 - size.width as i32) / 2;
    let y = if bottom {
        wa.position.y + wa.size.height as i32 - size.height as i32
    } else {
        m.position().y
    };
    {
        let state = win.state::<Island>();
        let mut hit = state.0.lock();
        hit.bottom = bottom;
        hit.edge = if bottom { m.position().y + m.size().height as i32 - 1 } else { m.position().y };
    }
    win.set_position(PhysicalPosition::new(x, y))
}

/// The window is a large transparent overlay that is click-through except while the island is shown.
/// - by default the island is hidden as a thin line ("out")
/// - cursor pushed against the screen edge within the island's width → "open" (UI shows the pill; a click expands it)
/// - it stays while the cursor is over it or the strip between it and the edge; that area
///   never shrinks while open, so switching to a shorter panel doesn't drop it out from under the cursor
/// - a fullscreen foreground app hides the island entirely
// ponytail: 30ms polling; switch to a low-level mouse hook if it ever shows up in CPU profiles
fn watch_hover(win: WebviewWindow) {
    thread::spawn(move || {
        let mut state = "out";
        let mut ignoring = true;
        let mut fullscreen = false;
        let mut sticky = [0.0f64; 4]; // [left, top, right, bottom] of everything shown since opening
        loop {
            thread::sleep(Duration::from_millis(30));
            if !win.is_visible().unwrap_or(false) {
                continue;
            }
            let (Ok(pos), Ok(scale)) = (win.outer_position(), win.scale_factor()) else {
                continue;
            };
            let fs = sys::fullscreen_app();
            if fs != fullscreen {
                fullscreen = fs;
                let _ = win.emit("fullscreen", fs);
            }
            let Hit { rect: [x, y, w, h], bottom, edge } = *win.state::<Island>().0.lock();
            let (px, py) = sys::cursor();
            let (cx, cy) = ((px - pos.x) as f64 / scale, (py - pos.y) as f64 / scale);
            let in_x = cx >= x && cx < x + w;
            let inside = in_x && cy >= y && cy < y + h;
            let at_edge = in_x && if bottom { py >= edge } else { py <= edge };
            sticky = if state == "open" {
                [sticky[0].min(x), sticky[1].min(y), sticky[2].max(x + w), sticky[3].max(y + h)]
            } else {
                [x, y, x + w, y + h]
            };
            let toward_edge =
                cx >= sticky[0] && cx < sticky[2] && if bottom { cy >= sticky[1] } else { cy < sticky[3] };

            let next = if fullscreen {
                "out"
            } else if at_edge || (state == "open" && toward_edge) {
                "open"
            } else {
                "out"
            };
            if next != state {
                state = next;
                let _ = win.emit("hover", state);
            }
            let ignore = !(state == "open" && inside);
            if ignore != ignoring {
                ignoring = ignore;
                let _ = win.set_ignore_cursor_events(ignore);
            }
        }
    });
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(Island(Mutex::default()))
        .invoke_handler(tauri::generate_handler![
            set_island,
            set_position,
            set_capture_hidden,
            username,
            sys::status,
            sys::stats,
            sys::inuse,
            sys::mic,
            sys::volume,
            sys::keep_awake,
            sys::open_path,
            sys::media,
            sys::media_control,
            sys::power_action,
            devices::drives,
            devices::eject,
            devices::monitors,
            devices::set_brightness,
            wapriv::wapriv,
            wapriv::set_wapriv,
        ])
        .setup(|app| {
            let win = app.get_webview_window("main").unwrap();
            win.set_ignore_cursor_events(true)?;
            place(&win, false)?;
            win.show()?;
            wapriv::spawn(app.handle().clone(), win.hwnd()?);
            watch_hover(win);
            sys::spawn_watchers(app.handle());
            devices::watch_usb(app.handle().clone());
            api::spawn(app.handle().clone());

            let toggle = MenuItem::with_id(app, "toggle", "Hide Island", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit DynamicBar", true, None::<&str>)?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("DynamicBar")
                .menu(&Menu::with_items(app, &[&toggle, &quit])?)
                .on_menu_event(move |app, e| match e.id.as_ref() {
                    "toggle" => {
                        let win = app.get_webview_window("main").unwrap();
                        let visible = win.is_visible().unwrap_or(true);
                        let _ = if visible { win.hide() } else { win.show() };
                        let _ = toggle.set_text(if visible { "Show Island" } else { "Hide Island" });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running DynamicBar");
}
