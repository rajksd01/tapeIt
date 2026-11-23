use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// What part of the screen a screenshot covers.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    Full,
    Area,
}

pub fn file_name() -> String {
    format!("TapeIt Screenshot {}.png", chrono::Local::now().format("%Y-%m-%d at %H.%M.%S"))
}

/// Captures the screen with macOS `screencapture`, which also gives the native area picker.
/// Returns None when the person presses Esc in the area picker.
pub fn capture(directory: &Path, region: Region) -> Result<Option<PathBuf>, String> {
    let path = directory.join(file_name());

    // -x: no shutter sound. -i: drag to pick an area, Space to pick a window.
    let mut command = Command::new("screencapture");
    command.arg("-x");
    if let Region::Area = region {
        command.arg("-i");
    }

    let status = command.arg(&path).status().map_err(|error| error.to_string())?;
    if !status.success() {
        return Err("Screenshot failed".into());
    }
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(path))
}
