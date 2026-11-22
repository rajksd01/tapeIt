use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use screencapturekit::prelude::*;
use screencapturekit::recording_output::{
    RecordingCallbacks, SCRecordingOutput, SCRecordingOutputCodec, SCRecordingOutputConfiguration,
    SCRecordingOutputFileType,
};
use serde::{Deserialize, Serialize};

const FRAMES_PER_SECOND: i32 = 60;
const FINISH_TIMEOUT: Duration = Duration::from_secs(15);

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGMainDisplayID() -> u32;
}

/// A region of the main display, in points, origin top-left.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

pub struct AudioSources {
    pub microphone: bool,
    pub system: bool,
}

pub struct Recording {
    stream: SCStream,
    _output: SCRecordingOutput,
    done: Receiver<Result<(), String>>,
    path: PathBuf,
}

fn text<E: std::fmt::Debug>(error: E) -> String {
    format!("{error:?}")
}

// Video encoders reject odd pixel sizes.
pub fn even(value: f64) -> u32 {
    (value.round() as u32) & !1
}

pub fn start(
    path: &Path,
    area: Option<Area>,
    scale: f64,
    hidden_windows: &[u32],
    audio: AudioSources,
) -> Result<Recording, String> {
    let content = SCShareableContent::get().map_err(text)?;
    let main_id = unsafe { CGMainDisplayID() };
    let display = content
        .displays()
        .into_iter()
        .find(|display| display.display_id() == main_id)
        .ok_or("Main display not found")?;

    let windows = content.windows();
    let hidden: Vec<&SCWindow> = windows.iter().filter(|window| hidden_windows.contains(&window.window_id())).collect();
    let filter = SCContentFilter::create()
        .with_display(&display)
        .with_excluding_windows(&hidden)
        .build()
        .map_err(text)?;

    let region = area.unwrap_or(Area {
        x: 0.0,
        y: 0.0,
        width: f64::from(display.width()),
        height: f64::from(display.height()),
    });
    let mut config = SCStreamConfiguration::new()
        .with_width(even(region.width * scale))
        .with_height(even(region.height * scale))
        .with_minimum_frame_interval(&CMTime::new(1, FRAMES_PER_SECOND))
        .with_shows_cursor(true)
        .with_captures_audio(audio.system)
        .with_excludes_current_process_audio(true)
        .with_captures_microphone(audio.microphone)
        .map_err(text)?;
    if area.is_some() {
        config = config.with_source_rect(CGRect::new(region.x, region.y, region.width, region.height));
    }

    let (finished, done) = channel();
    let failed = finished.clone();
    let callbacks = RecordingCallbacks::new()
        .on_finish(move || {
            let _ = finished.send(Ok(()));
        })
        .on_fail(move |error| {
            let _ = failed.send(Err(error));
        });

    let output_config = SCRecordingOutputConfiguration::new()
        .map_err(text)?
        .with_output_url(path)
        .map_err(text)?
        .with_video_codec(SCRecordingOutputCodec::HEVC)
        .with_output_file_type(SCRecordingOutputFileType::MOV);
    let output = SCRecordingOutput::new_with_delegate(&output_config, callbacks).ok_or("Recording needs macOS 15")?;

    let stream = SCStream::new(&filter, &config).map_err(text)?;
    stream.add_recording_output(&output).map_err(text)?;
    stream.start_capture().map_err(text)?;

    Ok(Recording {
        stream,
        _output: output,
        done,
        path: path.to_path_buf(),
    })
}

impl Recording {
    pub fn stop(self) -> Result<PathBuf, String> {
        self.stream.stop_capture().map_err(text)?;
        self.done
            .recv_timeout(FINISH_TIMEOUT)
            .map_err(|_| "The recording did not finish writing.".to_string())??;
        Ok(self.path)
    }
}
