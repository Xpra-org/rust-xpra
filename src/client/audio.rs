//! Platform-neutral pieces of Xpra speaker forwarding.
//!
//! Xpra's bare `opus` codec preserves one Opus packet per `audio-data` packet.  The two
//! GStreamer stream headers (`OpusHead` and `OpusTags`) can be bundled in packet field 4, before
//! the payload in field 2.  Keeping parsing and queue policy here makes the protocol testable on
//! every platform; the Media Foundation / WASAPI implementation is in `windows_audio.rs`.

#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::VecDeque;

use serde_json::{Value, json};
use yaml_rust2::Yaml;

use xpra::net::packet::{Packet, yaml_bool, yaml_bytes, yaml_str};

pub const CODEC: &str = "opus";
pub const AUDIO_DATA_PACKET: &str = "audio-data";
pub const AUDIO_CONTROL_PACKET: &str = "audio-control";
pub const AUDIO_CAPABILITIES_PACKET: &str = "audio-capabilities";

pub const SAMPLE_RATE: u32 = 48_000;
pub const START_TARGET_MS: u32 = 120;
pub const MIN_TARGET_MS: u32 = 80;
pub const MAX_TARGET_MS: u32 = 400;
pub const HARD_CAP_MS: u32 = 500;
pub const SYNC_THRESHOLD_MS: u32 = 40;

/// Initial hello contribution. Decoder lists deliberately do not appear here: negotiation is
/// asynchronous so the UI/network handshake never waits for audio initialization.
pub fn hello_capabilities() -> Value {
    json!({ "async": true })
}

pub fn receive_capabilities() -> Value {
    json!({
        "decoders": [CODEC],
        "receive": true,
        "encoders": [],
        "send": false,
    })
}

pub fn av_sync_capabilities() -> Value {
    json!({
        "": true,
        "enabled": true,
        "delay.default": START_TARGET_MS,
        "delay": START_TARGET_MS,
    })
}

pub fn control_packet(command: &str, argument: Value) -> Value {
    json!([AUDIO_CONTROL_PACKET, command, argument])
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OpusHeader {
    pub version: u8,
    pub channels: u8,
    pub pre_skip: u16,
    pub input_sample_rate: u32,
    pub output_gain: i16,
    pub mapping_family: u8,
}

impl OpusHeader {
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < 19 || &data[..8] != b"OpusHead" {
            return Err("invalid OpusHead signature or length".to_string());
        }
        let version = data[8];
        // RFC 7845 reserves versions whose high nibble is non-zero.
        if version & 0xf0 != 0 {
            return Err(format!("unsupported OpusHead version {version}"));
        }
        let channels = data[9];
        if channels == 0 {
            return Err("OpusHead declares zero channels".to_string());
        }
        let mapping_family = data[18];
        if mapping_family == 0 && channels > 2 {
            return Err("Opus mapping family 0 only supports mono or stereo".to_string());
        }
        if mapping_family != 0 {
            // For mapped streams the fixed header is followed by stream count, coupled count and
            // one mapping byte per channel.
            let required = 21usize.saturating_add(channels as usize);
            if data.len() < required {
                return Err("truncated mapped OpusHead".to_string());
            }
        }
        Ok(Self {
            version,
            channels,
            pre_skip: u16::from_le_bytes([data[10], data[11]]),
            input_sample_rate: u32::from_le_bytes([data[12], data[13], data[14], data[15]]),
            output_gain: i16::from_le_bytes([data[16], data[17]]),
            mapping_family,
        })
    }
}

pub fn is_opus_tags(data: &[u8]) -> bool {
    data.starts_with(b"OpusTags")
}

/// Number of decoded 48 kHz frames represented by one complete Opus packet.
pub fn opus_packet_frames(data: &[u8]) -> Option<u32> {
    let toc = *data.first()?;
    let config = toc >> 3;
    let frame_count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        3 => {
            let count = *data.get(1)? & 0x3f;
            if count == 0 {
                return None;
            }
            count as u32
        }
        _ => unreachable!(),
    };
    let per_frame = if config >= 16 {
        // CELT-only: 2.5, 5, 10 or 20ms.
        120u32 << (config & 3)
    } else if config >= 12 {
        // Hybrid: 10 or 20ms.
        480u32 << (config & 1)
    } else {
        // SILK-only: 10, 20, 40 or 60ms.
        [480, 960, 1920, 2880][(config & 3) as usize]
    };
    let total = per_frame.saturating_mul(frame_count);
    (total <= SAMPLE_RATE * 120 / 1000).then_some(total)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioMetadata {
    pub sequence: Option<u64>,
    pub start_of_stream: bool,
    pub end_of_stream: bool,
    pub codec: Option<String>,
    /// GStreamer presentation timestamp, in nanoseconds.
    pub timestamp_ns: Option<i64>,
    /// GStreamer packet duration, in nanoseconds.
    pub duration_ns: Option<i64>,
}

