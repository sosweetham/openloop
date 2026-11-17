//! Checks the video the Mac sends: every stream announced by a ServerHello and opened by a
//! keyframe, frame indices that only rise, display times close ahead of arrival on the headset
//! clock, and Annex-B access units whose keyframe flag matches their NAL units.

use serde::Serialize;
use tether_protocol::{ClientTime, PROTOCOL_VERSION, ServerHello, VideoFrame};

use crate::report::{Finding, Stats};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Hevc,
    H264,
}

impl Codec {
    fn nal_type(self, header: u8) -> u8 {
        match self {
            Codec::Hevc => (header >> 1) & 0x3f,
            Codec::H264 => header & 0x1f,
        }
    }

    fn is_picture(self, nal: u8) -> bool {
        match self {
            Codec::Hevc => nal <= 31,
            Codec::H264 => (1..=5).contains(&nal),
        }
    }

    /// HEVC IRAP or H.264 IDR: decoding can start here.
    fn is_random_access(self, nal: u8) -> bool {
        match self {
            Codec::Hevc => (16..=23).contains(&nal),
            Codec::H264 => nal == 5,
        }
    }

    /// VPS, SPS, PPS or SPS, PPS. Tether repeats them on every keyframe so a client can join or
    /// recover at any keyframe.
    fn parameter_sets(self) -> &'static [u8] {
        match self {
            Codec::Hevc => &[32, 33, 34],
            Codec::H264 => &[7, 8],
        }
    }
}

/// The NAL units of an Annex-B access unit, without their start codes.
fn nal_units(data: &[u8]) -> Result<Vec<&[u8]>, String> {
    let first = if data.starts_with(&[0, 0, 0, 1]) {
        4
    } else if data.starts_with(&[0, 0, 1]) {
        3
    } else {
        return Err("does not start with an Annex-B start code".into());
    };
    let mut units = Vec::new();
    let mut rest = &data[first..];
    loop {
        let next = rest.windows(3).position(|w| w == [0, 0, 1]);
        let mut unit = &rest[..next.unwrap_or(rest.len())];
        // Zeros before a start code belong to it (a four-byte start code, or trailing zeros).
        while let [head @ .., 0] = unit {
            unit = head;
        }
        units.push(unit);
        match next {
            Some(i) => rest = &rest[i + 3..],
            None => return Ok(units),
        }
    }
}

