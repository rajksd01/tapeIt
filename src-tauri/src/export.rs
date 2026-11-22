//! Trim, MP4 and GIF export on Apple frameworks only (AVFoundation + ImageIO); no bundled binaries.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::AnyThread;
use objc2_av_foundation::{
    AVAssetExportPresetHighestQuality, AVAssetExportSession, AVAssetExportSessionStatus, AVAssetImageGenerator,
    AVAssetTrack, AVFileTypeMPEG4, AVMediaTypeVideo, AVMutableVideoComposition, AVMutableVideoCompositionInstruction,
    AVMutableVideoCompositionLayerInstruction, AVURLAsset, AVVideoCompositionInstructionProtocol,
    AVVideoCompositionLayerInstruction,
};
use objc2_core_foundation::{CFDictionary, CFString, CFURL, CGAffineTransform, CGSize};
use objc2_core_media::{CMTime, CMTimeRange};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSURL};
use objc2_image_io::{
    kCGImagePropertyGIFDelayTime, kCGImagePropertyGIFDictionary, kCGImagePropertyGIFLoopCount, CGImageDestination,
};
use serde::{Deserialize, Serialize};

const TIMESCALE: i32 = 600;
const PROGRESS_POLL: Duration = Duration::from_millis(100);
const COMPOSITION_FPS: i32 = 60;

const ZOOM_LEVEL: f64 = 2.0;
const ZOOM_LEAD_SECONDS: f64 = 0.5;
const ZOOM_HOLD_SECONDS: f64 = 1.6;
const ZOOM_RAMP_SECONDS: f64 = 0.45;
// AVFoundation ramps are linear; several short ramps approximate an eased curve.
const RAMP_STEPS: usize = 8;

const GIF_FPS: f64 = 15.0;
const GIF_MAX_WIDTH: f64 = 960.0;
const GIF_TYPE: &str = "com.compuserve.gif";

/// A click during the recording: seconds from the start, position as a 0..1 fraction of the frame.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Click {
    pub seconds: f64,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    Mp4,
    Gif,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Mp4 => "mp4",
            Format::Gif => "gif",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub start: f64,
    pub end: f64,
    pub format: Format,
    pub zoom: bool,
}

pub struct Source<'a> {
    pub input: &'a Path,
    pub clicks: &'a [Click],
}

/// Camera state: zoom factor and the 0..1 point it centres on.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pose {
    zoom: f64,
    x: f64,
    y: f64,
}

const WIDE: Pose = Pose { zoom: 1.0, x: 0.5, y: 0.5 };

/// A move from one pose to another over [start, end] seconds.
#[derive(Debug, Clone, Copy)]
struct Move {
    start: f64,
    end: f64,
    from: Pose,
    to: Pose,
}

fn time(seconds: f64) -> CMTime {
    unsafe { CMTime::with_seconds(seconds, TIMESCALE) }
}

fn range(start: f64, duration: f64) -> CMTimeRange {
    CMTimeRange { start: time(start), duration: time(duration) }
}

fn smoothstep(value: f64) -> f64 {
    value * value * (3.0 - 2.0 * value)
}

fn blend(from: Pose, to: Pose, amount: f64) -> Pose {
    Pose {
        zoom: from.zoom + (to.zoom - from.zoom) * amount,
        x: from.x + (to.x - from.x) * amount,
        y: from.y + (to.y - from.y) * amount,
    }
}

