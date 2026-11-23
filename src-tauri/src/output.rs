use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Folder,
    Clipboard,
    Both,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub target: Target,
    pub folder: PathBuf,
    #[serde(default = "enabled")]
    pub show_clicks: bool,
    #[serde(default)]
    pub show_keys: bool,
    #[serde(default)]
    pub record_toolbar: bool,
    #[serde(default)]
    pub record_mic: bool,
    #[serde(default)]
    pub record_system_audio: bool,
    #[serde(default = "enabled")]
    pub open_editor: bool,
    #[serde(default)]
    pub drive_client_id: String,
    #[serde(default)]
    pub drive_client_secret: String,
    #[serde(default)]
    pub upload: UploadPolicy,
}

/// What happens to a new recording on Google Drive. The user picks; nothing uploads without a choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadPolicy {
    #[default]
    Ask,
    Always,
    Never,
}

fn enabled() -> bool {
    true
}

impl Settings {
    pub fn load(file: &Path, home: &Path) -> Self {
        fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| Settings {
                target: Target::Both,
                folder: home.join("Movies/TapeIt"),
                show_clicks: true,
                show_keys: false,
                record_toolbar: false,
                record_mic: false,
                record_system_audio: false,
                open_editor: true,
                drive_client_id: String::new(),
                drive_client_secret: String::new(),
                upload: UploadPolicy::Ask,
            })
    }

    /// Turns audio inputs off after a recording, so every recording starts silent by default.
    pub fn reset_inputs(&mut self) {
        self.record_mic = false;
        self.record_system_audio = false;
    }

    pub fn save(&self, file: &Path) -> Result<(), String> {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        fs::write(file, bytes).map_err(|error| error.to_string())
    }
}

pub fn file_name() -> String {
    format!("TapeIt {}.mov", chrono::Local::now().format("%Y-%m-%d at %H.%M.%S"))
}

pub fn export_name(extension: &str) -> String {
    format!("TapeIt {} (edited).{extension}", chrono::Local::now().format("%Y-%m-%d at %H.%M.%S"))
}

/// Moves the finished file to the folder and/or puts it on the clipboard.
/// Returns a status line and where the file ended up.
pub fn deliver(recording: &Path, settings: &Settings) -> Result<(String, PathBuf), String> {
    let mut path = recording.to_path_buf();

    if settings.target != Target::Clipboard {
        fs::create_dir_all(&settings.folder).map_err(|error| error.to_string())?;
        let destination = settings.folder.join(recording.file_name().ok_or("Bad file name")?);
        if fs::rename(recording, &destination).is_err() {
            fs::copy(recording, &destination).map_err(|error| error.to_string())?;
            let _ = fs::remove_file(recording);
        }
        path = destination;
    }

    if settings.target != Target::Folder {
        copy_file(&path)?;
    }

    let message = match settings.target {
        Target::Folder => "Saved to folder",
        Target::Clipboard => "Copied to clipboard",
        Target::Both => "Saved and copied",
    };
    Ok((message.into(), path))
}

// The path goes in as argv, so no quoting is needed inside the AppleScript.
fn copy_file(path: &Path) -> Result<(), String> {
    let status = Command::new("osascript")
        .args(["-e", "on run argv", "-e", "set the clipboard to (POSIX file (item 1 of argv))", "-e", "end run"])
        .arg(path)
        .status()
        .map_err(|error| error.to_string())?;

    if !status.success() {
        return Err("Could not copy to clipboard".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mic or system audio turned on mid-recording must not leak into the next one.
    #[test]
    fn reset_turns_audio_off() {
        let mut settings = Settings::load(Path::new("/nonexistent"), Path::new("/tmp"));
        settings.record_mic = true;
        settings.record_system_audio = true;

        settings.reset_inputs();

        assert!(!settings.record_mic);
        assert!(!settings.record_system_audio);
    }
}
