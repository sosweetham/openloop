//! The IOProc, the one function the HAL calls on its real-time IO thread (AudioHardware.h marks
//! AudioDeviceIOProc CA_REALTIME_API). It copies the tap's frames into the capture ring with
//! their capture time and does nothing else: no allocation, lock, log or blocking call.

use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use objc2_core_audio::AudioObjectID;
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp, AudioTimeStampFlags};

use crate::ring::{CHANNELS, CaptureRing, Chunk};
use crate::time::{Timebase, host_ticks};

/// What one registered IOProc writes with. It must outlive the registration: it is freed only
/// after AudioDeviceDestroyIOProcID succeeds, and leaked otherwise.
pub(crate) struct IoContext {
    ring: Arc<CaptureRing>,
    timebase: Timebase,
    pub epoch: u32,
    sample_rate: u32,
    active: AtomicBool,
    in_callback: AtomicBool,
    callbacks: AtomicU64,
    unreadable: AtomicU64,
}

impl IoContext {
    pub fn new(
        ring: Arc<CaptureRing>,
        timebase: Timebase,
        epoch: u32,
        sample_rate: u32,
    ) -> IoContext {
        IoContext {
            ring,
            timebase,
            epoch,
            sample_rate,
            active: AtomicBool::new(true),
            in_callback: AtomicBool::new(false),
            callbacks: AtomicU64::new(0),
            unreadable: AtomicU64::new(0),
        }
    }

    pub fn callbacks(&self) -> u64 {
        self.callbacks.load(Ordering::Relaxed)
    }

    /// Callbacks whose buffers could not be read (no buffer, null or misaligned data).
    pub fn unreadable(&self) -> u64 {
        self.unreadable.load(Ordering::Relaxed)
    }

    /// Stops this context writing and waits out a callback already in progress, so the ring
    /// has no producer once it returns, whatever the HAL does next. Non-real-time threads only.
    pub fn retire(&self) {
        // SeqCst pairs with on_io: either the callback sees `active` cleared, or this sees
        // `in_callback` set and waits for it.
        self.active.store(false, Ordering::SeqCst);
        while self.in_callback.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_micros(50));
        }
    }

    /// # Safety
    /// The pointers are as the HAL passes them to an IOProc: each null or valid for this call.
    /// One thread at a time.
    pub unsafe fn on_io(&self, input: *const AudioBufferList, input_time: *const AudioTimeStamp) {
        self.in_callback.store(true, Ordering::SeqCst);
        if self.active.load(Ordering::SeqCst) {
            // SAFETY: as this function's contract; an active context is the ring's producer.
            unsafe { self.capture(input, input_time) };
            self.callbacks.fetch_add(1, Ordering::Relaxed);
        }
        self.in_callback.store(false, Ordering::Release);
    }

    /// # Safety
    /// As [`on_io`](Self::on_io), and this context is the ring's producer.
    unsafe fn capture(&self, input: *const AudioBufferList, input_time: *const AudioTimeStamp) {
        let arrival_ns = self.timebase.ticks_to_ns(host_ticks());
        // SAFETY: null or valid, per the contract.
        let (host_ns, sample_time) = match unsafe { input_time.as_ref() } {
            Some(t) => (
                t.mFlags
                    .contains(AudioTimeStampFlags::HostTimeValid)
                    .then(|| self.timebase.ticks_to_ns(t.mHostTime)),
                t.mFlags
                    .contains(AudioTimeStampFlags::SampleTimeValid)
                    .then_some(t.mSampleTime),
            ),
            None => (None, None),
        };
        // SAFETY: null or valid, per the contract.
        let Some(layout) = (unsafe { Layout::of(input) }) else {
            self.unreadable.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let chunk = Chunk {
            epoch: self.epoch,
            sample_rate: self.sample_rate,
            host_ns,
            sample_time,
            arrival_ns,
        };
        // SAFETY: this context is the producer; `layout` covers `layout.frames` frames.
        unsafe {
            self.ring
                .push(chunk, layout.frames, |dst, offset| layout.copy(dst, offset))
        };
    }
}

