use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use block2::RcBlock;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventType};
use serde::Serialize;
use tauri::{AppHandle, Emitter};

pub static SHOW_CLICKS: AtomicBool = AtomicBool::new(true);
pub static SHOW_KEYS: AtomicBool = AtomicBool::new(false);
pub static TRACK_CURSOR: AtomicBool = AtomicBool::new(false);

static APP: OnceLock<(AppHandle, f64)> = OnceLock::new();
static CLICK_LOG: Mutex<Option<(Instant, Vec<(f64, f64, f64)>)>> = Mutex::new(None);

const KEY_RETURN: u16 = 36;
const KEY_TAB: u16 = 48;
const KEY_SPACE: u16 = 49;
const KEY_DELETE: u16 = 51;
const KEY_ESCAPE: u16 = 53;
const KEY_LEFT: u16 = 123;
const KEY_RIGHT: u16 = 124;
const KEY_DOWN: u16 = 125;
const KEY_UP: u16 = 126;

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum InputEvent {
    Click { x: f64, y: f64 },
    Move { x: f64, y: f64 },
    Key { label: String },
}

/// Watches clicks, cursor moves and keys in other apps and forwards them to the overlay.
/// Keys need Accessibility permission; without it macOS delivers only mouse events.
pub fn start(app: AppHandle, screen_height: f64) {
    let _ = APP.set((app, screen_height));

    let mask = NSEventMask::LeftMouseDown
        | NSEventMask::RightMouseDown
        | NSEventMask::MouseMoved
        | NSEventMask::LeftMouseDragged
        | NSEventMask::KeyDown;
    let block = RcBlock::new(|event: NonNull<NSEvent>| on_event(unsafe { event.as_ref() }));
    let monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block);

    // The monitor lives for the whole app run.
    std::mem::forget(monitor);
}

fn on_event(event: &NSEvent) {
    let Some((app, screen_height)) = APP.get() else {
        return;
    };

    let location = NSEvent::mouseLocation();
    let x = location.x;
    let y = screen_height - location.y;

    let is_click = matches!(event.r#type(), NSEventType::LeftMouseDown | NSEventType::RightMouseDown);
    if is_click {
        log_click(x, y);
    }

    let payload = match event.r#type() {
        NSEventType::LeftMouseDown | NSEventType::RightMouseDown if SHOW_CLICKS.load(Ordering::Relaxed) => {
            InputEvent::Click { x, y }
        }
        NSEventType::MouseMoved | NSEventType::LeftMouseDragged if TRACK_CURSOR.load(Ordering::Relaxed) => {
            InputEvent::Move { x, y }
        }
        NSEventType::KeyDown if SHOW_KEYS.load(Ordering::Relaxed) => InputEvent::Key { label: key_label(event) },
        _ => return,
    };

    let _ = app.emit_to(crate::windows::OVERLAY, "input", payload);
}

/// Starts collecting clicks for zoom-on-clicks; the log runs until `end_click_log`.
pub fn begin_click_log() {
    *CLICK_LOG.lock().unwrap() = Some((Instant::now(), Vec::new()));
}

/// Returns clicks as (seconds since start, x, y) in screen points.
pub fn end_click_log() -> Vec<(f64, f64, f64)> {
    CLICK_LOG.lock().unwrap().take().map(|(_, clicks)| clicks).unwrap_or_default()
}

fn log_click(x: f64, y: f64) {
    let mut log = CLICK_LOG.lock().unwrap();
    if let Some((started, clicks)) = log.as_mut() {
        clicks.push((started.elapsed().as_secs_f64(), x, y));
    }
}

// Example: ⌘⇧K, ⌃C, ↩.
fn key_label(event: &NSEvent) -> String {
    let flags = event.modifierFlags();
    let mut label = String::new();
    for (flag, symbol) in [
        (NSEventModifierFlags::Control, "⌃"),
        (NSEventModifierFlags::Option, "⌥"),
        (NSEventModifierFlags::Shift, "⇧"),
        (NSEventModifierFlags::Command, "⌘"),
    ] {
        if flags.contains(flag) {
            label.push_str(symbol);
        }
    }

    let key = match event.keyCode() {
        KEY_RETURN => "↩".to_string(),
        KEY_TAB => "⇥".to_string(),
        KEY_SPACE => "Space".to_string(),
        KEY_DELETE => "⌫".to_string(),
        KEY_ESCAPE => "Esc".to_string(),
        KEY_LEFT => "←".to_string(),
        KEY_RIGHT => "→".to_string(),
        KEY_DOWN => "↓".to_string(),
        KEY_UP => "↑".to_string(),
        _ => event
            .charactersIgnoringModifiers()
            .map(|text| text.to_string().to_uppercase())
            .unwrap_or_default(),
    };
    label.push_str(&key);
    label
}