/// Whether an access unit is well formed and flagged as what it is.
pub fn check_access_unit(codec: Codec, keyframe: bool, data: &[u8]) -> Result<(), String> {
    let mut types = Vec::new();
    for unit in nal_units(data)? {
        let &header = unit.first().ok_or("has an empty NAL unit")?;
        if header & 0x80 != 0 {
            return Err("has a NAL unit with its forbidden bit set".into());
        }
        types.push(codec.nal_type(header));
    }
    if !types.iter().any(|&t| codec.is_picture(t)) {
        return Err(format!("has no picture (NAL types {types:?})"));
    }
    let random_access = types.iter().any(|&t| codec.is_random_access(t));
    if random_access != keyframe {
        return Err(format!(
            "is flagged keyframe={keyframe} but its NAL types are {types:?}"
        ));
    }
    if keyframe && !codec.parameter_sets().iter().all(|p| types.contains(p)) {
        return Err(format!(
            "is a keyframe without its parameter sets (NAL types {types:?})"
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StreamSummary {
    pub video_width: u32,
    pub video_height: u32,
    pub began_at_s: f64,
    pub ended_at_s: Option<f64>,
    pub frames: u64,
    pub keyframes: u64,
    pub first_index: Option<u64>,
    pub last_index: Option<u64>,
    pub began_with_keyframe: Option<bool>,
    /// Places where indices skipped ahead, and how many frames they skipped in all.
    pub index_gaps: u64,
    pub missing_frames: u64,
    /// Frames per second between the stream's first and last arrival.
    pub arrival_rate_hz: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct VideoReport {
    pub frames: u64,
    pub keyframes: u64,
    pub bytes: u64,
    pub index_gaps: u64,
    pub missing_frames: u64,
    /// Display times outside the plausible window around arrival.
    pub implausible_display_times: u64,
    /// Frames that arrived after their display time.
    pub late_frames: u64,
    pub malformed_access_units: u64,
    /// Between consecutive frames of a stream, on CLOCK_MONOTONIC.
    pub arrival_interval_ms: Stats,
    /// Arrival intervals over twice the headset's frame period.
    pub arrival_intervals_over_2x_period: u64,
    /// Between consecutive frames' display times, on the headset clock.
    pub display_interval_ms: Stats,
    /// Display time minus arrival, on the headset clock: how far ahead of its display a frame
    /// arrives.
    pub display_lead_ms: Stats,
    pub streams: Vec<StreamSummary>,
}

struct Stream {
    summary: StreamSummary,
    first_arrival: Option<i64>,
    last_arrival: Option<i64>,
    last_display: Option<i64>,
}

impl Stream {
    fn summary(&self) -> StreamSummary {
        let span = match (self.first_arrival, self.last_arrival) {
            (Some(first), Some(last)) if last > first => (last - first) as f64 / 1e9,
            _ => 0.0,
        };
        StreamSummary {
            arrival_rate_hz: if span > 0.0 {
                self.summary.frames.saturating_sub(1) as f64 / span
            } else {
                0.0
            },
            ..self.summary.clone()
        }
    }
}

pub struct VideoCheck {
    codec: Codec,
    period_ns: f64,
    lead_window_ns: (i64, i64),
    stream: Option<Stream>,
    report: VideoReport,
    arrival_intervals: Vec<f64>,
    display_intervals: Vec<f64>,
    leads: Vec<f64>,
}

impl VideoCheck {
    pub fn new(codec: Codec, refresh_hz: f32, lead_window_ns: (i64, i64)) -> Self {
        Self {
            codec,
            period_ns: 1e9 / f64::from(refresh_hz.max(1.0)),
            lead_window_ns,
            stream: None,
            report: VideoReport::default(),
            arrival_intervals: Vec::new(),
            display_intervals: Vec::new(),
            leads: Vec::new(),
        }
    }

    /// `true` when the hello starts a stream.
    pub fn server_hello(&mut self, at_s: f64, hello: &ServerHello, out: &mut Vec<Finding>) -> bool {
        self.end(at_s);
        if hello.version != PROTOCOL_VERSION {
            out.push(Finding::error(format!(
                "ServerHello speaks protocol {}, this headset {PROTOCOL_VERSION}",
                hello.version
            )));
            return false;
        }
        if hello.video_width == 0 || hello.video_height == 0 {
            out.push(Finding::error(format!(
                "ServerHello announces a {}x{} stream",
                hello.video_width, hello.video_height
            )));
            return false;
        }
        self.stream = Some(Stream {
            summary: StreamSummary {
                video_width: hello.video_width,
                video_height: hello.video_height,
                began_at_s: at_s,
                ..Default::default()
            },
            first_arrival: None,
            last_arrival: None,
            last_display: None,
        });
        true
    }

    pub fn end(&mut self, at_s: f64) {
        if let Some(stream) = self.stream.take() {
            let mut summary = stream.summary();
            summary.ended_at_s = Some(at_s);
            self.report.streams.push(summary);
        }
    }

    /// Checks one frame that arrived at `arrival` (CLOCK_MONOTONIC) and `arrival_client` (the
    /// headset clock).
    pub fn frame(
        &mut self,
        arrival: i64,
        arrival_client: ClientTime,
        frame: &VideoFrame,
        out: &mut Vec<Finding>,
    ) {
        self.report.frames += 1;
        self.report.keyframes += u64::from(frame.keyframe);
        self.report.bytes += frame.data.len() as u64;
        if let Err(e) = check_access_unit(self.codec, frame.keyframe, &frame.data) {
            self.report.malformed_access_units += 1;
            out.push(Finding::error(format!("frame {} {e}", frame.frame_index)));
        }
        let lead = frame.display_time_ns - arrival_client.0;
        self.leads.push(lead as f64 / 1e6);
        self.report.late_frames += u64::from(lead < 0);
        if !(self.lead_window_ns.0..=self.lead_window_ns.1).contains(&lead) {
            self.report.implausible_display_times += 1;
            out.push(Finding::error(format!(
                "frame {} is to be displayed {:.1} ms after it arrived, outside {:.0}..{:.0} ms",
                frame.frame_index,
                lead as f64 / 1e6,
                self.lead_window_ns.0 as f64 / 1e6,
                self.lead_window_ns.1 as f64 / 1e6
            )));
        }

        let Some(stream) = &mut self.stream else {
            out.push(Finding::error(format!(
                "frame {} arrived outside a stream (no ServerHello since the last end)",
                frame.frame_index
            )));
            return;
        };
        let s = &mut stream.summary;
        match s.last_index {
            None => {
                s.first_index = Some(frame.frame_index);
                s.began_with_keyframe = Some(frame.keyframe);
                if !frame.keyframe {
                    out.push(Finding::error(format!(
                        "the {}x{} stream began with frame {}, which is not a keyframe",
                        s.video_width, s.video_height, frame.frame_index
                    )));
                }
            }
            Some(last) if frame.frame_index <= last => out.push(Finding::error(format!(
                "frame index went from {last} to {}",
                frame.frame_index
            ))),
            Some(last) if frame.frame_index > last + 1 => {
                let missing = frame.frame_index - last - 1;
                s.index_gaps += 1;
                s.missing_frames += missing;
                self.report.index_gaps += 1;
                self.report.missing_frames += missing;
                out.push(Finding::warning(format!(
                    "{missing} frame(s) after {last} never arrived"
                )));
            }
            Some(_) => {}
        }
        s.last_index = Some(frame.frame_index);
        s.frames += 1;
        s.keyframes += u64::from(frame.keyframe);

        stream.first_arrival.get_or_insert(arrival);
        if let Some(previous) = stream.last_arrival.replace(arrival) {
            let interval = (arrival - previous) as f64;
            self.arrival_intervals.push(interval / 1e6);
            if interval > 2.0 * self.period_ns {
                self.report.arrival_intervals_over_2x_period += 1;
            }
        }
        if let Some(previous) = stream.last_display.replace(frame.display_time_ns) {
            let interval = frame.display_time_ns - previous;
            self.display_intervals.push(interval as f64 / 1e6);
            if interval <= 0 {
                out.push(Finding::error(format!(
                    "frame {}'s display time is {:.3} ms after the previous frame's",
                    frame.frame_index,
                    interval as f64 / 1e6
                )));
            }
        }
    }

    pub fn report(&self) -> VideoReport {
        let mut report = self.report.clone();
        report
            .streams
            .extend(self.stream.as_ref().map(Stream::summary));
        report.arrival_interval_ms = Stats::of(&self.arrival_intervals);
        report.display_interval_ms = Stats::of(&self.display_intervals);
        report.display_lead_ms = Stats::of(&self.leads);
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Access units shaped like VideoToolbox's HEVC output after Tether's Annex-B conversion.
    fn hevc_keyframe() -> Vec<u8> {
        let mut au = Vec::new();
        for (header, body) in [
            (32u8, &[0x0c, 0x01][..]),
            (33, &[0x01, 0x01, 0x60]),
            (34, &[0xc1, 0x73]),
            (19, &[0xaf, 0x00, 0x00, 0x03, 0x01, 0x80]),
        ] {
            au.extend_from_slice(&[0, 0, 0, 1, header << 1, 1]);
            au.extend_from_slice(body);
        }
        au
    }

    fn hevc_delta() -> Vec<u8> {
        vec![0, 0, 0, 1, 1 << 1, 1, 0xd0, 0x00, 0x00, 0x03, 0x02, 0x40]
    }

    #[test]
    fn well_formed_access_units_pass() {
        assert_eq!(
            check_access_unit(Codec::Hevc, true, &hevc_keyframe()),
            Ok(())
        );
        assert_eq!(check_access_unit(Codec::Hevc, false, &hevc_delta()), Ok(()));
        let h264_idr = [
            0, 0, 0, 1, 0x67, 0x42, 0, 0, 1, 0x68, 0xce, 0, 0, 1, 0x65, 0x88,
        ];
        assert_eq!(check_access_unit(Codec::H264, true, &h264_idr), Ok(()));
    }

    #[test]
    fn mislabelled_or_broken_access_units_fail() {
        assert!(check_access_unit(Codec::Hevc, false, &hevc_keyframe()).is_err());
        assert!(check_access_unit(Codec::Hevc, true, &hevc_delta()).is_err());
        let without_sets = hevc_keyframe()[25..].to_vec();
        assert!(
            check_access_unit(Codec::Hevc, true, &without_sets)
                .unwrap_err()
                .contains("parameter sets")
        );
        assert!(
            check_access_unit(Codec::Hevc, false, &hevc_delta()[4..]).is_err(),
            "AVCC"
        );
        assert!(check_access_unit(Codec::Hevc, false, &[0, 0, 1, 0x80, 1]).is_err());
        assert!(check_access_unit(Codec::Hevc, false, &[0, 0, 1, 0, 0, 1, 2, 1]).is_err());
        assert!(
            check_access_unit(Codec::Hevc, false, &[0, 0, 1, 64, 1]).is_err(),
            "VPS only"
        );
    }

    #[test]
    fn zeros_before_a_start_code_are_not_part_of_the_unit() {
        let au = [0, 0, 0, 1, 0x40, 1, 0, 0, 0, 0, 1, 0x02, 1, 0x80];
        let units = nal_units(&au).unwrap();
        assert_eq!(units, [&[0x40, 1][..], &[0x02, 1, 0x80][..]]);
    }
}
