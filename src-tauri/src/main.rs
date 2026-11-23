mod drive;
mod export;
mod input;
mod library;
mod media;
mod output;
mod queue;
mod recorder;
mod screenshot;
mod tray;
mod writer;
mod windows;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use output::{Settings, Target, UploadPolicy};
use queue::{Notice, Uploads};
use recorder::{Area, AudioSources, Recording};
use screenshot::Region;
use serde::{Deserialize, Serialize};
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::{ActivationPolicy, AppHandle, Emitter, Manager, State};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogResult};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

const SETTINGS_FILE: &str = "settings.json";
const LIBRARY_FILE: &str = "library.json";
const QUEUE_FILE: &str = "queue.json";
const THUMBS_DIR: &str = "thumbs";
const LIBRARY_CHANGED: &str = "library-changed";

// Upload prompt buttons; the dialog reports the pressed label.
const ASK_UPLOAD: &str = "Upload";
const ASK_ALWAYS: &str = "Always Upload";
const ASK_SKIP: &str = "Not Now";
const START_SOUND: &str = "/System/Library/Sounds/Tink.aiff";
const STOP_SOUND: &str = "/System/Library/Sounds/Pop.aiff";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Pointer,
    Draw,
    Select,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordingEvent {
    active: bool,
    area: Option<Area>,
}

/// The last finished recording, as the editor sees it.
#[derive(Clone, Serialize)]
struct Session {
    path: PathBuf,
    width: u32,
    height: u32,
    clicks: Vec<export::Click>,
}

struct AppState {
    recording: Mutex<Option<Recording>>,
    recording_area: Mutex<Area>,
    session: Mutex<Option<Session>>,
    screen: Area,
    settings: Mutex<Settings>,
    mode: Mutex<Mode>,
    toolbar_id: u32,
    scale: f64,
    uploads: Arc<Uploads>,
}

fn settings_path(app: &AppHandle) -> PathBuf {
    app.path().app_config_dir().unwrap_or_default().join(SETTINGS_FILE)
}

fn library_path(app: &AppHandle) -> PathBuf {
    app.path().app_config_dir().unwrap_or_default().join(LIBRARY_FILE)
}

fn drive_client(settings: &Settings) -> drive::Client {
    drive::Client::resolve(&settings.drive_client_id, &settings.drive_client_secret)
}

/// Tells the dashboard to reload its list.
fn library_changed(app: &AppHandle) {
    let _ = app.emit(LIBRARY_CHANGED, ());
}

/// Applies the upload choice to a new recording: queue it, skip it, or ask the user.
fn offer_upload(app: &AppHandle, settings: &Settings, path: &Path) {
    // Clipboard-only recordings have no file to upload; without Drive set up there is nothing to offer.
    if settings.target == Target::Clipboard || !drive::is_connected() {
        return;
    }

    match settings.upload {
        UploadPolicy::Never => {}
        UploadPolicy::Always => app.state::<AppState>().uploads.enqueue(path),
        UploadPolicy::Ask => ask_upload(app, path.to_path_buf()),
    }
}

/// "Upload this recording to Google Drive?" Upload / Not Now / Always Upload.
fn ask_upload(app: &AppHandle, path: PathBuf) {
    let handle = app.clone();
    let name = path.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default();
    app.dialog()
        .message(format!("{name}\n\nChange this later in the menu or the dashboard."))
        .title("Upload to Google Drive?")
        .buttons(MessageDialogButtons::YesNoCancelCustom(ASK_UPLOAD.into(), ASK_ALWAYS.into(), ASK_SKIP.into()))
        .show_with_result(move |result| {
            let MessageDialogResult::Custom(label) = result else {
                return;
            };
            if label == ASK_ALWAYS {
                update_settings(&handle, |settings| settings.upload = UploadPolicy::Always);
            }
            if label == ASK_UPLOAD || label == ASK_ALWAYS {
                handle.state::<AppState>().uploads.enqueue(&path);
                library_changed(&handle);
            }
        });
}

