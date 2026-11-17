//! A small binding to AAudio output streams. `ndk::audio` 0.9 is not used: its `Drop` panics
//! when `AAudioStream_close` fails, and it reports the size `AAudioStream_setBufferSizeInFrames`
//! granted, a success, as an error.

use std::ffi::{CStr, c_void};
use std::fmt;
use std::ptr::{self, NonNull};

use ndk_sys as ffi;

/// A negative `aaudio_result_t`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub ffi::aaudio_result_t);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: AAudio returns a static C string for any value.
        let text = unsafe { ffi::AAudio_convertResultToText(self.0) };
        if text.is_null() {
            return write!(f, "AAudio error {}", self.0);
        }
        // SAFETY: see above.
        let text = unsafe { CStr::from_ptr(text) };
        write!(f, "{} ({})", text.to_string_lossy(), self.0)
    }
}

impl std::error::Error for Error {}

fn check(result: ffi::aaudio_result_t) -> Result<i32, Error> {
    if result < 0 {
        Err(Error(result))
    } else {
        Ok(result)
    }
}

/// Called on AAudio's threads while its stream is open.
pub trait Handler: Sync {
    /// On AAudio's real-time thread, which must not allocate, lock or block (AAudio.h,
    /// `AAudioStream_dataCallback`). `out` holds the callback's frames, interleaved.
    fn render(&self, stream: &Callback<'_>, out: &mut [f32]);
    /// May only signal another thread to stop, close or reopen the stream (AAudio.h,
    /// `AAudioStreamBuilder_setErrorCallback`).
    fn error(&self, error: Error);
}

/// A stream that stays open while this value exists.
struct Open(NonNull<ffi::AAudioStream>);

impl Open {
    /// Every `AAudioStream_get*` but `getTimestamp` is thread safe and allowed in the data
    /// callback (AAudio.h, `AAudioStream_dataCallback`; the AAudio guide, "Thread safety").
    fn get<T>(&self, getter: unsafe extern "C" fn(*mut ffi::AAudioStream) -> T) -> T {
        // SAFETY: the stream is open, and the getter is thread safe.
        unsafe { getter(self.0.as_ptr()) }
    }
}

/// The stream as its data callback sees it.
pub struct Callback<'a> {
    stream: &'a Open,
    channels: usize,
}

impl Callback<'_> {
    pub fn sample_rate(&self) -> u32 {
        self.stream.get(ffi::AAudioStream_getSampleRate).max(0) as u32
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The stream position of this callback's first frame.
    pub fn frames_written(&self) -> i64 {
        self.stream.get(ffi::AAudioStream_getFramesWritten)
    }

    pub fn buffer_size(&self) -> u32 {
        self.stream
            .get(ffi::AAudioStream_getBufferSizeInFrames)
            .max(0) as u32
    }

    /// `(position, time)`: stream frame `position` was heard at `time`, CLOCK_MONOTONIC
    /// nanoseconds; none until the started stream has one. Only the data callback asks, since
    /// `getTimestamp` is the one getter AAudio does not make thread safe.
    pub fn timestamp(&self) -> Option<(i64, i64)> {
        let (mut position, mut time) = (0, 0);
        // SAFETY: the stream is open for the callback, and both out-pointers are valid.
        let result = unsafe {
            ffi::AAudioStream_getTimestamp(
                self.stream.0.as_ptr(),
                ffi::CLOCK_MONOTONIC as ffi::clockid_t,
                &mut position,
                &mut time,
            )
        };
        (result == ffi::AAUDIO_OK).then_some((position, time))
    }
}

/// An output stream of interleaved `f32` frames, closed on drop.
pub struct OutputStream<H: Handler> {
    stream: Open,
    /// From `Box::leak`; AAudio's threads share it until the stream is closed.
    handler: NonNull<H>,
}

