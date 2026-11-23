use std::path::{Path, PathBuf};
use std::sync::Arc;

use screencapturekit::cm::{CMSampleBuffer, CMSampleBufferSCExt, SCFrameStatus};
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::pixel_format::PixelFormat;
use serde::{Deserialize, Serialize};

use crate::writer::{Track, Writer};

const FRAMES_PER_SECOND: i32 = 60;
// Matches the writer's AAC settings, so audio needs no resampling.
const AUDIO_SAMPLE_RATE: i32 = 48_000;
const AUDIO_CHANNELS: i32 = 2;

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
    config: SCStreamConfiguration,
    writer: Arc<Writer>,
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
        .with_pixel_format(PixelFormat::YCbCr_420v)
        .with_sample_rate(AUDIO_SAMPLE_RATE)
        .with_channel_count(AUDIO_CHANNELS)
        .with_captures_audio(audio.system)
        .with_excludes_current_process_audio(true)
        .with_captures_microphone(audio.microphone)
        .map_err(text)?;
    if area.is_some() {
        config = config.with_source_rect(CGRect::new(region.x, region.y, region.width, region.height));
    }

    let writer = Arc::new(Writer::new(path, even(region.width * scale), even(region.height * scale), FRAMES_PER_SECOND)?);

    // Every stream feeds the writer; audio streams only deliver while their source is on.
    let mut stream = SCStream::new(&filter, &config).map_err(text)?;
    for (kind, track) in [
        (SCStreamOutputType::Screen, Track::Video),
        (SCStreamOutputType::Audio, Track::System),
        (SCStreamOutputType::Microphone, Track::Mic),
    ] {
        let sink = Arc::clone(&writer);
        stream
            .add_output_handler(move |sample: CMSampleBuffer, _kind: SCStreamOutputType| feed(&sink, track, &sample), kind)
            .map_err(text)?;
    }
    stream.start_capture().map_err(text)?;

    Ok(Recording {
        stream,
        config,
        writer,
        path: path.to_path_buf(),
    })
}

/// Hands one sample to the writer. Screen samples without a new frame (idle, blank) are skipped.
fn feed(writer: &Writer, track: Track, sample: &CMSampleBuffer) {
    if track == Track::Video && sample.frame_status() != Some(SCFrameStatus::Complete) {
        return;
    }
    // Same CoreMedia object, seen through the AVFoundation bindings.
    let sample = unsafe { &*(sample.as_ptr() as *const objc2_core_media::CMSampleBuffer) };
    writer.append(track, sample);
}

impl Recording {
    /// Turns mic and system audio on or off while the recording runs.
    pub fn set_audio(&mut self, audio: AudioSources) -> Result<(), String> {
        self.config.set_captures_audio(audio.system);
        self.config.set_captures_microphone(audio.microphone).map_err(text)?;
        self.stream.update_configuration(&self.config).map_err(text)
    }

    pub fn stop(self) -> Result<PathBuf, String> {
        self.stream.stop_capture().map_err(text)?;
        self.writer.finish()?;
        Ok(self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    const RECORD_SECONDS: u64 = 3;

    fn has_audio_track(file: &Path) -> bool {
        let output = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "a", "-show_entries", "stream=codec_type", "-of", "csv=p=0"])
            .arg(file)
            .output()
            .expect("ffprobe runs");
        String::from_utf8_lossy(&output.stdout).contains("audio")
    }

    // Needs Screen Recording permission for the terminal: cargo test -- --ignored system_audio
    #[test]
    #[ignore]
    fn system_audio_is_recorded() {
        let path = std::env::temp_dir().join("tapeit-audio-test.mov");
        let _ = std::fs::remove_file(&path);

        let recording = start(&path, None, 1.0, &[], AudioSources { microphone: false, system: true }).expect("starts");
        let _ = Command::new("afplay").arg("/System/Library/Sounds/Glass.aiff").status();
        std::thread::sleep(Duration::from_secs(RECORD_SECONDS));
        let file = recording.stop().expect("stops");

        assert!(has_audio_track(&file), "no audio track in {}", file.display());
    }

    // Turning sound on mid-recording must reach the file: cargo test -- --ignored mid_recording
    #[test]
    #[ignore]
    fn audio_turned_on_mid_recording() {
        let path = std::env::temp_dir().join("tapeit-audio-mid-test.mov");
        let _ = std::fs::remove_file(&path);

        let mut recording = start(&path, None, 1.0, &[], AudioSources { microphone: false, system: false }).expect("starts");
        std::thread::sleep(Duration::from_secs(1));
        recording.set_audio(AudioSources { microphone: false, system: true }).expect("updates");
        let _ = Command::new("afplay").arg("/System/Library/Sounds/Glass.aiff").status();
        std::thread::sleep(Duration::from_secs(RECORD_SECONDS));
        let file = recording.stop().expect("stops");

        assert!(has_audio_track(&file), "no audio track in {}", file.display());
    }
}