fn status(app: &AppHandle, message: impl Into<String>) {
    let _ = app.emit("status", message.into());
}

fn on_off(value: bool) -> &'static str {
    if value {
        return "on";
    }
    "off"
}

/// Appends one line to ~/Library/Logs/TapeIt.log; failures are ignored because logging is best-effort.
fn log_line(line: &str) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = PathBuf::from(home).join("Library/Logs/TapeIt.log");
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{} {line}", chrono::Local::now().format("%H:%M:%S"));
}

fn play(sound: &str) {
    let _ = Command::new("afplay").arg(sound).spawn();
}

fn apply_input_flags(settings: &Settings) {
    input::SHOW_CLICKS.store(settings.show_clicks, Ordering::Relaxed);
    input::SHOW_KEYS.store(settings.show_keys, Ordering::Relaxed);
}

#[tauri::command]
fn set_mode(app: AppHandle, state: State<AppState>, mode: Mode) -> Result<(), String> {
    *state.mode.lock().unwrap() = mode;

    let overlay = app.get_webview_window(windows::OVERLAY).ok_or("Overlay missing")?;
    overlay.set_ignore_cursor_events(mode == Mode::Pointer).map_err(|error| error.to_string())?;
    if mode != Mode::Pointer {
        overlay.set_focus().map_err(|error| error.to_string())?;
    }

    app.emit("mode", mode).map_err(|error| error.to_string())
}

#[tauri::command]
fn set_spotlight(app: AppHandle, on: bool) -> Result<(), String> {
    input::TRACK_CURSOR.store(on, Ordering::Relaxed);
    app.emit("spotlight", on).map_err(|error| error.to_string())
}

/// Asks the overlay to run the 3-2-1 countdown; the overlay then calls start_recording.
fn request_recording(app: &AppHandle, area: Option<Area>) {
    let _ = app.emit_to(windows::OVERLAY, "countdown", area);
}

#[tauri::command]
async fn start_recording(app: AppHandle, state: State<'_, AppState>, area: Option<Area>) -> Result<(), String> {
    let mut slot = state.recording.lock().unwrap();
    if slot.is_some() {
        return Ok(());
    }

    let cache = app.path().app_cache_dir().map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&cache).map_err(|error| error.to_string())?;
    let path = cache.join(output::file_name());

    let settings = state.settings.lock().unwrap().clone();
    let hidden: &[u32] = if settings.record_toolbar { &[] } else { &[state.toolbar_id] };
    let audio = AudioSources {
        microphone: settings.record_mic,
        system: settings.record_system_audio,
    };

    // Say what will be captured, so a silent recording is never a surprise.
    let heard = format!(
        "Recording · mic {} · Mac sound {}",
        on_off(audio.microphone),
        on_off(audio.system)
    );
    log_line(&heard);
    status(&app, heard);

    // The sound plays before capture starts, so system audio does not record it.
    play(START_SOUND);
    *slot = Some(recorder::start(&path, area, state.scale, hidden, audio)?);
    *state.recording_area.lock().unwrap() = area.unwrap_or(state.screen);
    windows::place_camera(&app, area.unwrap_or(state.screen));
    input::begin_click_log();
    tray::set_look(&app, tray::Look::Recording);
    app.emit("recording", RecordingEvent { active: true, area }).map_err(|error| error.to_string())
}

