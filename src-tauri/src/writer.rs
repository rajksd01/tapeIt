//! Writes captured frames to a movie with AVAssetWriter.
//!
//!   SCStream ──Screen──▶ video input (H.264)  ─┐
//!            ──Audio───▶ system input (AAC)    ├─▶ AVAssetWriter ─▶ .mov
//!            ──Mic─────▶ mic input (AAC)      ─┘
//!
//! All three tracks exist from the start, so mic and system audio can be turned on at any time.

use std::path::Path;
use std::sync::mpsc::channel;
use std::sync::Mutex;
use std::time::Duration;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, Message};
use objc2_av_foundation::{
    AVAssetWriter, AVAssetWriterInput, AVAssetWriterStatus, AVFileTypeQuickTimeMovie, AVMediaTypeAudio,
    AVMediaTypeVideo, AVVideoAverageBitRateKey, AVVideoCodecKey, AVVideoCodecTypeH264,
    AVVideoCompressionPropertiesKey, AVVideoExpectedSourceFrameRateKey, AVVideoHeightKey, AVVideoWidthKey,
};
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

// Screen content compresses well; 0.05 bits per pixel per frame keeps text sharp.
// Example: 2940 x 1912 at 60 fps -> about 17 Mbit/s.
const BITS_PER_PIXEL: f64 = 0.05;

// AVFAudio keys; Apple documents each key's value as its own name.
const FORMAT_ID_KEY: &str = "AVFormatIDKey";
const SAMPLE_RATE_KEY: &str = "AVSampleRateKey";
const CHANNELS_KEY: &str = "AVNumberOfChannelsKey";
const BIT_RATE_KEY: &str = "AVEncoderBitRateKey";
const AAC_FORMAT: u32 = u32::from_be_bytes(*b"aac ");
const SAMPLE_RATE: f64 = 48_000.0;
const CHANNELS: i32 = 2;
const AUDIO_BIT_RATE: i32 = 128_000;

const FINISH_TIMEOUT: Duration = Duration::from_secs(30);

/// Which captured stream a sample belongs to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Track {
    Video,
    System,
    Mic,
}

struct Inner {
    writer: Retained<AVAssetWriter>,
    video: Retained<AVAssetWriterInput>,
    system: Retained<AVAssetWriterInput>,
    mic: Retained<AVAssetWriterInput>,
    started: bool,
    failure: Option<String>,
}

/// Thread-safe front for one AVAssetWriter; capture callbacks arrive on SCStream queues.
pub struct Writer {
    inner: Mutex<Inner>,
}

// AVAssetWriter is used from one thread at a time: every call goes through the Mutex.
unsafe impl Send for Writer {}
unsafe impl Sync for Writer {}

fn key(text: &str) -> Retained<NSString> {
    NSString::from_str(text)
}

fn object(number: Retained<NSNumber>) -> Retained<AnyObject> {
    Retained::into_super(Retained::into_super(Retained::into_super(number)))
}

fn video_settings(width: u32, height: u32, fps: i32) -> Retained<NSDictionary<NSString, AnyObject>> {
    let bit_rate = (f64::from(width) * f64::from(height) * f64::from(fps) * BITS_PER_PIXEL) as i64;
    let compression: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_retained_objects(
        &[unsafe { AVVideoAverageBitRateKey.unwrap() }, unsafe { AVVideoExpectedSourceFrameRateKey.unwrap() }],
        &[object(NSNumber::new_i64(bit_rate)), object(NSNumber::new_i32(fps))],
    );
    let codec: Retained<AnyObject> = unsafe { Retained::into_super(Retained::into_super(AVVideoCodecTypeH264.unwrap().retain())) };

    NSDictionary::from_retained_objects(
        &[
            unsafe { AVVideoCodecKey.unwrap() },
            unsafe { AVVideoWidthKey.unwrap() },
            unsafe { AVVideoHeightKey.unwrap() },
            unsafe { AVVideoCompressionPropertiesKey.unwrap() },
        ],
        &[
            codec,
            object(NSNumber::new_u32(width)),
            object(NSNumber::new_u32(height)),
            Retained::into_super(Retained::into_super(compression)),
        ],
    )
}