/// Registered with AudioDeviceCreateIOProcID; `client` is the [`IoContext`].
///
/// # Safety
/// `client` is null or a live `IoContext`; the other pointers are as the HAL passes them.
pub(crate) unsafe extern "C" fn io_proc(
    _device: AudioObjectID,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> i32 {
    // SAFETY: per this function's contract.
    if let Some(context) = unsafe { client.cast::<IoContext>().as_ref() } {
        // SAFETY: the HAL's pointers, passed through.
        unsafe { context.on_io(input, input_time) };
    }
    0
}

/// Where each frame's left and right samples are in the HAL's buffers: one interleaved buffer
/// (a mono one feeds both sides), or one buffer per channel.
struct Layout {
    left: *const f32,
    right: *const f32,
    left_stride: usize,
    right_stride: usize,
    frames: usize,
}

impl Layout {
    /// # Safety
    /// `list` is null or an AudioBufferList with `mNumberBuffers` valid buffers.
    unsafe fn of(list: *const AudioBufferList) -> Option<Layout> {
        if list.is_null() {
            return None;
        }
        // SAFETY: valid per the contract. The struct declares one buffer and the HAL allocates
        // `mNumberBuffers`, so they are reached through the raw pointer, not the array type.
        let (count, buffers) = unsafe {
            (
                (*list).mNumberBuffers as usize,
                ptr::addr_of!((*list).mBuffers).cast::<AudioBuffer>(),
            )
        };
        match count {
            0 => None,
            1 => {
                // SAFETY: one valid buffer.
                let (data, channels, frames) = channel_data(unsafe { &*buffers })?;
                let right = if channels >= 2 {
                    data.wrapping_add(1)
                } else {
                    data
                };
                Some(Layout {
                    left: data,
                    right,
                    left_stride: channels,
                    right_stride: channels,
                    frames,
                })
            }
            _ => {
                // SAFETY: at least two valid buffers.
                let (left, left_stride, left_frames) = channel_data(unsafe { &*buffers })?;
                let (right, right_stride, right_frames) =
                    channel_data(unsafe { &*buffers.add(1) })?;
                Some(Layout {
                    left,
                    right,
                    left_stride,
                    right_stride,
                    frames: left_frames.min(right_frames),
                })
            }
        }
    }

    /// Writes frames `offset..` into `dst`, interleaved.
    ///
    /// # Safety
    /// `offset + dst.len() / CHANNELS` is at most `self.frames`.
    unsafe fn copy(&self, dst: &mut [f32], offset: usize) {
        let frames = dst.len() / CHANNELS;
        if self.left_stride == CHANNELS
            && self.right_stride == CHANNELS
            && self.right == self.left.wrapping_add(1)
        {
            // SAFETY: interleaved stereo, the tap's own layout: one copy of in-bounds frames.
            unsafe {
                ptr::copy_nonoverlapping(
                    self.left.add(offset * CHANNELS),
                    dst.as_mut_ptr(),
                    frames * CHANNELS,
                )
            };
            return;
        }
        for (i, frame) in dst.chunks_exact_mut(CHANNELS).enumerate() {
            let n = offset + i;
            // SAFETY: frame `n` is in bounds for both channels.
            unsafe {
                frame[0] = *self.left.add(n * self.left_stride);
                frame[1] = *self.right.add(n * self.right_stride);
            }
        }
    }
}

/// A buffer's first-channel samples, its channel count and its whole frames.
fn channel_data(buffer: &AudioBuffer) -> Option<(*const f32, usize, usize)> {
    let data = buffer.mData.cast::<f32>().cast_const();
    let channels = buffer.mNumberChannels as usize;
    if data.is_null() || !data.is_aligned() || channels == 0 {
        return None;
    }
    Some((
        data,
        channels,
        buffer.mDataByteSize as usize / (channels * size_of::<f32>()),
    ))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// An AudioBufferList with room for two buffers.
    #[repr(C)]
    pub(crate) struct BufferList {
        count: u32,
        buffers: [AudioBuffer; 2],
    }

    pub(crate) fn buffer(data: &mut [f32], channels: u32) -> AudioBuffer {
        AudioBuffer {
            mNumberChannels: channels,
            mDataByteSize: size_of_val(data) as u32,
            mData: data.as_mut_ptr().cast(),
        }
    }

    pub(crate) fn list(buffers: &[AudioBuffer]) -> BufferList {
        let empty = AudioBuffer {
            mNumberChannels: 0,
            mDataByteSize: 0,
            mData: ptr::null_mut(),
        };
        BufferList {
            count: buffers.len() as u32,
            buffers: [
                buffers.first().copied().unwrap_or(empty),
                buffers.get(1).copied().unwrap_or(empty),
            ],
        }
    }

    pub(crate) fn time(
        host_ticks: u64,
        sample_time: f64,
        flags: AudioTimeStampFlags,
    ) -> AudioTimeStamp {
        // SAFETY: AudioTimeStamp is plain data; all-zero is valid.
        let mut t: AudioTimeStamp = unsafe { std::mem::zeroed() };
        t.mHostTime = host_ticks;
        t.mSampleTime = sample_time;
        t.mFlags = flags;
        t
    }

    pub(crate) fn call(
        context: &IoContext,
        list: Option<&BufferList>,
        time: Option<&AudioTimeStamp>,
    ) {
        let input = list.map_or(ptr::null(), |l| ptr::from_ref(l).cast::<AudioBufferList>());
        let input_time = time.map_or(ptr::null(), ptr::from_ref);
        let now = self::time(0, 0.0, AudioTimeStampFlags::empty());
        // SAFETY: test buffers stay alive for the call; the context is live.
        unsafe {
            io_proc(
                7,
                &now,
                input,
                input_time,
                ptr::null_mut(),
                &now,
                ptr::from_ref(context).cast_mut().cast(),
            )
        };
    }

    fn read_all(ring: &CaptureRing) -> Vec<(crate::ring::Stamp, Vec<f32>)> {
        let mut out = Vec::new();
        // SAFETY: tests are single-consumer.
        unsafe {
            while let Some(stamp) = ring.peek() {
                let mut frames = vec![0.0; stamp.frames as usize * CHANNELS];
                ring.consume(stamp.position, stamp.frames as usize, |segment, offset| {
                    frames[offset * CHANNELS..offset * CHANNELS + segment.len()]
                        .copy_from_slice(segment);
                });
                ring.release_stamp();
                out.push((stamp, frames));
            }
        }
        out
    }

    fn context(ring: &Arc<CaptureRing>) -> IoContext {
        IoContext::new(ring.clone(), Timebase::new(125, 3), 4, 48_000)
    }

    const VALID: AudioTimeStampFlags = AudioTimeStampFlags::SampleHostTimeValid;

    #[test]
    fn copies_interleaved_stereo_with_its_capture_time() {
        let ring = Arc::new(CaptureRing::new(1024));
        let context = context(&ring);
        let mut data: Vec<f32> = (0..512 * 2).map(|i| i as f32).collect();
        let list = list(&[buffer(&mut data, 2)]);
        call(
            &context,
            Some(&list),
            Some(&time(24_000_000, 1234.0, VALID)),
        );
        let got = read_all(&ring);
        assert_eq!(got.len(), 1);
        let (stamp, frames) = &got[0];
        assert_eq!(
            (stamp.epoch, stamp.frames, stamp.sample_rate),
            (4, 512, 48_000)
        );
        assert_eq!(stamp.host_ns, Some(1_000_000_000));
        assert_eq!(stamp.sample_time, Some(1234.0));
        assert!(stamp.arrival_ns > 0);
        assert_eq!(frames, &data);
        assert_eq!(context.callbacks(), 1);
    }

    #[test]
    fn mono_feeds_both_sides_and_wider_buffers_give_their_first_two_channels() {
        let ring = Arc::new(CaptureRing::new(1024));
        let context = context(&ring);
        let mut mono = vec![0.25f32, -0.5, 0.75];
        call(
            &context,
            Some(&list(&[buffer(&mut mono, 1)])),
            Some(&time(0, 0.0, VALID)),
        );
        let mut quad: Vec<f32> = (0..8).map(|i| i as f32).collect();
        call(
            &context,
            Some(&list(&[buffer(&mut quad, 4)])),
            Some(&time(0, 3.0, VALID)),
        );
        let got = read_all(&ring);
        assert_eq!(got[0].1, vec![0.25, 0.25, -0.5, -0.5, 0.75, 0.75]);
        assert_eq!(got[1].1, vec![0.0, 1.0, 4.0, 5.0]);
    }

    #[test]
    fn planar_buffers_are_interleaved() {
        let ring = Arc::new(CaptureRing::new(1024));
        let context = context(&ring);
        let mut left = vec![1.0f32, 2.0, 3.0];
        let mut right = vec![-1.0f32, -2.0];
        call(
            &context,
            Some(&list(&[buffer(&mut left, 1), buffer(&mut right, 1)])),
            Some(&time(0, 0.0, VALID)),
        );
        assert_eq!(read_all(&ring)[0].1, vec![1.0, -1.0, 2.0, -2.0]);
    }

    #[test]
    fn invalid_times_are_left_out() {
        let ring = Arc::new(CaptureRing::new(1024));
        let context = context(&ring);
        let mut data = vec![0.0f32; 4];
        let list = list(&[buffer(&mut data, 2)]);
        call(
            &context,
            Some(&list),
            Some(&time(99, 5.0, AudioTimeStampFlags::HostTimeValid)),
        );
        call(
            &context,
            Some(&list),
            Some(&time(99, 5.0, AudioTimeStampFlags::SampleTimeValid)),
        );
        call(&context, Some(&list), None);
        let got = read_all(&ring);
        assert_eq!((got[0].0.host_ns, got[0].0.sample_time), (Some(4125), None));
        assert_eq!((got[1].0.host_ns, got[1].0.sample_time), (None, Some(5.0)));
        assert_eq!((got[2].0.host_ns, got[2].0.sample_time), (None, None));
    }

    #[test]
    fn unreadable_buffers_are_counted_not_copied() {
        let ring = Arc::new(CaptureRing::new(1024));
        let context = context(&ring);
        call(&context, None, None);
        call(&context, Some(&list(&[])), None);
        let null = AudioBuffer {
            mNumberChannels: 2,
            mDataByteSize: 64,
            mData: ptr::null_mut(),
        };
        call(&context, Some(&list(&[null])), None);
        let mut data = vec![0.0f32; 4];
        call(&context, Some(&list(&[buffer(&mut data, 0)])), None);
        let mut floats = [0.0f32; 16];
        let one_byte_in = floats.as_mut_ptr().cast::<u8>().wrapping_add(1);
        let misaligned = AudioBuffer {
            mNumberChannels: 2,
            mDataByteSize: 32,
            mData: one_byte_in.cast(),
        };
        call(&context, Some(&list(&[misaligned])), None);
        assert!(read_all(&ring).is_empty());
        assert_eq!((context.callbacks(), context.unreadable()), (5, 5));
    }

    #[test]
    fn partial_frames_are_not_read() {
        let ring = Arc::new(CaptureRing::new(1024));
        let context = context(&ring);
        let mut data = vec![1.0f32; 5];
        call(&context, Some(&list(&[buffer(&mut data, 2)])), None);
        assert_eq!(read_all(&ring)[0].0.frames, 2);
    }

    #[test]
    fn a_retired_context_writes_nothing() {
        let ring = Arc::new(CaptureRing::new(1024));
        let context = context(&ring);
        context.retire();
        let mut data = vec![1.0f32; 4];
        call(
            &context,
            Some(&list(&[buffer(&mut data, 2)])),
            Some(&time(0, 0.0, VALID)),
        );
        assert!(read_all(&ring).is_empty());
        assert_eq!(context.callbacks(), 0);
    }

    #[test]
    fn retire_waits_out_a_callback_on_another_thread() {
        let ring = Arc::new(CaptureRing::new(1 << 16));
        let context = Arc::new(context(&ring));
        let io_context = context.clone();
        let io = std::thread::spawn(move || {
            let mut data = vec![0.5f32; 64];
            let list = list(&[buffer(&mut data, 2)]);
            let mut calls = 0u64;
            while calls < 2_000_000 {
                call(
                    &io_context,
                    Some(&list),
                    Some(&time(0, calls as f64 * 32.0, VALID)),
                );
                calls += 1;
                if io_context.callbacks() < calls {
                    return calls;
                }
            }
            calls
        });
        std::thread::sleep(Duration::from_millis(20));
        context.retire();
        let written = context.callbacks();
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            context.callbacks(),
            written,
            "nothing written after retire returned"
        );
        io.join().unwrap();
    }
}
