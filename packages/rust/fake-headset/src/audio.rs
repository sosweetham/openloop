//! Checks the audio the Mac sends: only to a client that plays audio and in its format, one stream
//! at a time, started before its packets and never after its stop, sequence numbers and frame
//! indices that run on without overlap, and presentation times spaced by the frames between them.
//! Each stream's first channel is measured too, so a test tone shows it arrived intact.

use std::collections::HashMap;

use serde::Serialize;
use tether_protocol::{AudioFormat, AudioPacket, ClientTime};

use crate::report::{Finding, Stats};

#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioStreamSummary {
    pub stream: u16,
    pub sample_rate: u32,
    pub channels: u8,
    pub started_at_s: Option<f64>,
    /// When an AudioStop, a newer stream or the end of the link ended it.
    pub ended_at_s: Option<f64>,
    pub packets: u64,
    pub frames: u64,
    /// The first channel's level; full scale is 1.0.
    pub rms: f64,
    /// The first channel's frequency from its rising zero crossings; 0 with fewer than two.
    pub frequency_hz: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AudioReport {
    pub starts: u64,
    pub stops: u64,
    pub packets: u64,
    pub frames: u64,
    /// Sequence numbers skipped within a stream.
    pub lost_packets: u64,
    /// Frames skipped between consecutive packets of a stream.
    pub missing_frames: u64,
    /// Packets that arrived after their presentation time.
    pub late_packets: u64,
    /// Presentation time minus arrival, on the headset clock.
    pub lead_ms: Stats,
    /// Each packet's presentation time against the previous packet's plus the frames between
    /// them at the stream's rate: the timestamps' jitter.
    pub spacing_error_ms: Stats,
    /// Each packet's presentation time against the one its frame index implies from the
    /// stream's first packet: the drift of the Mac's audio clock and clock mapping.
    pub schedule_error_ms: Stats,
    pub packet_frames: Stats,
    pub streams: Vec<AudioStreamSummary>,
}

struct Stream {
    /// Index into the report's streams.
    summary: usize,
    format: AudioFormat,
    ended: Option<Ended>,
    /// `(sequence, first_frame, frames, present_at)` of the latest packet.
    last: Option<(u32, u64, u64, i64)>,
    /// `(first_frame, present_at)` of the stream's first packet.
    anchor: Option<(u64, i64)>,
    level: Level,
}

/// Measures the first channel of a stream as its packets arrive.
#[derive(Default)]
struct Level {
    squares: f64,
    samples: u64,
    /// The frame index and value of the latest sample, to find crossings between packets.
    last: Option<(u64, f64)>,
    /// Fractional frame positions of the first and latest rising zero crossings.
    first_crossing: Option<f64>,
    last_crossing: f64,
    crossings: u64,
}

impl Level {
    fn add(&mut self, first_frame: u64, samples: &[i16], channels: usize) {
        for (i, frame) in samples.chunks_exact(channels.max(1)).enumerate() {
            let index = first_frame + i as u64;
            let value = f64::from(frame[0]) / 32768.0;
            self.squares += value * value;
            self.samples += 1;
            if let Some((previous_index, previous)) = self.last
                && previous_index + 1 == index
                && previous < 0.0
                && value >= 0.0
            {
                let at = previous_index as f64 + previous / (previous - value);
                self.first_crossing.get_or_insert(at);
                self.last_crossing = at;
                self.crossings += 1;
            }
            self.last = Some((index, value));
        }
    }

    fn rms(&self) -> f64 {
        if self.samples == 0 {
            0.0
        } else {
            (self.squares / self.samples as f64).sqrt()
        }
    }

    fn frequency_hz(&self, sample_rate: u32) -> f64 {
        match self.first_crossing {
            Some(first) if self.crossings >= 2 && self.last_crossing > first => {
                (self.crossings - 1) as f64 * f64::from(sample_rate) / (self.last_crossing - first)
            }
            _ => 0.0,
        }
    }
}