#[tauri::command]
async fn stop_recording(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let Some(recording) = state.recording.lock().unwrap().take() else {
        return Ok(());
    };
    let _ = app.emit("recording", RecordingEvent { active: false, area: None });
    play(STOP_SOUND);
    tray::set_look(&app, tray::Look::Idle);

    let clicks = input::end_click_log();
    let file = recording.stop()?;
    log_line(&format!("Stopped · {}", file.display()));
    let settings = state.settings.lock().unwrap().clone();

    // Mic, system audio and camera go back to off, and drawings clear, so the next recording starts clean.
    // This runs after capture stops, so the drawings still appear in the last frames.
    let _ = app.emit("clear", ());
    let _ = set_mode(app.clone(), app.state::<AppState>(), Mode::Pointer);
    update_settings(&app, Settings::reset_inputs);
    let _ = app.emit("mic", false);
    let _ = app.emit("system-audio", false);
    if windows::camera_open(&app) {
        let _ = toggle_camera(app.clone());
    }
    let (message, path) = output::deliver(&file, &settings)?;
    status(&app, message);
    library_changed(&app);
    offer_upload(&app, &settings, &path);

    let area = *state.recording_area.lock().unwrap();
    *state.session.lock().unwrap() = Some(Session {
        path,
        width: recorder::even(area.width * state.scale),
        height: recorder::even(area.height * state.scale),
        clicks: clicks_in(&area, &clicks),
    });

    if settings.open_editor {
        windows::open_editor(&app)?;
    }
    Ok(())
}

/// Keeps clicks inside the recorded area, as 0..1 fractions of it.
fn clicks_in(area: &Area, clicks: &[(f64, f64, f64)]) -> Vec<export::Click> {
    clicks
        .iter()
        .map(|&(seconds, x, y)| export::Click {
            seconds,
            x: (x - area.x) / area.width,
            y: (y - area.y) / area.height,
        })
        .filter(|click| (0.0..=1.0).contains(&click.x) && (0.0..=1.0).contains(&click.y))
        .collect()
}

#[tauri::command]
fn get_session(state: State<AppState>) -> Option<Session> {
    state.session.lock().unwrap().clone()
}

#[tauri::command]
async fn export_clip(app: AppHandle, state: State<'_, AppState>, request: export::Request) -> Result<String, String> {
    let session = state.session.lock().unwrap().clone().ok_or("No recording to export")?;
    let cache = app.path().app_cache_dir().map_err(|error| error.to_string())?;
    let output = cache.join(output::export_name(request.format.extension()));

    let source = export::Source {
        input: &session.path,
        clicks: &session.clicks,
    };
    let editor = app.clone();
    export::run(&source, &request, &output, |fraction| {
        let _ = editor.emit_to(windows::EDITOR, "export-progress", fraction);
    })?;

    let settings = state.settings.lock().unwrap().clone();
    let (message, path) = output::deliver(&output, &settings)?;
    let _ = app.emit_to(windows::EDITOR, "exported", path.to_string_lossy().to_string());
    library_changed(&app);
    Ok(message)
}