impl AudioMetadata {
    pub fn parse(value: &Yaml) -> Self {
        let Yaml::Hash(hash) = value else {
            return Self::default();
        };
        let get = |name: &str| hash.get(&Yaml::String(name.to_string()));
        let nonnegative = |value: Option<&Yaml>| match value {
            Some(Yaml::Integer(value)) if *value >= 0 => Some(*value as u64),
            _ => None,
        };
        let positive_i64 = |value: Option<&Yaml>| match value {
            Some(Yaml::Integer(value)) if *value >= 0 => Some(*value),
            _ => None,
        };
        Self {
            sequence: nonnegative(get("sequence")),
            start_of_stream: get("start-of-stream").map(yaml_bool).unwrap_or(false),
            end_of_stream: get("end-of-stream").map(yaml_bool).unwrap_or(false),
            codec: get("codec").map(yaml_str).filter(|s| !s.is_empty()),
            timestamp_ns: positive_i64(get("timestamp")),
            duration_ns: positive_i64(get("duration")),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IncomingAudio {
    pub codec: String,
    pub data: Vec<u8>,
    pub metadata: AudioMetadata,
    pub headers: Vec<Vec<u8>>,
}

impl IncomingAudio {
    pub fn parse(packet: &mut Packet) -> Result<Self, String> {
        if packet.len() < 4 {
            return Err("audio-data packet has fewer than four fields".to_string());
        }
        let codec = packet.get_str(1);
        let data = packet.get_bytes(2);
        let metadata = AudioMetadata::parse(&packet.main[3]);
        let headers = if packet.len() > 4 {
            match &packet.main[4] {
                Yaml::Array(values) => values.iter().map(yaml_bytes).collect(),
                // Be liberal for implementations which send one header rather than a sequence.
                value => {
                    let header = yaml_bytes(value);
                    if header.is_empty() { Vec::new() } else { vec![header] }
                }
            }
        } else {
            Vec::new()
        };
        Ok(Self { codec, data, metadata, headers })
    }
}

pub fn is_audio_data_type(packet_type: &str) -> bool {
    matches!(packet_type, AUDIO_DATA_PACKET | "sound-data")
}

pub fn async_requested(hello: &Yaml) -> bool {
    hash_value(hello, "audio")
        .and_then(|audio| hash_value(audio, "async"))
        .map(yaml_bool)
        .unwrap_or(false)
}

pub fn server_av_sync_enabled(hello: &Yaml) -> bool {
    hash_value(hello, "av-sync")
        .and_then(|av| hash_value(av, "enabled").or_else(|| hash_value(av, "")))
        .map(yaml_bool)
        .unwrap_or(false)
}

pub fn server_can_send_opus(capabilities: &Yaml) -> bool {
    let send = hash_value(capabilities, "send").map(yaml_bool).unwrap_or(false);
    let encoders = hash_value(capabilities, "encoders");
    send && encoders
        .and_then(|v| match v { Yaml::Array(v) => Some(v), _ => None })
        .is_some_and(|values| values.iter().any(|v| yaml_str(v) == CODEC))
}

fn hash_value<'a>(value: &'a Yaml, key: &str) -> Option<&'a Yaml> {
    let Yaml::Hash(hash) = value else {
        return None;
    };
    hash.get(&Yaml::String(key.to_string()))
}

#[derive(Clone, Debug)]
pub struct AudioProtocol {
    pub capabilities_sent: bool,
    pub negotiated: bool,
    pub active: bool,
    pub sequence: u64,
    pub header_seen: bool,
    pub server_av_sync: bool,
}

impl Default for AudioProtocol {
    fn default() -> Self {
        Self {
            capabilities_sent: false,
            negotiated: false,
            active: false,
            sequence: 0,
            header_seen: false,
            server_av_sync: false,
        }
    }
}

impl AudioProtocol {
    pub fn accepts_sequence(&self, sequence: Option<u64>) -> bool {
        !matches!(sequence, Some(sequence) if sequence < self.sequence)
    }

