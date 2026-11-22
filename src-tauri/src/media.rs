//! Reads video facts and poster frames with AVFoundation + ImageIO, so the library needs no bundled tools.

use std::path::Path;

use objc2::rc::Retained;
use objc2_av_foundation::{AVAssetImageGenerator, AVAssetTrack, AVMediaTypeVideo, AVURLAsset};
use objc2_core_foundation::{CFString, CFURL, CGSize};
use objc2_core_media::CMTime;
use objc2_foundation::{NSString, NSURL};
use objc2_image_io::CGImageDestination;
use serde::{Deserialize, Serialize};

const THUMB_WIDTH: f64 = 480.0;
const JPEG_TYPE: &str = "public.jpeg";
const TIMESCALE: i32 = 600;
// Frame 0 is often black (the countdown just ended); half a second in shows real content.
const POSTER_SECONDS: f64 = 0.5;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Facts {
    pub seconds: f64,
    pub width: u32,
    pub height: u32,
}

fn asset(path: &Path) -> Retained<AVURLAsset> {
    let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
    unsafe { AVURLAsset::URLAssetWithURL_options(&url, None) }
}

#[allow(deprecated)]
fn video_track(asset: &AVURLAsset) -> Option<Retained<AVAssetTrack>> {
    let media_type = unsafe { AVMediaTypeVideo }?;
    unsafe { asset.tracksWithMediaType(media_type) }.firstObject()
}

/// Duration and pixel size; None when the file is not a readable video.
#[allow(deprecated)]
pub fn probe(path: &Path) -> Option<Facts> {
    let asset = asset(path);
    let track = video_track(&asset)?;
    let size = unsafe { track.naturalSize() };
    let seconds = unsafe { asset.duration().seconds() };

    if !seconds.is_finite() {
        return None;
    }
    Some(Facts { seconds, width: size.width as u32, height: size.height as u32 })
}

/// Writes a JPEG poster frame for the video at `path` to `output`.
#[allow(deprecated)]
pub fn thumbnail(path: &Path, output: &Path) -> Result<(), String> {
    let asset = asset(path);
    let generator = unsafe { AVAssetImageGenerator::assetImageGeneratorWithAsset(&asset) };
    unsafe {
        generator.setAppliesPreferredTrackTransform(true);
        generator.setMaximumSize(CGSize { width: THUMB_WIDTH, height: 0.0 });
    }

    // Short clips: clamp the poster time inside the video.
    let seconds = unsafe { asset.duration().seconds() };
    let poster = POSTER_SECONDS.min(seconds / 2.0).max(0.0);
    let at = unsafe { CMTime::new((poster * TIMESCALE as f64) as i64, TIMESCALE) };
    let image = unsafe { generator.copyCGImageAtTime_actualTime_error(at, std::ptr::null_mut()) }
        .map_err(|error| format!("Could not read a frame: {}", error.localizedDescription()))?;

    let url = NSURL::fileURLWithPath(&NSString::from_str(&output.to_string_lossy()));
    let cf_url = unsafe { &*(Retained::as_ptr(&url).cast::<CFURL>()) };
    let jpeg = NSString::from_str(JPEG_TYPE);
    let cf_type = unsafe { &*(Retained::as_ptr(&jpeg).cast::<CFString>()) };
    let destination = unsafe { CGImageDestination::with_url(cf_url, cf_type, 1, None) }.ok_or("Could not create the thumbnail")?;

    unsafe { destination.add_image(&image, None) };
    if !unsafe { destination.finalize() } {
        return Err("Could not write the thumbnail".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    // Test fixture only: Homebrew ffmpeg makes a sample clip. The app never calls it.
    const FFMPEG: &str = "/opt/homebrew/bin/ffmpeg";

    #[test]
    fn probes_and_thumbnails_a_real_video() {
        let folder = std::env::temp_dir().join("screenpen-media-test");
        std::fs::create_dir_all(&folder).unwrap();
        let input = folder.join("clip.mov");
        let made = Command::new(FFMPEG)
            .args(["-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=s=640x360:r=30:d=3", "-c:v", "libx264", "-pix_fmt", "yuv420p"])
            .arg(&input)
            .status()
            .unwrap();
        assert!(made.success());

        let facts = probe(&input).unwrap();
        assert_eq!((facts.width, facts.height), (640, 360));
        assert!((facts.seconds - 3.0).abs() < 0.1, "{}", facts.seconds);

        let thumb = folder.join("thumb.jpg");
        let _ = std::fs::remove_file(&thumb);
        thumbnail(&input, &thumb).unwrap();
        let bytes = std::fs::read(&thumb).unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG magic");

        assert!(probe(&folder.join("missing.mov")).is_none());
    }
}