#[tauri::command]
async fn list_recordings(app: AppHandle, state: State<'_, AppState>) -> Result<Vec<library::Item>, String> {
    let folder = state.settings.lock().unwrap().folder.clone();
    let thumbs = app.path().app_cache_dir().map_err(|error| error.to_string())?.join(THUMBS_DIR);
    let cache = library_path(&app);

    // Thumbnails take AVFoundation time; keep them off the async runtime.
    tauri::async_runtime::spawn_blocking(move || library::scan(&library::Paths { folder: &folder, cache: &cache, thumbs: &thumbs }))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
fn rename_recording(app: AppHandle, state: State<AppState>, path: PathBuf, name: String) -> Result<PathBuf, String> {
    let renamed = library::rename(&library_path(&app), &path, &name)?;

    // A queued upload follows the file to its new name.
    let queued = state.uploads.jobs().iter().any(|job| job.path == path);
    if queued {
        state.uploads.cancel(&path);
        state.uploads.enqueue(&renamed);
    }
    library_changed(&app);
    Ok(renamed)
}

#[tauri::command]
fn delete_recording(app: AppHandle, state: State<AppState>, path: PathBuf) -> Result<(), String> {
    state.uploads.cancel(&path);
    library::trash(&path)?;
    library_changed(&app);
    Ok(())
}

/// Opens any library recording in the editor. Old recordings have no click log, so zoom has no targets.
#[tauri::command]
fn edit_recording(app: AppHandle, state: State<AppState>, path: PathBuf) -> Result<(), String> {
    let facts = media::probe(&path).ok_or("This file cannot be edited")?;
    *state.session.lock().unwrap() = Some(Session { path, width: facts.width, height: facts.height, clicks: Vec::new() });
    windows::open_editor(&app)
}

#[tauri::command]
fn upload_recording(state: State<AppState>, path: PathBuf) {
    state.uploads.enqueue(&path);
}

#[tauri::command]
fn cancel_upload(state: State<AppState>, path: PathBuf) {
    state.uploads.cancel(&path);
}

#[tauri::command]
fn upload_jobs(state: State<AppState>) -> Vec<queue::Job> {
    state.uploads.jobs()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DriveStatus {
    available: bool,
    connected: bool,
    account: Option<String>,
    upload: UploadPolicy,
}

/// Async because the account email may need one network call after sign-in.
#[tauri::command]
async fn drive_status(state: State<'_, AppState>) -> Result<DriveStatus, String> {
    let settings = state.settings.lock().unwrap().clone();
    let client = drive_client(&settings);
    let connected = drive::is_connected();
    let lookup = client.clone();
    let account = match connected {
        true => tauri::async_runtime::spawn_blocking(move || drive::account(&lookup)).await.unwrap_or(None),
        false => None,
    };
    Ok(DriveStatus { available: client.is_set(), connected, account, upload: settings.upload })
}

#[tauri::command]
async fn drive_connect(state: State<'_, AppState>) -> Result<(), String> {
    let client = drive_client(&state.settings.lock().unwrap());
    tauri::async_runtime::spawn_blocking(move || drive::connect(&client)).await.map_err(|error| error.to_string())?
}

#[tauri::command]
fn drive_disconnect() {
    drive::disconnect();
}

#[tauri::command]
fn set_upload_policy(app: AppHandle, policy: UploadPolicy) {
    update_settings(&app, |settings| settings.upload = policy);
}

/// Puts text on the clipboard through pbcopy; the webview clipboard API needs a user gesture it may not see.
#[tauri::command]
fn copy_text(text: String) -> Result<(), String> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn().map_err(|error| error.to_string())?;
    child.stdin.take().ok_or("pbcopy has no input")?.write_all(text.as_bytes()).map_err(|error| error.to_string())?;
    child.wait().map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn reveal(path: String) {
    let _ = Command::new("open").arg("-R").arg(path).status();
}

#[tauri::command]
fn show_menu(app: AppHandle, state: State<AppState>) -> Result<(), String> {
    let settings = state.settings.lock().unwrap().clone();
    let menu = build_menu(&app, &settings).map_err(|error| error.to_string())?;
    let toolbar = app.get_webview_window(windows::TOOLBAR).ok_or("Toolbar missing")?;
    toolbar.popup_menu(&menu).map_err(|error| error.to_string())
}

fn build_menu(app: &AppHandle, settings: &Settings) -> tauri::Result<Menu<tauri::Wry>> {
    let check = |id: &str, title: &str, checked: bool| CheckMenuItem::with_id(app, id, title, true, checked, None::<&str>);
    Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "toggle-toolbar", "Show / Hide Toolbar", true, None::<&str>)?,
            &MenuItem::with_id(app, "open-dashboard", "Open Dashboard…", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &check("target-folder", "Save to Folder", settings.target == Target::Folder)?,
            &check("target-clipboard", "Copy to Clipboard", settings.target == Target::Clipboard)?,
            &check("target-both", "Save and Copy", settings.target == Target::Both)?,
            &PredefinedMenuItem::separator(app)?,
            &check("show-clicks", "Show Clicks", settings.show_clicks)?,
            &check("show-keys", "Show Keystrokes (needs Accessibility)", settings.show_keys)?,
            &check("record-toolbar", "Show Toolbar in Recordings and Screen Share", settings.record_toolbar)?,
            &PredefinedMenuItem::separator(app)?,
            &check("record-mic", "Record Microphone", settings.record_mic)?,
            &check("record-system-audio", "Record System Audio", settings.record_system_audio)?,
            &PredefinedMenuItem::separator(app)?,
            &check("upload-ask", "Drive: Ask to Upload Each Recording", settings.upload == UploadPolicy::Ask)?,
            &check("upload-always", "Drive: Always Upload", settings.upload == UploadPolicy::Always)?,
            &check("upload-never", "Drive: Never Upload", settings.upload == UploadPolicy::Never)?,
            &PredefinedMenuItem::separator(app)?,
            &check("open-editor", "Open Editor After Recording", settings.open_editor)?,
            &MenuItem::with_id(app, "edit-last", "Edit Last Recording…", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "choose-folder", "Choose Folder…", true, None::<&str>)?,
            &MenuItem::with_id(app, "open-folder", "Open Folder", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "Quit TapeIt", true, None::<&str>)?,
        ],
    )
}

