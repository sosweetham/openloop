//! Hardware video decode into an `AImageReader` whose images the renderer samples directly.
//!
//! An encoded frame is never discarded for want of a decoder buffer: it waits. Only a backlog
//! that would add visible latency is cut, and then the stream skips to the next keyframe so
//! the reference chain is never broken.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use ndk::hardware_buffer::HardwareBufferUsage;
use ndk::media::image_reader::{AcquireResult, Image, ImageFormat, ImageReader};
use ndk::media::media_codec::{
    DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
    MediaFormat,
};
use ndk::native_window::NativeWindow;

/// Frames queued beyond this are latency nobody wants; drop to the next keyframe instead.
const MAX_BACKLOG: usize = 8;

struct Encoded {
    index: u64,
    keyframe: bool,
    data: Vec<u8>,
}

pub struct Decoder {
    input: Sender<Encoded>,
    reader: ImageReader,
    /// Set when the client should ask the Mac for a keyframe: at start and after a skip.
    pub needs_keyframe: Arc<AtomicBool>,
}

impl Decoder {
    pub fn new(width: u32, height: u32, hevc: bool) -> Result<Self, String> {
        let reader = ImageReader::new_with_usage(
            width as i32,
            height as i32,
            ImageFormat::PRIVATE,
            HardwareBufferUsage::GPU_SAMPLED_IMAGE,
            4,
        )
        .map_err(|e| format!("image reader: {e:?}"))?;
        let window = reader
            .window()
            .map_err(|e| format!("image reader window: {e:?}"))?;
        let (input, rx) = mpsc::channel::<Encoded>();
        let (ready_tx, ready) = mpsc::channel();
        let needs_keyframe = Arc::new(AtomicBool::new(true));
        let flag = needs_keyframe.clone();
        // MediaCodec is not Send, so it is created and owned by its thread.
        thread::Builder::new()
            .name("openloop-decode".into())
            .spawn(move || match open(&window, width, height, hevc) {
                Ok(codec) => {
                    let _ = ready_tx.send(Ok(()));
                    run(&codec, &rx, &flag);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| format!("decoder thread: {e}"))?;
        ready
            .recv()
            .map_err(|_| "decoder thread exited".to_owned())??;
        Ok(Self {
            input,
            reader,
            needs_keyframe,
        })
    }

    pub fn submit(&self, index: u64, keyframe: bool, data: Vec<u8>) {
        let _ = self.input.send(Encoded {
            index,
            keyframe,
            data,
        });
    }

    /// The newest decoded image and the frame index it carries.
    pub fn latest(&self) -> Option<(Image, u64)> {
        match self.reader.acquire_latest_image() {
            Ok(AcquireResult::Image(image)) => {
                let index = image.timestamp().ok()? as u64 / 1000;
                Some((image, index))
            }
            _ => None,
        }
    }
}

fn open(window: &NativeWindow, width: u32, height: u32, hevc: bool) -> Result<MediaCodec, String> {
    let mime = if hevc { "video/hevc" } else { "video/avc" };
    let codec =
        MediaCodec::from_decoder_type(mime).ok_or_else(|| format!("no decoder for {mime}"))?;
    let mut format = MediaFormat::new();
    format.set_str("mime", mime);
    format.set_i32("width", width as i32);
    format.set_i32("height", height as i32);
    // Decode as fast as frames arrive rather than to a playback clock.
    format.set_i32("low-latency", 1);
    format.set_i32("priority", 0);
    format.set_i32("operating-rate", i16::MAX as i32);
    codec
        .configure(&format, Some(window), MediaCodecDirection::Decoder)
        .map_err(|e| format!("configure: {e:?}"))?;
    codec.start().map_err(|e| format!("start: {e:?}"))?;
    Ok(codec)
}

fn run(codec: &MediaCodec, rx: &Receiver<Encoded>, needs_keyframe: &AtomicBool) {
    let mut pending: VecDeque<Encoded> = VecDeque::new();
    let mut waiting_for_key = true;
    loop {
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(frame) => pending.push_back(frame),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        pending.extend(rx.try_iter());
        if pending.len() > MAX_BACKLOG {
            log::warn!(
                "decoder backlog of {}; skipping to the next keyframe",
                pending.len()
            );
            pending.clear();
            waiting_for_key = true;
            needs_keyframe.store(true, Ordering::Release);
        }
        if waiting_for_key {
            while pending.front().is_some_and(|f| !f.keyframe) {
                pending.pop_front();
            }
            waiting_for_key = pending.is_empty();
        }
        while let Some(frame) = pending.front() {
            match codec.dequeue_input_buffer(Duration::from_millis(1)) {
                Ok(DequeuedInputBufferResult::Buffer(mut buffer)) => {
                    let dst = buffer.buffer_mut();
                    let len = frame.data.len().min(dst.len());
                    // SAFETY: `len` fits both slices; MaybeUninit<u8> has u8's layout.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            frame.data.as_ptr(),
                            dst.as_mut_ptr().cast(),
                            len,
                        )
                    };
                    if let Err(e) = codec.queue_input_buffer(buffer, 0, len, frame.index, 0) {
                        log::warn!("queue input: {e:?}");
                    }
                    pending.pop_front();
                }
                Ok(DequeuedInputBufferResult::TryAgainLater) => break,
                Err(e) => {
                    log::warn!("dequeue input: {e:?}");
                    break;
                }
            }
        }
        loop {
            match codec.dequeue_output_buffer(Duration::ZERO) {
                Ok(DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let _ = codec.release_output_buffer(buffer, true);
                }
                Ok(DequeuedOutputBufferInfoResult::TryAgainLater) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
}
