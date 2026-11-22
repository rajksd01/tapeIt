use tauri::image::Image;
use tauri::menu::Menu;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Wry};

use crate::windows;

const TRAY_ID: &str = "screenpen";

// 18pt menu bar glyph at 2x.
const ICON_SIZE: u32 = 36;
const RING_RADIUS: f64 = 14.0;
const RING_WIDTH: f64 = 2.6;
const DOT_RADIUS: f64 = 7.0;

const TEMPLATE_INK: [u8; 3] = [0, 0, 0];
const RECORDING_RED: [u8; 3] = [255, 69, 58];

pub enum Look {
    Idle,
    Recording,
}

/// Draws a ring with a dot inside ("record" glyph), anti-aliased by distance to each edge.
fn glyph(ink: [u8; 3]) -> Image<'static> {
    let center = f64::from(ICON_SIZE) / 2.0;
    let mut pixels = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);

    for row in 0..ICON_SIZE {
        for column in 0..ICON_SIZE {
            let distance = (f64::from(column) + 0.5 - center).hypot(f64::from(row) + 0.5 - center);
            let ring = (RING_WIDTH / 2.0 - (distance - RING_RADIUS).abs() + 0.5).clamp(0.0, 1.0);
            let dot = (DOT_RADIUS - distance + 0.5).clamp(0.0, 1.0);
            let alpha = (ring.max(dot) * 255.0) as u8;
            pixels.extend_from_slice(&[ink[0], ink[1], ink[2], alpha]);
        }
    }
    Image::new_owned(pixels, ICON_SIZE, ICON_SIZE)
}

pub fn create(app: &AppHandle, menu: &Menu<Wry>) -> tauri::Result<()> {
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(glyph(TEMPLATE_INK))
        .icon_as_template(true)
        .tooltip("TapeIt")
        .menu(menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event else {
                return;
            };
            toggle_toolbar(tray.app_handle());
        })
        .build(app)?;
    Ok(())
}

/// Replaces the right-click menu, so its check marks follow the settings.
pub fn set_menu(app: &AppHandle, menu: Menu<Wry>) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_menu(Some(menu));
    }
}

pub fn set_look(app: &AppHandle, look: Look) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    let (ink, template) = match look {
        Look::Idle => (TEMPLATE_INK, true),
        Look::Recording => (RECORDING_RED, false),
    };
    let _ = tray.set_icon(Some(glyph(ink)));
    let _ = tray.set_icon_as_template(template);
}

pub fn toggle_toolbar(app: &AppHandle) {
    let Some(toolbar) = app.get_webview_window(windows::TOOLBAR) else {
        return;
    };
    if toolbar.is_visible().unwrap_or(false) {
        let _ = toolbar.hide();
        return;
    }
    let _ = toolbar.show();
}