fn update_settings(app: &AppHandle, change: impl FnOnce(&mut Settings)) {
    let state = app.state::<AppState>();
    let mut settings = state.settings.lock().unwrap();
    change(&mut settings);
    apply_input_flags(&settings);
    let _ = settings.save(&settings_path(app));

    if let Ok(menu) = build_menu(app, &settings) {
        tray::set_menu(app, menu);
    }
}

#[tauri::command]
fn minimize(app: AppHandle) {
    tray::toggle_toolbar(&app);
}

fn on_menu(app: &AppHandle, event: MenuEvent) {
    match event.id().as_ref() {
        "toggle-toolbar" => tray::toggle_toolbar(app),
        "open-dashboard" => {
            if let Err(error) = windows::open_dashboard(app) {
                status(app, error);
            }
        }
        "target-folder" => update_settings(app, |settings| settings.target = Target::Folder),
        "target-clipboard" => update_settings(app, |settings| settings.target = Target::Clipboard),
        "target-both" => update_settings(app, |settings| settings.target = Target::Both),
        "show-clicks" => update_settings(app, |settings| settings.show_clicks = !settings.show_clicks),
        "show-keys" => update_settings(app, |settings| settings.show_keys = !settings.show_keys),
        "record-toolbar" => {
            update_settings(app, |settings| settings.record_toolbar = !settings.record_toolbar);
            let shared = app.state::<AppState>().settings.lock().unwrap().record_toolbar;
            windows::set_toolbar_shared(app, shared);
        }
        "record-mic" => toggle_mic(app.clone()).map(|_| ()).unwrap_or_default(),
        "record-system-audio" => toggle_system_audio(app.clone()).map(|_| ()).unwrap_or_default(),
        "upload-ask" => update_settings(app, |settings| settings.upload = UploadPolicy::Ask),
        "upload-always" => update_settings(app, |settings| settings.upload = UploadPolicy::Always),
        "upload-never" => update_settings(app, |settings| settings.upload = UploadPolicy::Never),
        "open-editor" => update_settings(app, |settings| settings.open_editor = !settings.open_editor),
        "edit-last" => {
            if app.state::<AppState>().session.lock().unwrap().is_none() {
                status(app, "Record something first");
                return;
            }
            if let Err(error) = windows::open_editor(app) {
                status(app, error);
            }
        }
        "choose-folder" => choose_folder(app),
        "open-folder" => {
            let folder = app.state::<AppState>().settings.lock().unwrap().folder.clone();
            let _ = std::fs::create_dir_all(&folder);
            let _ = Command::new("open").arg(folder).status();
        }
        "quit" => app.exit(0),
        _ => {}
    }
}