#[derive(Clone, Copy)]
enum Ended {
    Stopped,
    Replaced(u16),
    LinkDown,
}

pub struct AudioCheck {
    /// The profile's sample rate and channels; `None` when the client plays no audio.
    output: Option<(u32, u8)>,
    streams: HashMap<u16, Stream>,
    playing: Option<u16>,
    report: AudioReport,
    leads: Vec<f64>,
    spacing_errors: Vec<f64>,
    schedule_errors: Vec<f64>,
    packet_frames: Vec<f64>,
}

impl AudioCheck {
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            output: (sample_rate > 0).then_some((sample_rate, channels)),
            streams: HashMap::new(),
            playing: None,
            report: AudioReport::default(),
            leads: Vec::new(),
            spacing_errors: Vec::new(),
            schedule_errors: Vec::new(),
            packet_frames: Vec::new(),
        }
    }

    pub fn start(&mut self, at_s: f64, format: AudioFormat, out: &mut Vec<Finding>) {
        self.report.starts += 1;
        if let Some(previous) = self.playing {
            if previous == format.stream {
                out.push(Finding::error(format!(
                    "audio stream {} started again without a stop",
                    format.stream
                )));
            }
            self.end(previous, at_s, Ended::Replaced(format.stream));
        }
        self.check_format(format, out);
        let summary = self.summary(format, Some(at_s));
        self.streams.insert(
            format.stream,
            Stream {
                summary,
                format,
                ended: None,
                last: None,
                anchor: None,
                level: Level::default(),
            },
        );
        self.playing = Some(format.stream);
    }

    pub fn stop(&mut self, at_s: f64, stream: u16, out: &mut Vec<Finding>) {
        self.report.stops += 1;
        if self.playing == Some(stream) {
            self.end(stream, at_s, Ended::Stopped);
        } else {
            out.push(Finding::warning(format!(
                "AudioStop for stream {stream}, which is not playing"
            )));
        }
    }

    /// Every stream ends with the link it played on.
    pub fn link_down(&mut self, at_s: f64) {
        let open: Vec<u16> = self
            .streams
            .iter()
            .filter(|(_, s)| s.ended.is_none())
            .map(|(&id, _)| id)
            .collect();
        for id in open {
            self.end(id, at_s, Ended::LinkDown);
        }
    }

    pub fn packet(&mut self, arrival: ClientTime, packet: &AudioPacket, out: &mut Vec<Finding>) {
        let header = &packet.header;
        let frames = packet.frames() as u64;
        self.report.packets += 1;
        self.report.frames += frames;
        self.packet_frames.push(frames as f64);
        let lead = header.present_at.0 - arrival.0;
        self.leads.push(lead as f64 / 1e6);
        if lead < 0 {
            self.report.late_packets += 1;
        }
        if frames == 0 {
            out.push(Finding::warning(format!(
                "audio packet {} of stream {} has no frames",
                header.sequence, header.format.stream
            )));
        }

        let id = header.format.stream;
        let Some(stream) = self.streams.get_mut(&id) else {
            out.push(Finding::error(format!(
                "audio packet {} of stream {id}, which was never started",
                header.sequence
            )));
            self.check_format(header.format, out);
            let summary = self.summary(header.format, None);
            self.streams.insert(
                id,
                Stream {
                    summary,
                    format: header.format,
                    ended: None,
                    last: Some((
                        header.sequence,
                        header.first_frame,
                        frames,
                        header.present_at.0,
                    )),
                    anchor: Some((header.first_frame, header.present_at.0)),
                    level: Level::default(),
                },
            );
            self.count(summary, frames);
            return;
        };
        match stream.ended {
            None => {}
            Some(Ended::Stopped) => out.push(Finding::error(format!(
                "audio packet {} of stream {id} after its AudioStop",
                header.sequence
            ))),
            Some(Ended::Replaced(by)) => out.push(Finding::error(format!(
                "audio packet {} of stream {id} after stream {by} replaced it",
                header.sequence
            ))),
            Some(Ended::LinkDown) => out.push(Finding::error(format!(
                "audio packet {} of stream {id}, which ended with its link",
                header.sequence
            ))),
        }
        if header.format != stream.format {
            out.push(Finding::error(format!(
                "audio stream {id} changed format from {:?} to {:?} without a restart",
                stream.format, header.format
            )));
        }
        let rate = f64::from(stream.format.sample_rate);
        if let Some((sequence, first_frame, last_frames, present_at)) = stream.last {
            let expected = sequence.wrapping_add(1);
            if header.sequence != expected {
                let skipped = header.sequence.wrapping_sub(expected);
                if skipped < u32::MAX / 2 {
                    self.report.lost_packets += u64::from(skipped);
                    out.push(Finding::warning(format!(
                        "audio stream {id} lost {skipped} packet(s) before {}",
                        header.sequence
                    )));
                } else {
                    out.push(Finding::error(format!(
                        "audio stream {id}'s sequence went back from {sequence} to {}",
                        header.sequence
                    )));
                }
            }
            let next = first_frame + last_frames;
            if header.first_frame > next {
                self.report.missing_frames += header.first_frame - next;
                out.push(Finding::warning(format!(
                    "audio stream {id} skipped {} frame(s) before frame {}",
                    header.first_frame - next,
                    header.first_frame
                )));
            } else if header.first_frame < next {
                out.push(Finding::error(format!(
                    "audio stream {id} went back to frame {} after frames up to {}",
                    header.first_frame,
                    next - 1
                )));
            }
            if rate > 0.0 {
                let between = (header.first_frame as f64 - first_frame as f64) * 1e9 / rate;
                let spacing = (header.present_at.0 - present_at) as f64;
                self.spacing_errors.push((spacing - between) / 1e6);
            }
        }
        stream.last = Some((
            header.sequence,
            header.first_frame,
            frames,
            header.present_at.0,
        ));
        let (anchor_frame, anchor_time) = *stream
            .anchor
            .get_or_insert((header.first_frame, header.present_at.0));
        if rate > 0.0 {
            let elapsed = (header.first_frame as f64 - anchor_frame as f64) * 1e9 / rate;
            let expected = anchor_time as f64 + elapsed;
            self.schedule_errors
                .push((header.present_at.0 as f64 - expected) / 1e6);
        }
        let channels = usize::from(stream.format.channels);
        stream
            .level
            .add(header.first_frame, &packet.samples, channels);
        let (summary, rms, frequency_hz) = (
            stream.summary,
            stream.level.rms(),
            stream.level.frequency_hz(stream.format.sample_rate),
        );
        self.count(summary, frames);
        let s = &mut self.report.streams[summary];
        (s.rms, s.frequency_hz) = (rms, frequency_hz);
    }

    pub fn report(&self) -> AudioReport {
        AudioReport {
            lead_ms: Stats::of(&self.leads),
            spacing_error_ms: Stats::of(&self.spacing_errors),
            schedule_error_ms: Stats::of(&self.schedule_errors),
            packet_frames: Stats::of(&self.packet_frames),
            ..self.report.clone()
        }
    }

    fn end(&mut self, stream: u16, at_s: f64, why: Ended) {
        if let Some(s) = self.streams.get_mut(&stream) {
            s.ended = Some(why);
            self.report.streams[s.summary].ended_at_s = Some(at_s);
        }
        if self.playing == Some(stream) {
            self.playing = None;
        }
    }

    fn check_format(&self, format: AudioFormat, out: &mut Vec<Finding>) {
        match self.output {
            None => out.push(Finding::error(format!(
                "audio stream {} sent to a headset whose profile plays no audio",
                format.stream
            ))),
            Some((rate, channels)) if (format.sample_rate, format.channels) != (rate, channels) => {
                out.push(Finding::error(format!(
                    "audio stream {} is {} Hz × {}, the headset plays {rate} Hz × {channels}",
                    format.stream, format.sample_rate, format.channels
                )))
            }
            Some(_) => {}
        }
    }

    fn summary(&mut self, format: AudioFormat, started_at_s: Option<f64>) -> usize {
        self.report.streams.push(AudioStreamSummary {
            stream: format.stream,
            sample_rate: format.sample_rate,
            channels: format.channels,
            started_at_s,
            ..Default::default()
        });
        self.report.streams.len() - 1
    }

    fn count(&mut self, summary: usize, frames: u64) {
        let s = &mut self.report.streams[summary];
        s.packets += 1;
        s.frames += frames;
    }
}