fn audio_settings() -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys = [key(FORMAT_ID_KEY), key(SAMPLE_RATE_KEY), key(CHANNELS_KEY), key(BIT_RATE_KEY)];
    let key_refs: Vec<&NSString> = keys.iter().map(|key| &**key).collect();
    NSDictionary::from_retained_objects(
        &key_refs,
        &[
            object(NSNumber::new_u32(AAC_FORMAT)),
            object(NSNumber::new_f64(SAMPLE_RATE)),
            object(NSNumber::new_i32(CHANNELS)),
            object(NSNumber::new_i32(AUDIO_BIT_RATE)),
        ],
    )
}

fn input(writer: &AVAssetWriter, media: &NSString, settings: &NSDictionary<NSString, AnyObject>) -> Result<Retained<AVAssetWriterInput>, String> {
    let input = unsafe { AVAssetWriterInput::assetWriterInputWithMediaType_outputSettings(media, Some(settings)) };
    unsafe { input.setExpectsMediaDataInRealTime(true) };
    if !unsafe { writer.canAddInput(&input) } {
        return Err("Cannot add track to recording".into());
    }
    unsafe { writer.addInput(&input) };
    Ok(input)
}

fn describe(writer: &AVAssetWriter) -> String {
    unsafe { writer.error() }
        .map(|error| error.localizedDescription().to_string())
        .unwrap_or_else(|| "unknown error".into())
}

impl Writer {
    pub fn new(path: &Path, width: u32, height: u32, fps: i32) -> Result<Self, String> {
        let _ = std::fs::remove_file(path);
        let url = NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()));
        let writer = unsafe {
            AVAssetWriter::initWithURL_fileType_error(AVAssetWriter::alloc(), &url, AVFileTypeQuickTimeMovie.unwrap())
        }
        .map_err(|error| error.localizedDescription().to_string())?;

        let video = input(&writer, unsafe { AVMediaTypeVideo.unwrap() }, &video_settings(width, height, fps))?;
        let audio = audio_settings();
        let system = input(&writer, unsafe { AVMediaTypeAudio.unwrap() }, &audio)?;
        let mic = input(&writer, unsafe { AVMediaTypeAudio.unwrap() }, &audio)?;

        if !unsafe { writer.startWriting() } {
            return Err(describe(&writer));
        }
        Ok(Writer {
            inner: Mutex::new(Inner { writer, video, system, mic, started: false, failure: None }),
        })
    }

    /// Appends one captured sample. The session starts at the first video frame; audio before it is dropped.
    /// A full or failed input drops the sample instead of blocking the capture queue.
    pub fn append(&self, track: Track, sample: &CMSampleBuffer) {
        let mut inner = self.inner.lock().unwrap();
        if inner.failure.is_some() {
            return;
        }

        if !inner.started {
            if track != Track::Video {
                return;
            }
            unsafe { inner.writer.startSessionAtSourceTime(sample.presentation_time_stamp()) };
            inner.started = true;
        }

        let input = match track {
            Track::Video => &inner.video,
            Track::System => &inner.system,
            Track::Mic => &inner.mic,
        };
        if !unsafe { input.isReadyForMoreMediaData() } {
            return;
        }
        if unsafe { input.appendSampleBuffer(sample) } {
            return;
        }
        if unsafe { inner.writer.status() } == AVAssetWriterStatus::Failed {
            inner.failure = Some(describe(&inner.writer));
        }
    }

    /// Closes all tracks and waits for the file to be complete.
    pub fn finish(&self) -> Result<(), String> {
        let inner = self.inner.lock().unwrap();
        if let Some(failure) = &inner.failure {
            return Err(format!("Recording failed: {failure}"));
        }
        if !inner.started {
            return Err("Recording stopped before the first frame".into());
        }

        unsafe {
            inner.video.markAsFinished();
            inner.system.markAsFinished();
            inner.mic.markAsFinished();
        }
        let (finished, done) = channel();
        let block = RcBlock::new(move || {
            let _ = finished.send(());
        });
        unsafe { inner.writer.finishWritingWithCompletionHandler(&block) };
        done.recv_timeout(FINISH_TIMEOUT).map_err(|_| "The recording did not finish writing.".to_string())?;

        if unsafe { inner.writer.status() } != AVAssetWriterStatus::Completed {
            return Err(format!("Recording failed: {}", describe(&inner.writer)));
        }
        Ok(())
    }
}