/// Clicks close together share one zoom: the camera pans between them instead of zooming out and in.
fn moves(clicks: &[Click], duration: f64) -> Vec<Move> {
    let mut sorted = clicks.to_vec();
    sorted.sort_by(|left, right| left.seconds.total_cmp(&right.seconds));

    let mut result: Vec<Move> = Vec::new();
    let mut current = WIDE;
    let mut cursor: f64 = 0.0;

    for (index, click) in sorted.iter().enumerate() {
        let target = Pose { zoom: ZOOM_LEVEL, x: click.x, y: click.y };
        let start = (click.seconds - ZOOM_LEAD_SECONDS).max(cursor);
        let end = (start + ZOOM_RAMP_SECONDS).min(duration);
        if end > start {
            result.push(Move { start, end, from: current, to: target });
        }
        current = target;
        cursor = end;

        let next_start = sorted.get(index + 1).map(|next| next.seconds - ZOOM_LEAD_SECONDS);
        let release = click.seconds + ZOOM_HOLD_SECONDS;
        let keeps_zoom = next_start.is_some_and(|next| next < release);
        if keeps_zoom {
            continue;
        }

        let out_start = (release - ZOOM_RAMP_SECONDS).max(cursor);
        let out_end = release.min(duration);
        if out_end > out_start {
            result.push(Move { start: out_start, end: out_end, from: current, to: WIDE });
        }
        current = WIDE;
        cursor = out_end.max(cursor);
    }
    result
}

/// Scales the frame and shifts it so the pose centre sits mid-frame, never showing past the edges.
fn transform(pose: Pose, size: CGSize) -> CGAffineTransform {
    let shift = |centre: f64, length: f64| (length / 2.0 - pose.zoom * centre * length).clamp(length - pose.zoom * length, 0.0);
    CGAffineTransform {
        a: pose.zoom,
        b: 0.0,
        c: 0.0,
        d: pose.zoom,
        tx: shift(pose.x, size.width),
        ty: shift(pose.y, size.height),
    }
}

#[allow(deprecated)]
fn video_track(asset: &AVURLAsset) -> Result<Retained<AVAssetTrack>, String> {
    let media_type = unsafe { AVMediaTypeVideo }.ok_or("Video media type missing")?;
    unsafe { asset.tracksWithMediaType(media_type) }
        .firstObject()
        .ok_or_else(|| "The recording has no video track".to_string())
}

#[allow(deprecated)]
fn zoom_composition(asset: &AVURLAsset, clicks: &[Click]) -> Result<Retained<AVMutableVideoComposition>, String> {
    let track = video_track(asset)?;
    let size = unsafe { track.naturalSize() };
    let duration = unsafe { asset.duration().seconds() };

    let layer = unsafe { AVMutableVideoCompositionLayerInstruction::videoCompositionLayerInstructionWithAssetTrack(&track) };
    unsafe { layer.setTransform_atTime(transform(WIDE, size), time(0.0)) };
    for change in moves(clicks, duration) {
        let step = (change.end - change.start) / RAMP_STEPS as f64;
        for index in 0..RAMP_STEPS {
            let from = blend(change.from, change.to, smoothstep(index as f64 / RAMP_STEPS as f64));
            let to = blend(change.from, change.to, smoothstep((index + 1) as f64 / RAMP_STEPS as f64));
            let span = range(change.start + step * index as f64, step);
            unsafe { layer.setTransformRampFromStartTransform_toEndTransform_timeRange(transform(from, size), transform(to, size), span) };
        }
    }

    let instruction = unsafe { AVMutableVideoCompositionInstruction::videoCompositionInstruction() };
    unsafe {
        instruction.setTimeRange(range(0.0, duration));
        let layers: Retained<NSArray<AVVideoCompositionLayerInstruction>> = NSArray::from_retained_slice(&[Retained::into_super(layer)]);
        instruction.setLayerInstructions(&layers);
    }

    let composition = unsafe { AVMutableVideoComposition::videoComposition() };
    unsafe {
        composition.setRenderSize(size);
        composition.setFrameDuration(CMTime::new(1, COMPOSITION_FPS));
        let protocol: Retained<ProtocolObject<dyn AVVideoCompositionInstructionProtocol>> = ProtocolObject::from_retained(instruction);
        composition.setInstructions(&NSArray::from_retained_slice(&[protocol]));
    }
    Ok(composition)
}