#[cfg(test)]
mod tests {
    use tether_protocol::AudioHeader;

    use super::*;
    use crate::report::Severity;

    const FORMAT: AudioFormat = AudioFormat {
        stream: 1,
        sample_rate: 48_000,
        channels: 2,
    };
    const MS: i64 = 1_000_000;

    fn packet(stream: u16, sequence: u32, first_frame: u64, present_at: i64) -> AudioPacket {
        AudioPacket {
            header: AudioHeader {
                format: AudioFormat { stream, ..FORMAT },
                sequence,
                first_frame,
                present_at: ClientTime(present_at),
            },
            samples: vec![0; 480],
        }
    }

    fn errors(findings: &[Finding]) -> Vec<&str> {
        findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .map(|f| f.message.as_str())
            .collect()
    }

    #[test]
    fn a_clean_stream_has_no_findings_and_keeps_its_schedule() {
        let mut check = AudioCheck::new(48_000, 2);
        let mut out = Vec::new();
        check.start(0.0, FORMAT, &mut out);
        for i in 0..100u32 {
            let first = u64::from(i) * 240;
            check.packet(
                ClientTime(0),
                &packet(1, i, first, 40 * MS + i64::from(i) * 5 * MS),
                &mut out,
            );
        }
        check.stop(1.0, 1, &mut out);
        assert!(out.is_empty(), "{out:?}");
        let report = check.report();
        assert_eq!(
            (report.packets, report.frames, report.late_packets),
            (100, 24_000, 0)
        );
        assert_eq!(report.spacing_error_ms.count, 99);
        assert!(report.spacing_error_ms.max.abs() < 1e-9);
        assert!(report.schedule_error_ms.max.abs() < 1e-9);
        assert_eq!(report.streams[0].ended_at_s, Some(1.0));
    }