impl<H: Handler> OutputStream<H> {
    /// Low latency, shared, for a game, `channels` channels at the device's own rate: asking for
    /// a rate can route the stream through a resampler with much higher latency (Android's
    /// low-latency audio guidance). Not started yet.
    pub fn open(channels: i32, handler: H) -> Result<Self, Error> {
        let builder = Builder::new()?;
        let handler = NonNull::from(Box::leak(Box::new(handler)));
        let user = handler.as_ptr().cast::<c_void>();
        let b = builder.0.as_ptr();
        let mut raw = ptr::null_mut();
        // SAFETY: `b` is a live builder and `raw` a valid out-pointer. `user` outlives the stream:
        // it is freed only after the stream is closed, or here if none opened.
        let opened = check(unsafe {
            ffi::AAudioStreamBuilder_setDirection(b, ffi::AAUDIO_DIRECTION_OUTPUT as i32);
            ffi::AAudioStreamBuilder_setPerformanceMode(
                b,
                ffi::AAUDIO_PERFORMANCE_MODE_LOW_LATENCY as i32,
            );
            ffi::AAudioStreamBuilder_setSharingMode(b, ffi::AAUDIO_SHARING_MODE_SHARED as i32);
            ffi::AAudioStreamBuilder_setUsage(b, ffi::AAUDIO_USAGE_GAME as i32);
            ffi::AAudioStreamBuilder_setFormat(b, ffi::AAUDIO_FORMAT_PCM_FLOAT);
            ffi::AAudioStreamBuilder_setChannelCount(b, channels);
            ffi::AAudioStreamBuilder_setDataCallback(b, Some(data::<H>), user);
            ffi::AAudioStreamBuilder_setErrorCallback(b, Some(error::<H>), user);
            ffi::AAudioStreamBuilder_openStream(b, &mut raw)
        });
        let Some(raw) = opened.ok().and_then(|_| NonNull::new(raw)) else {
            // SAFETY: no stream opened, so AAudio kept no pointer to the handler.
            drop(unsafe { Box::from_raw(handler.as_ptr()) });
            return Err(opened.err().unwrap_or(Error(ffi::AAUDIO_ERROR_NULL)));
        };
        let stream = Self {
            stream: Open(raw),
            handler,
        };
        // `data` hands the callback `f32`s; AAudio opens a requested format or fails, but the
        // slice's soundness should not rest on that alone.
        if stream.stream.get(ffi::AAudioStream_getFormat) != ffi::AAUDIO_FORMAT_PCM_FLOAT {
            return Err(Error(ffi::AAUDIO_ERROR_INVALID_FORMAT));
        }
        Ok(stream)
    }

    pub fn handler(&self) -> &H {
        // SAFETY: the handler lives until drop, and AAudio's threads only share it.
        unsafe { self.handler.as_ref() }
    }

    pub fn start(&self) -> Result<(), Error> {
        // SAFETY: the stream is open.
        check(unsafe { ffi::AAudioStream_requestStart(self.stream.0.as_ptr()) }).map(drop)
    }

    /// Returns the size AAudio granted, which may differ from `frames`.
    pub fn set_buffer_size(&self, frames: i32) -> Result<i32, Error> {
        // SAFETY: the stream is open.
        check(unsafe { ffi::AAudioStream_setBufferSizeInFrames(self.stream.0.as_ptr(), frames) })
    }

    pub fn sample_rate(&self) -> i32 {
        self.stream.get(ffi::AAudioStream_getSampleRate)
    }

    pub fn channels(&self) -> i32 {
        self.stream.get(ffi::AAudioStream_getChannelCount)
    }

    pub fn frames_per_burst(&self) -> i32 {
        self.stream.get(ffi::AAudioStream_getFramesPerBurst)
    }

    pub fn buffer_capacity(&self) -> i32 {
        self.stream.get(ffi::AAudioStream_getBufferCapacityInFrames)
    }

    pub fn buffer_size(&self) -> i32 {
        self.stream.get(ffi::AAudioStream_getBufferSizeInFrames)
    }