fn choose_folder(app: &AppHandle) {
    let handle = app.clone();
    app.dialog().file().pick_folder(move |picked| {
        let Some(folder) = picked.and_then(|path| path.into_path().ok()) else {
            return;
        };
        update_settings(&handle, |settings| settings.folder = folder);
    });
}

fn toggle_recording(app: &AppHandle) {
    let is_recording = app.state::<AppState>().recording.lock().unwrap().is_some();
    if !is_recording {
        request_recording(app, None);
        return;
    }

    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = stop_recording(handle.clone(), handle.state::<AppState>()).await {
            status(&handle, error);
        }
    });
}

/// Pushes the current mic and system audio choice into a running recording, if any.
fn apply_audio(app: &AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let mut slot = state.recording.lock().unwrap();
    let Some(recording) = slot.as_mut() else {
        return Ok(());
    };
    recording.set_audio(AudioSources {
        microphone: settings.record_mic,
        system: settings.record_system_audio,
    })
}

/// Flips the microphone setting and tells the toolbar, so its mic button stays in sync with the menu.
#[tauri::command]
fn toggle_mic(app: AppHandle) -> Result<bool, String> {
    update_settings(&app, |settings| settings.record_mic = !settings.record_mic);
    let on = app.state::<AppState>().settings.lock().unwrap().record_mic;
    log_line(&format!("toggle_mic -> {on}"));
    apply_audio(&app)?;
    app.emit("mic", on).map_err(|error| error.to_string())?;
    Ok(on)
}

#[tauri::command]
fn mic_enabled(state: State<AppState>) -> bool {
    state.settings.lock().unwrap().record_mic
}

/// Flips system audio and tells the toolbar, so its button stays in sync with the menu.
#[tauri::command]
fn toggle_system_audio(app: AppHandle) -> Result<bool, String> {
    update_settings(&app, |settings| settings.record_system_audio = !settings.record_system_audio);
    let on = app.state::<AppState>().settings.lock().unwrap().record_system_audio;
    log_line(&format!("toggle_system_audio -> {on}"));
    apply_audio(&app)?;
    app.emit("system-audio", on).map_err(|error| error.to_string())?;
    Ok(on)
}

#[tauri::command]
fn system_audio_enabled(state: State<AppState>) -> bool {
    state.settings.lock().unwrap().record_system_audio
}

/// The toolbar hides during capture so it never shows up in the screenshot; drawings stay.
const TOOLBAR_HIDE_MS: u64 = 200;

#[tauri::command]
async fn take_screenshot(app: AppHandle, region: Region) -> Result<(), String> {
    let toolbar = app.get_webview_window(windows::TOOLBAR).ok_or("Toolbar missing")?;
    toolbar.hide().map_err(|error| error.to_string())?;
    std::thread::sleep(std::time::Duration::from_millis(TOOLBAR_HIDE_MS));

    let cache = app.path().app_cache_dir().map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&cache).map_err(|error| error.to_string())?;
    let captured = screenshot::capture(&cache, region);
    let _ = toolbar.show();

    let Some(file) = captured? else {
        return Ok(());
    };
    let settings = app.state::<AppState>().settings.lock().unwrap().clone();
    let (message, _) = output::deliver(&file, &settings)?;
    status(&app, message);
    Ok(())
}

#[tauri::command]
fn toggle_camera(app: AppHandle) -> Result<bool, String> {
    let on = windows::toggle_camera(&app)?;

    // Turned on mid-recording: put the bubble inside the recorded area right away.
    let state = app.state::<AppState>();
    let recording = state.recording.lock().unwrap().is_some();
    if on && recording {
        windows::place_camera(&app, *state.recording_area.lock().unwrap());
    }
    app.emit("camera", on).map_err(|error| error.to_string())?;
    Ok(on)
}

#[tauri::command]
fn record(app: AppHandle) {
    toggle_recording(&app);
}