    #[test]
    fn spacing_follows_the_frames_between_packets_and_schedule_error_accumulates() {
        let mut check = AudioCheck::new(48_000, 2);
        let mut out = Vec::new();
        check.start(0.0, FORMAT, &mut out);
        // 5 ms of frames per packet, but stamped 5.1 ms apart; then one packet 2 ms late.
        for i in 0..10u32 {
            let at = 40 * MS + i64::from(i) * 5_100_000 + if i == 9 { 2 * MS } else { 0 };
            check.packet(
                ClientTime(0),
                &packet(1, i, u64::from(i) * 240, at),
                &mut out,
            );
        }
        // A lost packet: the spacing is judged over the frames between the two.
        check.packet(
            ClientTime(0),
            &packet(1, 11, 11 * 240, 40 * MS + 11 * 5_100_000),
            &mut out,
        );
        let report = check.report();
        let spacing = report.spacing_error_ms;
        assert!((spacing.p50 - 0.1).abs() < 1e-9, "{spacing:?}");
        assert!((spacing.max - 2.1).abs() < 1e-9, "{spacing:?}");
        assert!((spacing.min - (-1.8)).abs() < 1e-9, "{spacing:?}");
        assert!((report.schedule_error_ms.max - 2.9).abs() < 1e-9);
        assert_eq!((report.lost_packets, report.missing_frames), (1, 240));
    }