fn file_url(path: &Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn export_mp4(
    asset: &AVURLAsset,
    composition: Option<&AVMutableVideoComposition>,
    request: &Request,
    output: &Path,
    progress: &mut impl FnMut(f64),
) -> Result<(), String> {
    let session = unsafe {
        AVAssetExportSession::initWithAsset_presetName(AVAssetExportSession::alloc(), asset, AVAssetExportPresetHighestQuality)
    }
    .ok_or("This recording cannot be exported")?;

    let _ = std::fs::remove_file(output);
    unsafe {
        session.setOutputURL(Some(&file_url(output)));
        session.setOutputFileType(AVFileTypeMPEG4);
        session.setTimeRange(range(request.start, request.end - request.start));
        session.setShouldOptimizeForNetworkUse(true);
        if let Some(composition) = composition {
            session.setVideoComposition(Some(composition));
        }
    }

    let (finished, done) = channel();
    let block = RcBlock::new(move || {
        let _ = finished.send(());
    });
    unsafe { session.exportAsynchronouslyWithCompletionHandler(&block) };

    loop {
        match done.recv_timeout(PROGRESS_POLL) {
            Ok(()) => break,
            Err(RecvTimeoutError::Timeout) => progress(f64::from(unsafe { session.progress() })),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    if unsafe { session.status() } == AVAssetExportSessionStatus::Completed {
        return Ok(());
    }
    let reason = unsafe { session.error() }
        .map(|error| error.localizedDescription().to_string())
        .unwrap_or_else(|| "unknown error".into());
    Err(format!("Export failed: {reason}"))
}

/// NSDictionary is toll-free bridged to CFDictionary; ImageIO wants the CF type.
fn cf_dictionary(dictionary: &NSDictionary<NSString, AnyObject>) -> &CFDictionary {
    unsafe { &*(dictionary as *const NSDictionary<NSString, AnyObject>).cast::<CFDictionary>() }
}

fn ns_key(key: &CFString) -> &NSString {
    unsafe { &*(key as *const CFString).cast::<NSString>() }
}

fn gif_properties(key: &CFString, value: Retained<NSNumber>) -> Retained<NSDictionary<NSString, AnyObject>> {
    let inner: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_retained_objects(&[ns_key(key)], &[Retained::into_super(Retained::into_super(Retained::into_super(value)))]);
    NSDictionary::from_retained_objects(&[ns_key(unsafe { kCGImagePropertyGIFDictionary })], &[Retained::into_super(Retained::into_super(inner))])
}

#[allow(deprecated)]
fn export_gif(
    asset: &AVURLAsset,
    composition: Option<&AVMutableVideoComposition>,
    request: &Request,
    output: &Path,
    progress: &mut impl FnMut(f64),
) -> Result<(), String> {
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(asset) };
    unsafe {
        generator.setAppliesPreferredTrackTransform(true);
        generator.setMaximumSize(CGSize { width: GIF_MAX_WIDTH, height: 0.0 });
        generator.setRequestedTimeToleranceBefore(time(0.0));
        generator.setRequestedTimeToleranceAfter(time(0.0));
        if let Some(composition) = composition {
            generator.setVideoComposition(Some(composition));
        }
    }

    let frames = (((request.end - request.start) * GIF_FPS).floor() as usize).max(1);
    let url = file_url(output);
    let cf_url = unsafe { &*(Retained::as_ptr(&url).cast::<CFURL>()) };
    let gif_type = NSString::from_str(GIF_TYPE);
    let cf_type = unsafe { &*(Retained::as_ptr(&gif_type).cast::<CFString>()) };
    let destination = unsafe { CGImageDestination::with_url(cf_url, cf_type, frames, None) }.ok_or("Could not create the GIF file")?;

    let looping = gif_properties(unsafe { kCGImagePropertyGIFLoopCount }, NSNumber::new_i32(0));
    let delay = gif_properties(unsafe { kCGImagePropertyGIFDelayTime }, NSNumber::new_f64(1.0 / GIF_FPS));
    unsafe { destination.set_properties(Some(cf_dictionary(&looping))) };

    for index in 0..frames {
        let at = time(request.start + index as f64 / GIF_FPS);
        let image = unsafe { generator.copyCGImageAtTime_actualTime_error(at, std::ptr::null_mut()) }
            .map_err(|error| format!("Could not read a frame: {}", error.localizedDescription()))?;
        unsafe { destination.add_image(&image, Some(cf_dictionary(&delay))) };
        progress((index + 1) as f64 / frames as f64);
    }

    if !unsafe { destination.finalize() } {
        return Err("Could not write the GIF file".into());
    }
    Ok(())
}

/// Exports the requested range; reports progress as a 0..1 fraction.
pub fn run(source: &Source, request: &Request, output: &Path, mut progress: impl FnMut(f64)) -> Result<PathBuf, String> {
    let asset = unsafe { AVURLAsset::URLAssetWithURL_options(&file_url(source.input), None) };
    let composition = if request.zoom && !source.clicks.is_empty() {
        Some(zoom_composition(&asset, source.clicks)?)
    } else {
        None
    };

    match request.format {
        Format::Mp4 => export_mp4(&asset, composition.as_deref(), request, output, &mut progress)?,
        Format::Gif => export_gif(&asset, composition.as_deref(), request, output, &mut progress)?,
    }
    Ok(output.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    // Test fixtures only: Homebrew ffmpeg makes a sample clip and ffprobe reads results. The app never calls them.
    const FFMPEG: &str = "/opt/homebrew/bin/ffmpeg";
    const FFPROBE: &str = "/opt/homebrew/bin/ffprobe";

    #[test]
    fn far_apart_clicks_zoom_in_and_out_separately() {
        let clicks = [Click { seconds: 2.0, x: 0.2, y: 0.2 }, Click { seconds: 6.0, x: 0.8, y: 0.8 }];
        let result = moves(&clicks, 10.0);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].from, WIDE);
        assert_eq!(result[1].to, WIDE);
        assert!(result.windows(2).all(|pair| pair[0].end <= pair[1].start));
    }

    #[test]
    fn close_clicks_pan_without_zooming_out() {
        let clicks = [Click { seconds: 2.0, x: 0.2, y: 0.2 }, Click { seconds: 2.8, x: 0.8, y: 0.8 }];
        let result = moves(&clicks, 10.0);
        assert_eq!(result.len(), 3);
        assert_eq!(result[1].from.zoom, ZOOM_LEVEL);
        assert_eq!(result[1].to.x, 0.8);
        assert_eq!(result[2].to, WIDE);
    }

    #[test]
    fn transform_never_shows_past_the_frame_edge() {
        let size = CGSize { width: 1000.0, height: 500.0 };
        let corner = transform(Pose { zoom: 2.0, x: 1.0, y: 0.0 }, size);
        assert_eq!(corner.tx, -1000.0);
        assert_eq!(corner.ty, 0.0);
        assert_eq!(transform(WIDE, size).tx, 0.0);
    }

    fn probe(path: &Path) -> String {
        let output = Command::new(FFPROBE)
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height:format=duration", "-of", "csv=p=0"])
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().replace('\n', ",")
    }

    #[test]
    fn exports_trimmed_zoomed_mp4_and_gif() {
        let folder = std::env::temp_dir().join("screenpen-export-test");
        std::fs::create_dir_all(&folder).unwrap();
        let input = folder.join("input.mov");
        let made = Command::new(FFMPEG)
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=4", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(made.success());

        let clicks = [Click { seconds: 2.0, x: 0.8, y: 0.2 }];
        let source = Source { input: &input, clicks: &clicks };
        let mut last_progress = 0.0;

        let mp4 = folder.join("out.mp4");
        let request = Request { start: 1.0, end: 3.0, format: Format::Mp4, zoom: true };
        run(&source, &request, &mp4, |fraction| last_progress = fraction).unwrap();
        let probed = probe(&mp4);
        assert!(probed.starts_with("640,360,"), "{probed}");
        let seconds: f64 = probed.rsplit(',').next().unwrap().parse().unwrap();
        assert!((seconds - 2.0).abs() < 0.1, "duration {seconds}");
        assert!(last_progress >= 0.0);

        let gif = folder.join("out.gif");
        let request = Request { start: 0.0, end: 2.0, format: Format::Gif, zoom: true };
        run(&source, &request, &gif, |_| {}).unwrap();
        assert!(std::fs::metadata(&gif).unwrap().len() > 1000);
    }
}