    pub fn begin(&mut self, codec: &str, sequence: Option<u64>) -> Result<u64, String> {
        if codec != CODEC {
            return Err(format!("unsupported audio codec {codec:?}"));
        }
        if !self.accepts_sequence(sequence) {
            return Err(format!(
                "old audio sequence {} (current is {})",
                sequence.unwrap_or_default(),
                self.sequence,
            ));
        }
        if let Some(sequence) = sequence {
            self.sequence = sequence;
        }
        self.active = true;
        self.header_seen = false;
        Ok(self.sequence)
    }

    pub fn finish(&mut self) -> u64 {
        self.active = false;
        self.header_seen = false;
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }
}

#[derive(Clone, Debug)]
struct PcmChunk {
    samples: Vec<i16>,
    frame_offset: usize,
}

/// A channel-aware PCM queue used by the Windows worker and unit tests.
#[derive(Clone, Debug)]
pub struct PcmBuffer {
    channels: usize,
    chunks: VecDeque<PcmChunk>,
    frames: usize,
}

impl PcmBuffer {
    pub fn new(channels: u8) -> Self {
        Self {
            channels: channels.max(1) as usize,
            chunks: VecDeque::new(),
            frames: 0,
        }
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn latency_ms(&self) -> u32 {
        (self.frames as u64 * 1000 / SAMPLE_RATE as u64) as u32
    }

    pub fn push(&mut self, mut samples: Vec<i16>) {
        let remainder = samples.len() % self.channels;
        if remainder != 0 {
            samples.truncate(samples.len() - remainder);
        }
        let frames = samples.len() / self.channels;
        if frames != 0 {
            self.frames += frames;
            self.chunks.push_back(PcmChunk { samples, frame_offset: 0 });
        }
    }

    pub fn pop_into(&mut self, destination: &mut [i16], requested_frames: usize) -> usize {
        let want = requested_frames
            .min(self.frames)
            .min(destination.len() / self.channels);
        let mut copied_frames = 0;
        while copied_frames < want {
            let Some(front) = self.chunks.front_mut() else {
                break;
            };
            let available = front.samples.len() / self.channels - front.frame_offset;
            let take = available.min(want - copied_frames);
            let src_start = front.frame_offset * self.channels;
            let src_end = src_start + take * self.channels;
            let dst_start = copied_frames * self.channels;
            destination[dst_start..dst_start + take * self.channels]
                .copy_from_slice(&front.samples[src_start..src_end]);
            front.frame_offset += take;
            copied_frames += take;
            if front.frame_offset == front.samples.len() / self.channels {
                self.chunks.pop_front();
            }
        }
        self.frames -= copied_frames;
        copied_frames
    }

    /// Enforce the 500ms cap. Old audio is removed and the new head is faded in over 5ms.
    pub fn enforce_hard_cap(&mut self) -> usize {
        let cap_frames = SAMPLE_RATE as usize * HARD_CAP_MS as usize / 1000;
        if self.frames <= cap_frames {
            return 0;
        }
        let drop_frames = self.frames - cap_frames;
        self.discard_frames(drop_frames);
        self.fade_in(SAMPLE_RATE as usize * 5 / 1000);
        drop_frames
    }

    fn discard_frames(&mut self, mut frames: usize) {
        let original = frames.min(self.frames);
        while frames != 0 {
            let Some(front) = self.chunks.front_mut() else {
                break;
            };
            let available = front.samples.len() / self.channels - front.frame_offset;
            let take = available.min(frames);
            front.frame_offset += take;
            frames -= take;
            if front.frame_offset == front.samples.len() / self.channels {
                self.chunks.pop_front();
            }
        }
        self.frames -= original - frames;
    }

    fn fade_in(&mut self, fade_frames: usize) {
        let mut remaining = fade_frames.min(self.frames);
        let mut position = 0usize;
        for chunk in &mut self.chunks {
            let available = chunk.samples.len() / self.channels - chunk.frame_offset;
            let take = available.min(remaining);
            for frame in 0..take {
                let gain = (position + frame) as f32 / fade_frames.max(1) as f32;
                let base = (chunk.frame_offset + frame) * self.channels;
                for channel in 0..self.channels {
                    chunk.samples[base + channel] =
                        (chunk.samples[base + channel] as f32 * gain) as i16;
                }
            }
            position += take;
            remaining -= take;
            if remaining == 0 {
                break;
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct AdaptiveJitter {
    target_ms: u32,
    jitter_ms: VecDeque<u32>,
    last_arrival_ms: Option<u64>,
    last_duration_ms: u32,
    stable_since_ms: u64,
}

impl AdaptiveJitter {
    pub fn new(now_ms: u64) -> Self {
        Self {
            target_ms: START_TARGET_MS,
            jitter_ms: VecDeque::new(),
            last_arrival_ms: None,
            last_duration_ms: 0,
            stable_since_ms: now_ms,
        }
    }

    pub fn target_ms(&self) -> u32 {
        self.target_ms
    }

    pub fn observe_packet(&mut self, now_ms: u64, duration_ms: u32) {
        if let Some(last) = self.last_arrival_ms {
            let actual = now_ms.saturating_sub(last).min(u32::MAX as u64) as u32;
            self.jitter_ms.push_back(actual.abs_diff(self.last_duration_ms));
            while self.jitter_ms.len() > 256 {
                self.jitter_ms.pop_front();
            }
        }
        self.last_arrival_ms = Some(now_ms);
        self.last_duration_ms = duration_ms;
    }

    pub fn underrun(&mut self, now_ms: u64) {
        self.target_ms = (self.target_ms + 40).min(MAX_TARGET_MS);
        self.stable_since_ms = now_ms;
    }

    pub fn update_stable(&mut self, now_ms: u64) -> bool {
        if now_ms.saturating_sub(self.stable_since_ms) < 30_000 {
            return false;
        }
        let floor = (MIN_TARGET_MS + 2 * self.p95_jitter_ms()).clamp(MIN_TARGET_MS, MAX_TARGET_MS);
        let old = self.target_ms;
        if self.target_ms > floor {
            self.target_ms = self.target_ms.saturating_sub(10).max(floor);
        }
        self.stable_since_ms = now_ms;
        self.target_ms != old
    }

    pub fn p95_jitter_ms(&self) -> u32 {
        if self.jitter_ms.is_empty() {
            return 0;
        }
        let mut values: Vec<u32> = self.jitter_ms.iter().copied().collect();
        values.sort_unstable();
        let index = ((values.len() - 1) * 95 + 99) / 100;
        values[index.min(values.len() - 1)]
    }

    pub fn playback_rate(&self, queued_ms: u32) -> f32 {
        let error = queued_ms as i64 - self.target_ms as i64;
        if error.abs() <= 20 {
            return 1.0;
        }
        // Reach the ±0.5% cap at 100ms from target, with a deadband around the target.
        let magnitude = ((error.abs() - 20) as f32 / 80.0).min(1.0) * 0.005;
        if error > 0 { 1.0 + magnitude } else { 1.0 - magnitude }
    }
}

#[derive(Clone, Debug, Default)]
pub struct LatencyReporter {
    last_ms: Option<u32>,
}

impl LatencyReporter {
    pub fn update(&mut self, total_ms: u32) -> Option<u32> {
        let changed = self.last_ms
            .map(|last| last.abs_diff(total_ms) >= SYNC_THRESHOLD_MS)
            .unwrap_or(true);
        if changed {
            self.last_ms = Some(total_ms);
            Some(total_ms)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use yaml_rust2::YamlLoader;

    fn yaml(value: &str) -> Yaml {
        YamlLoader::load_from_str(value).unwrap().remove(0)
    }

    #[test]
    fn parses_opus_head() {
        let head = [
            b"OpusHead".as_slice(),
            &[1, 2],
            &312u16.to_le_bytes(),
            &48_000u32.to_le_bytes(),
            &0i16.to_le_bytes(),
            &[0],
        ].concat();
        assert_eq!(
            OpusHeader::parse(&head).unwrap(),
            OpusHeader {
                version: 1,
                channels: 2,
                pre_skip: 312,
                input_sample_rate: 48_000,
                output_gain: 0,
                mapping_family: 0,
            }
        );
        assert!(OpusHeader::parse(b"OpusHead").is_err());
    }

    #[test]
    fn parses_packet_metadata_and_bundled_headers() {
        let mut packet = Packet {
            main: match yaml(
                r#"["audio-data", "opus", "", {
                    sequence: 7, start-of-stream: true,
                    timestamp: 20000000, duration: 20000000
                }, ["T3B1c0hlYWQBAg==", "T3B1c1RhZ3M="]]"#,
            ) {
                Yaml::Array(values) => values,
                _ => unreachable!(),
            },
            raw: HashMap::new(),
            decode_time_us: None,
        };
        let parsed = IncomingAudio::parse(&mut packet).unwrap();
        assert_eq!(parsed.codec, "opus");
        assert_eq!(parsed.metadata.sequence, Some(7));
        assert!(parsed.metadata.start_of_stream);
        assert_eq!(parsed.metadata.duration_ns, Some(20_000_000));
        assert_eq!(parsed.headers[0], b"OpusHead\x01\x02");
        assert_eq!(parsed.headers[1], b"OpusTags");
    }

    #[test]
    fn dispatch_accepts_canonical_and_legacy_alias() {
        assert!(is_audio_data_type(AUDIO_DATA_PACKET));
        assert!(is_audio_data_type("sound-data"));
        assert!(!is_audio_data_type("sound-control"));
    }

    #[test]
    fn negotiation_is_async_and_opus_only() {
        let initial = hello_capabilities();
        assert_eq!(initial, serde_json::json!({"async": true}));
        assert!(initial.get("decoders").is_none());
        assert_eq!(
            receive_capabilities(),
            serde_json::json!({
                "decoders": ["opus"], "receive": true, "encoders": [], "send": false
            }),
        );
        let hello = yaml(r#"{audio: {async: true}, av-sync: {enabled: true}}"#);
        assert!(async_requested(&hello));
        assert!(server_av_sync_enabled(&hello));
        assert!(server_can_send_opus(&yaml(
            r#"{send: true, receive: false, encoders: [opus], decoders: []}"#
        )));
        assert!(!server_can_send_opus(&yaml(
            r#"{send: true, encoders: [mp3]}"#
        )));
    }

    #[test]
    fn outgoing_controls_are_canonical_only() {
        for command in ["start", "stop", "new-sequence", "sync"] {
            let packet = control_packet(command, serde_json::json!(0));
            assert_eq!(packet[0], AUDIO_CONTROL_PACKET);
            assert_ne!(packet[0], "sound-control");
        }
    }

    #[test]
    fn rejects_old_sequences_and_advances_after_eos() {
        let mut protocol = AudioProtocol { negotiated: true, ..AudioProtocol::default() };
        assert_eq!(protocol.begin("opus", Some(4)).unwrap(), 4);
        assert!(protocol.accepts_sequence(Some(4)));
        assert!(!protocol.accepts_sequence(Some(3)));
        assert_eq!(protocol.finish(), 5);
        assert!(!protocol.accepts_sequence(Some(4)));
    }

    #[test]
    fn pcm_accounting_and_hard_cap() {
        let mut pcm = PcmBuffer::new(2);
        pcm.push(vec![1000; SAMPLE_RATE as usize * 2 * 600 / 1000]);
        assert_eq!(pcm.latency_ms(), 600);
        let dropped = pcm.enforce_hard_cap();
        assert_eq!(dropped, SAMPLE_RATE as usize / 10);
        assert_eq!(pcm.latency_ms(), HARD_CAP_MS);
        let mut output = vec![0i16; 480 * 2];
        assert_eq!(pcm.pop_into(&mut output, 480), 480);
        assert_eq!(pcm.frames(), SAMPLE_RATE as usize / 2 - 480);
        assert_eq!(output[0], 0); // fade after dropping the old head
    }

    #[test]
    fn adaptive_target_and_drift_are_bounded() {
        let mut jitter = AdaptiveJitter::new(0);
        jitter.underrun(10);
        assert_eq!(jitter.target_ms(), 160);
        for i in 0..32 {
            jitter.observe_packet(100 + i * 20, 20);
        }
        assert!(jitter.update_stable(30_010));
        assert_eq!(jitter.target_ms(), 150);
        assert_eq!(jitter.playback_rate(150), 1.0);
        assert_eq!(jitter.playback_rate(500), 1.005);
        assert_eq!(jitter.playback_rate(0), 0.995);
    }

    #[test]
    fn reports_latency_initially_and_at_forty_ms_delta() {
        let mut reporter = LatencyReporter::default();
        assert_eq!(reporter.update(0), Some(0));
        assert_eq!(reporter.update(39), None);
        assert_eq!(reporter.update(40), Some(40));
        assert_eq!(reporter.update(1), None);
        assert_eq!(reporter.update(0), Some(0));
    }

    #[test]
    fn opus_toc_duration_is_bounded() {
        // CELT 20ms, one frame.
        assert_eq!(opus_packet_frames(&[31 << 3]), Some(960));
        // Code 3 with 63 20ms frames exceeds Opus' 120ms maximum.
        assert_eq!(opus_packet_frames(&[(31 << 3) | 3, 63]), None);
    }
}