    #[test]
    fn gaps_overlaps_and_strays_are_reported() {
        let mut check = AudioCheck::new(48_000, 2);
        let mut out = Vec::new();
        check.packet(ClientTime(0), &packet(9, 0, 0, 10), &mut out);
        check.packet(ClientTime(0), &packet(9, 1, 240, 10), &mut out);
        assert_eq!(
            errors(&out),
            ["audio packet 0 of stream 9, which was never started"],
            "once per stream"
        );
        out.clear();
        check.start(0.0, FORMAT, &mut out);
        check.packet(ClientTime(0), &packet(1, 0, 0, 10), &mut out);
        check.packet(ClientTime(0), &packet(1, 3, 960, 10), &mut out);
        assert_eq!(
            (errors(&out).len(), out.len()),
            (0, 2),
            "lost packets and frames warn"
        );
        check.packet(ClientTime(0), &packet(1, 4, 500, 10), &mut out);
        assert_eq!(
            errors(&out),
            ["audio stream 1 went back to frame 500 after frames up to 1199"]
        );
        let report = check.report();
        assert_eq!((report.lost_packets, report.missing_frames), (2, 720));
    }

    #[test]
    fn a_stream_ends_with_its_stop_a_newer_stream_or_the_link() {
        let mut check = AudioCheck::new(48_000, 2);
        let mut out = Vec::new();
        check.start(0.0, FORMAT, &mut out);
        check.start(
            1.0,
            AudioFormat {
                stream: 2,
                ..FORMAT
            },
            &mut out,
        );
        assert!(out.is_empty(), "a new stream replaces the old: {out:?}");
        check.packet(ClientTime(0), &packet(1, 0, 0, 0), &mut out);
        check.stop(2.0, 2, &mut out);
        check.packet(ClientTime(0), &packet(2, 0, 0, 0), &mut out);
        check.start(3.0, FORMAT, &mut out);
        check.link_down(4.0);
        check.packet(ClientTime(0), &packet(1, 0, 0, 0), &mut out);
        assert_eq!(
            errors(&out),
            [
                "audio packet 0 of stream 1 after stream 2 replaced it",
                "audio packet 0 of stream 2 after its AudioStop",
                "audio packet 0 of stream 1, which ended with its link",
            ]
        );
        out.clear();
        check.start(5.0, FORMAT, &mut out);
        assert!(
            out.is_empty(),
            "a stream may start again on a new link: {out:?}"
        );
        check.start(6.0, FORMAT, &mut out);
        assert_eq!(
            errors(&out),
            ["audio stream 1 started again without a stop"]
        );
        let ended: Vec<_> = check
            .report()
            .streams
            .iter()
            .map(|s| s.ended_at_s)
            .collect();
        assert_eq!(ended, [Some(1.0), Some(2.0), Some(4.0), Some(6.0), None]);
    }

    #[test]
    fn each_stream_measures_its_tone_across_packets() {
        let mut check = AudioCheck::new(48_000, 2);
        let mut out = Vec::new();
        check.start(0.0, FORMAT, &mut out);
        let tone: Vec<i16> = (0..48_000)
            .flat_map(|n: i32| {
                let v = (0.25
                    * (f64::from(n) * 440.0 / 48_000.0 * std::f64::consts::TAU).sin()
                    * 32768.0) as i16;
                [v, -v]
            })
            .collect();
        for (i, samples) in tone.chunks(480).enumerate() {
            let mut p = packet(1, i as u32, i as u64 * 240, 40 * MS + i as i64 * 5 * MS);
            p.samples = samples.to_vec();
            check.packet(ClientTime(0), &p, &mut out);
        }
        let stream = &check.report().streams[0];
        assert!((stream.frequency_hz - 440.0).abs() < 0.01, "{stream:?}");
        assert!(
            (stream.rms - 0.25 / 2f64.sqrt()).abs() < 0.001,
            "{stream:?}"
        );
    }

    #[test]
    fn audio_for_a_client_without_audio_or_in_another_format_is_an_error() {
        let mut out = Vec::new();
        AudioCheck::new(0, 0).start(0.0, FORMAT, &mut out);
        AudioCheck::new(44_100, 2).start(0.0, FORMAT, &mut out);
        assert_eq!(errors(&out).len(), 2);
    }
}