fn on_shortcut(app: &AppHandle, shortcut: &Shortcut) {
    let state = app.state::<AppState>();
    let current = *state.mode.lock().unwrap();

    match shortcut.key {
        Code::KeyR => toggle_recording(app),
        Code::KeyA => {
            let _ = set_mode(app.clone(), state, Mode::Select);
        }
        Code::KeyD => {
            let next = if current == Mode::Draw { Mode::Pointer } else { Mode::Draw };
            let _ = set_mode(app.clone(), state, next);
        }
        Code::KeyC => {
            let _ = app.emit("clear", ());
        }
        Code::KeyF => {
            let _ = app.emit("toggle-life", ());
        }
        Code::KeyT => tray::toggle_toolbar(app),
        Code::KeyW => {
            let _ = toggle_camera(app.clone());
        }
        Code::KeyS => {
            let _ = app.emit_to(windows::TOOLBAR, "toggle-spotlight", ());
        }
        _ => {}
    }
}

fn main() {
    let hotkeys = [Code::KeyR, Code::KeyA, Code::KeyD, Code::KeyC, Code::KeyF, Code::KeyS, Code::KeyW, Code::KeyT]
        .map(|key| Shortcut::new(Some(Modifiers::CONTROL | Modifiers::ALT), key));

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        on_shortcut(app, shortcut);
                    }
                })
                .build(),
        )
        .setup(move |app| {
            app.set_activation_policy(ActivationPolicy::Accessory);

            let handle = app.handle();
            let home = app.path().home_dir()?;
            let settings = Settings::load(&settings_path(handle), &home);
            apply_input_flags(&settings);
            let created = windows::create(handle)?;
            windows::set_toolbar_shared(handle, settings.record_toolbar);
            input::start(handle.clone(), created.screen_height);
            let screen = Area {
                x: 0.0,
                y: 0.0,
                width: created.screen_width,
                height: created.screen_height,
            };

            // The upload worker reads the OAuth client fresh per job, so pasting a client ID needs no restart.
            let config = app.path().app_config_dir()?;
            let uploads = Uploads::load(config.join(QUEUE_FILE), config.join(LIBRARY_FILE));
            let reader = handle.clone();
            let notifier = handle.clone();
            queue::start(
                uploads.clone(),
                move || drive_client(&reader.state::<AppState>().settings.lock().unwrap()),
                move |notice| match notice {
                    Notice::Progress(path, fraction) => {
                        let _ = notifier.emit("upload-progress", (path, fraction));
                    }
                    Notice::Changed => library_changed(&notifier),
                },
            );

            app.manage(AppState {
                uploads,
                recording: Mutex::new(None),
                recording_area: Mutex::new(screen),
                session: Mutex::new(None),
                screen,
                settings: Mutex::new(settings),
                mode: Mutex::new(Mode::Pointer),
                toolbar_id: created.toolbar_id,
                scale: created.scale,
            });

            let tray_menu = build_menu(handle, &app.state::<AppState>().settings.lock().unwrap())?;
            tray::create(handle, &tray_menu)?;

            for hotkey in hotkeys {
                app.global_shortcut().register(hotkey)?;
            }
            Ok(())
        })
        .on_menu_event(on_menu)
        .invoke_handler(tauri::generate_handler![
            set_mode,
            set_spotlight,
            record,
            toggle_mic,
            mic_enabled,
            toggle_system_audio,
            system_audio_enabled,
            take_screenshot,
            toggle_camera,
            start_recording,
            stop_recording,
            get_session,
            minimize,
            export_clip,
            reveal,
            show_menu,
            list_recordings,
            rename_recording,
            delete_recording,
            edit_recording,
            upload_recording,
            cancel_upload,
            upload_jobs,
            drive_status,
            drive_connect,
            drive_disconnect,
            set_upload_policy,
            copy_text
        ])
        .run(tauri::generate_context!())
        .expect("TapeIt failed to start");
}
