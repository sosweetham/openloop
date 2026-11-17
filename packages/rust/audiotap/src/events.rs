//! What the capture engine tells its owner.

use std::fmt;

/// A HAL call the control thread makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HalCall {
    TranslatePid,
    AddListener,
    RemoveListener,
    CreateTap,
    ReadTapFormat,
    DestroyTap,
    CreateAggregate,
    ReadDeviceAlive,
    DestroyAggregate,
    CreateIoProc,
    StartIo,
    StopIo,
    DestroyIoProc,
}

/// Why capture is off for the rest of this process, and across restarts when a marker file is
/// configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockReason {
    /// A HAL call took about as long as the HAL's own RPC timeout: a System Audio Recording
    /// prompt, this process's or any other's, may be waiting for an answer.
    Stalled { call: HalCall, elapsed_ms: u64 },
    /// IO started but no callback arrived: the permission is missing, denied or still pending.
    NoCallbacks { waited_ms: u64 },
    /// An earlier process recorded a block in the marker file.
    Recorded(String),
}

impl fmt::Display for BlockReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockReason::Stalled { call, elapsed_ms } => write!(
                f,
                "{call:?} took {elapsed_ms} ms: a System Audio Recording prompt may be waiting for an answer"
            ),
            BlockReason::NoCallbacks { waited_ms } => write!(
                f,
                "no audio arrived {waited_ms} ms after IO started: the System Audio Recording permission is missing, denied or still pending"
            ),
            BlockReason::Recorded(text) => write!(f, "recorded by an earlier run: {}", text.trim()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Stopped,
    /// Another capture took over.
    Replaced,
    ProcessExited,
    /// The tap's format changed before the permission was proven, so nothing was rebuilt.
    FormatChanged,
    /// The aggregate device died before the permission was proven, so nothing was rebuilt.
    DeviceDied,
    /// The audio server restarted before the permission was proven, so nothing was rebuilt.
    ServiceRestarted,
    /// A HAL call failed; the [`Event::Failed`] before this says which. Not retried.
    Failed,
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartReason {
    FormatChanged,
    DeviceDied,
    ServiceRestarted,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// The process has no Core Audio process object yet, because it has not used audio;
    /// capture starts when one appears.
    WaitingForProcess { pid: i32 },
    /// The tap, its aggregate device and the IOProc are up, and IO is started.
    Started { pid: i32, sample_rate: u32 },
    /// The first IOProc callback arrived, which proves the capture permission.
    Receiving { pid: i32, after_ms: u64 },
    /// The aggregate device and IOProc were rebuilt; packets continue in a new stream.
    Restarted { pid: i32, reason: RestartReason, sample_rate: u32 },
    /// The tap delivers something other than Float32 linear PCM at a whole sample rate.
    UnsupportedFormat { pid: i32, sample_rate: f64, channels: u32, float32: bool },
    SlowCall { call: HalCall, elapsed_ms: u64 },
    /// `status` is the call's OSStatus.
    Failed { pid: i32, call: HalCall, status: i32 },
    /// Frames the IOProc dropped since the last report because the ring was full.
    Overrun { dropped_frames: u64 },
    /// IOProc callbacks since the last report whose buffers could not be read: none, a null or
    /// misaligned buffer, or no channels.
    Unreadable { pid: i32, callbacks: u64 },
    /// Capture is now off: no HAL call will be made again in this process.
    Blocked(BlockReason),
    /// A capture request arrived while capture is blocked.
    Refused { pid: i32, reason: BlockReason },
    Ended { pid: i32, reason: EndReason },
}

/// An OSStatus as a number and, when it is one, a four-character code: `560947818 ('!obj')`.
pub fn status_text(status: i32) -> String {
    let bytes = status.to_be_bytes();
    if bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        format!("{status} ('{}')", String::from_utf8_lossy(&bytes))
    } else {
        status.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_read_as_four_character_codes() {
        assert_eq!(status_text(560947818), "560947818 ('!obj')");
        assert_eq!(status_text(268451843), "268451843");
        assert_eq!(status_text(-50), "-50");
    }
}
