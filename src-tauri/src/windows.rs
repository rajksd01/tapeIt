use objc2_app_kit::{NSWindow, NSWindowSharingType};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

pub const TOOLBAR: &str = "toolbar";
pub const OVERLAY: &str = "overlay";
pub const CAMERA: &str = "camera";

pub const EDITOR: &str = "editor";

const EDITOR_WIDTH: f64 = 920.0;
const EDITOR_HEIGHT: f64 = 680.0;

/// Opens the editor for the last recording, or reloads it if already open.
pub fn open_editor(app: &AppHandle) -> Result<(), String> {
    if let Some(editor) = app.get_webview_window(EDITOR) {
        editor.eval("location.reload()").map_err(|error| error.to_string())?;
        return editor.set_focus().map_err(|error| error.to_string());
    }

    WebviewWindowBuilder::new(app, EDITOR, WebviewUrl::App("editor.html".into()))
        .title("TapeIt — Edit")
        .inner_size(EDITOR_WIDTH, EDITOR_HEIGHT)
        .min_inner_size(640.0, 520.0)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .center()
        .focused(true)
        .build()
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub const DASHBOARD: &str = "dashboard";

const DASHBOARD_WIDTH: f64 = 1100.0;
const DASHBOARD_HEIGHT: f64 = 760.0;

/// Opens the recordings dashboard, or focuses it if already open.
pub fn open_dashboard(app: &AppHandle) -> Result<(), String> {
    if let Some(dashboard) = app.get_webview_window(DASHBOARD) {
        dashboard.show().map_err(|error| error.to_string())?;
        return dashboard.set_focus().map_err(|error| error.to_string());
    }

    WebviewWindowBuilder::new(app, DASHBOARD, WebviewUrl::App("dashboard.html".into()))
        .title("TapeIt")
        .inner_size(DASHBOARD_WIDTH, DASHBOARD_HEIGHT)
        .min_inner_size(720.0, 520.0)
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true)
        .center()
        .focused(true)
        .build()
        .map_err(|error| error.to_string())?;
    Ok(())
}

pub const CAMERA_SIZE: f64 = 220.0;
const CAMERA_MARGIN: f64 = 32.0;

// The toolbar window is larger than the pill: the margin holds the shadow and the tooltips below it.
const TOOLBAR_WIDTH: f64 = 880.0;
const TOOLBAR_HEIGHT: f64 = 112.0;
const TOOLBAR_TOP: f64 = 34.0;

// AppKit window levels: the toolbar must sit above the drawing overlay.
const STATUS_WINDOW_LEVEL: isize = 25;
const POPUP_MENU_WINDOW_LEVEL: isize = 101;

pub struct Created {
    pub toolbar_id: u32,
    pub scale: f64,
    pub screen_width: f64,
    pub screen_height: f64,
}

pub fn create(app: &AppHandle) -> Result<Created, String> {
    let monitor = app
        .primary_monitor()
        .map_err(|error| error.to_string())?
        .ok_or("No monitor")?;
    let scale = monitor.scale_factor();
    let size = monitor.size().to_logical::<f64>(scale);

    // accept_first_mouse: without it macOS spends the first click on an unfocused window on focusing it.
    let overlay = WebviewWindowBuilder::new(app, OVERLAY, WebviewUrl::App("overlay.html".into()))
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .resizable(false)
        .skip_taskbar(true)
        .focused(false)
        .accept_first_mouse(true)
        .visible_on_all_workspaces(true)
        .position(0.0, 0.0)
        .inner_size(size.width, size.height)
        .build()
        .map_err(|error| error.to_string())?;
    overlay.set_ignore_cursor_events(true).map_err(|error| error.to_string())?;
    set_level(&overlay, STATUS_WINDOW_LEVEL)?;

    let toolbar = WebviewWindowBuilder::new(app, TOOLBAR, WebviewUrl::App("toolbar.html".into()))
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .resizable(false)
        .skip_taskbar(true)
        .accept_first_mouse(true)
        .visible_on_all_workspaces(true)
        .position((size.width - TOOLBAR_WIDTH) / 2.0, TOOLBAR_TOP)
        .inner_size(TOOLBAR_WIDTH, TOOLBAR_HEIGHT)
        .build()
        .map_err(|error| error.to_string())?;
    let toolbar_id = set_level(&toolbar, POPUP_MENU_WINDOW_LEVEL)?;

    Ok(Created {
        toolbar_id,
        scale,
        screen_width: size.width,
        screen_height: size.height,
    })
}

/// Opens the webcam bubble bottom-left, or closes it. Returns true when it is now open.
pub fn toggle_camera(app: &AppHandle) -> Result<bool, String> {
    if let Some(camera) = app.get_webview_window(CAMERA) {
        camera.close().map_err(|error| error.to_string())?;
        return Ok(false);
    }

    let monitor = app
        .primary_monitor()
        .map_err(|error| error.to_string())?
        .ok_or("No monitor")?;
    let size = monitor.size().to_logical::<f64>(monitor.scale_factor());

    let camera = WebviewWindowBuilder::new(app, CAMERA, WebviewUrl::App("camera.html".into()))
        .transparent(true)
        .decorations(false)
        .shadow(false)
        .resizable(false)
        .skip_taskbar(true)
        .focused(false)
        .accept_first_mouse(true)
        .visible_on_all_workspaces(true)
        .position(CAMERA_MARGIN, size.height - CAMERA_SIZE - CAMERA_MARGIN)
        .inner_size(CAMERA_SIZE, CAMERA_SIZE)
        .build()
        .map_err(|error| error.to_string())?;
    set_level(&camera, STATUS_WINDOW_LEVEL + 1)?;
    Ok(true)
}

/// Hides the toolbar from screen capture (Zoom, Slack, Meet, screenshots) unless the user wants it shown.
pub fn set_toolbar_shared(app: &AppHandle, shared: bool) {
    let Some(toolbar) = app.get_webview_window(TOOLBAR) else {
        return;
    };
    let window = toolbar.clone();
    let _ = toolbar.run_on_main_thread(move || {
        let Ok(pointer) = window.ns_window() else {
            return;
        };
        let ns_window = unsafe { &*(pointer as *const NSWindow) };
        let sharing = if shared { NSWindowSharingType::ReadOnly } else { NSWindowSharingType::None };
        ns_window.setSharingType(sharing);
    });
}

/// Sets the AppKit level and returns the window number, which ScreenCaptureKit uses as the window ID.
fn set_level(window: &WebviewWindow, level: isize) -> Result<u32, String> {
    let pointer = window.ns_window().map_err(|error| error.to_string())?;
    let ns_window = unsafe { &*(pointer as *const NSWindow) };
    ns_window.setLevel(level);
    Ok(ns_window.windowNumber() as u32)
}