    pub fn xrun_count(&self) -> i32 {
        self.stream.get(ffi::AAudioStream_getXRunCount)
    }

    /// What was granted, which need not be what was asked (AAudio.h,
    /// `AAudioStreamBuilder_setPerformanceMode`).
    pub fn performance_mode(&self) -> &'static str {
        match self.stream.get(ffi::AAudioStream_getPerformanceMode) as u32 {
            ffi::AAUDIO_PERFORMANCE_MODE_LOW_LATENCY => "low latency",
            ffi::AAUDIO_PERFORMANCE_MODE_POWER_SAVING => "power saving",
            _ => "no performance mode",
        }
    }

    pub fn sharing_mode(&self) -> &'static str {
        match self.stream.get(ffi::AAudioStream_getSharingMode) as u32 {
            ffi::AAUDIO_SHARING_MODE_EXCLUSIVE => "exclusive",
            _ => "shared",
        }
    }
}

impl<H: Handler> Drop for OutputStream<H> {
    fn drop(&mut self) {
        // SAFETY: the stream is open and closed only here.
        match check(unsafe { ffi::AAudioStream_close(self.stream.0.as_ptr()) }) {
            // SAFETY: AAudioStream_close returns once the stream's callbacks are joined (AOSP
            // `AudioStreamTrack::close_l`, `stopAndJoinCallbacks`), so nothing calls into the
            // handler any more; it came from `Box::leak` in `open`.
            Ok(_) => drop(unsafe { Box::from_raw(self.handler.as_ptr()) }),
            // A stream that failed to close may still call back, so its handler is leaked.
            Err(e) => log::error!("closing the audio output: {e}"),
        }
    }
}

struct Builder(NonNull<ffi::AAudioStreamBuilder>);

impl Builder {
    fn new() -> Result<Self, Error> {
        let mut raw = ptr::null_mut();
        // SAFETY: valid out-pointer.
        check(unsafe { ffi::AAudio_createStreamBuilder(&mut raw) })?;
        NonNull::new(raw)
            .map(Self)
            .ok_or(Error(ffi::AAUDIO_ERROR_NULL))
    }
}

impl Drop for Builder {
    fn drop(&mut self) {
        // SAFETY: the builder is live and deleted only here; streams it opened do not need it.
        unsafe { ffi::AAudioStreamBuilder_delete(self.0.as_ptr()) };
    }
}

unsafe extern "C" fn data<H: Handler>(
    stream: *mut ffi::AAudioStream,
    user: *mut c_void,
    audio: *mut c_void,
    frames: i32,
) -> ffi::aaudio_data_callback_result_t {
    const CONTINUE: ffi::aaudio_data_callback_result_t =
        ffi::AAUDIO_CALLBACK_RESULT_CONTINUE as ffi::aaudio_data_callback_result_t;
    let Some(stream) = NonNull::new(stream).filter(|_| !audio.is_null()) else {
        return CONTINUE;
    };
    let stream = Open(stream);
    let channels = stream.get(ffi::AAudioStream_getChannelCount).max(0) as usize;
    let len = channels * frames.max(0) as usize;
    // SAFETY: `open` made sure the stream carries `f32` samples, and AAudio passes this callback
    // alone `frames` frames of `channels` samples each.
    let out = unsafe { std::slice::from_raw_parts_mut(audio.cast::<f32>(), len) };
    // SAFETY: `user` is the handler of the stream calling back, freed only once it is closed.
    let handler = unsafe { &*user.cast_const().cast::<H>() };
    handler.render(
        &Callback {
            stream: &stream,
            channels,
        },
        out,
    );
    CONTINUE
}

unsafe extern "C" fn error<H: Handler>(
    _: *mut ffi::AAudioStream,
    user: *mut c_void,
    error: ffi::aaudio_result_t,
) {
    // SAFETY: as in `data`.
    let handler = unsafe { &*user.cast_const().cast::<H>() };
    handler.error(Error(error));
}
