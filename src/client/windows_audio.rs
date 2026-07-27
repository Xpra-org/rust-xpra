//! Windows speaker worker: bare Opus through Media Foundation, then event-driven WASAPI.
//!
//! All COM interfaces in this module are created, used and released on the `audio` thread.  The
//! UI thread only has a bounded `SyncSender`, so a stalled decoder or endpoint can never stall
//! window/input packet handling.

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, info, trace, warn};
use winit::event_loop::EventLoopProxy;
use windows::core::HRESULT;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use xpra::net::packet::Packet;

use super::audio::{
    AdaptiveJitter, LatencyReporter, OpusHeader, PcmBuffer, SAMPLE_RATE, opus_packet_frames,
};

const COMMAND_CAPACITY: usize = 96;
const EVENT_WAIT_MS: u32 = 10;
const DESIRED_ENDPOINT_FRAMES: u32 = SAMPLE_RATE * 20 / 1000;
const MF_E_TRANSFORM_NEED_MORE_INPUT: HRESULT = HRESULT(0xC00D6D72u32 as i32);
const MF_E_TRANSFORM_STREAM_CHANGE: HRESULT = HRESULT(0xC00D6D61u32 as i32);

#[derive(Debug)]
enum Command {
    Reset { sequence: u64 },
    Configure { sequence: u64, header: OpusHeader, opus_head: Vec<u8> },
    Packet {
        sequence: u64,
        data: Vec<u8>,
        timestamp_ns: Option<i64>,
        duration_ns: Option<i64>,
        arrival_ms: u64,
    },
    End { sequence: u64 },
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EnqueueError {
    Full,
    Stopped,
}

pub struct AudioWorker {
    sender: SyncSender<Command>,
    running: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}

impl AudioWorker {
    /// Start the worker and synchronously wait for its decoder/output probe. This runs before the
    /// hello, because failed probes must omit audio capabilities altogether.
    pub fn start(proxy: EventLoopProxy<Packet>) -> Result<Self, String> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = running.clone();
        let join = thread::Builder::new()
            .name("audio".to_string())
            .spawn(move || worker_main(receiver, proxy, ready_tx, worker_running))
            .map_err(|e| format!("failed to start audio worker: {e}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { sender, running, join: Some(join) }),
            Ok(Err(error)) => {
                let _ = join.join();
                Err(error)
            }
            Err(_) => Err("audio startup probe timed out".to_string()),
        }
    }

    pub fn reset(&self, sequence: u64) -> Result<(), EnqueueError> {
        self.send(Command::Reset { sequence })
    }

    pub fn configure(
        &self,
        sequence: u64,
        header: OpusHeader,
        opus_head: Vec<u8>,
    ) -> Result<(), EnqueueError> {
        self.send(Command::Configure { sequence, header, opus_head })
    }

    pub fn packet(
        &self,
        sequence: u64,
        data: Vec<u8>,
        timestamp_ns: Option<i64>,
        duration_ns: Option<i64>,
        arrival_ms: u64,
    ) -> Result<(), EnqueueError> {
        self.send(Command::Packet {
            sequence,
            data,
            timestamp_ns,
            duration_ns,
            arrival_ms,
        })
    }

    pub fn end(&self, sequence: u64) -> Result<(), EnqueueError> {
        self.send(Command::End { sequence })
    }

    fn send(&self, command: Command) -> Result<(), EnqueueError> {
        if !self.running.load(Ordering::Acquire) {
            return Err(EnqueueError::Stopped);
        }
        self.sender.try_send(command).map_err(|error| match error {
            TrySendError::Full(_) => EnqueueError::Full,
            TrySendError::Disconnected(_) => EnqueueError::Stopped,
        })
    }
}

impl Drop for AudioWorker {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = self.sender.try_send(Command::Shutdown);
        // Do not block the UI waiting for a driver/MFT during shutdown. The worker owns all of its
        // resources and will release them when it observes the disconnect or shutdown command.
        self.join.take();
    }
}

fn worker_main(
    receiver: Receiver<Command>,
    proxy: EventLoopProxy<Packet>,
    ready: SyncSender<Result<(), String>>,
    worker_running: Arc<AtomicBool>,
) {
    let com_initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    if !com_initialized {
        let _ = ready.send(Err("COM initialization failed on audio worker".to_string()));
        return;
    }
    let startup = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }
        .map_err(|e| format!("Media Foundation startup failed: {e}"))
        .and_then(|()| probe_audio());
    if let Err(error) = startup {
        let _ = ready.send(Err(error));
        unsafe { CoUninitialize() };
        return;
    }
    let _ = ready.send(Ok(()));
    info!("Windows Opus audio worker started");

    let start = Instant::now();
    let mut pipeline: Option<Pipeline> = None;
    let mut running = true;
    while running && worker_running.load(Ordering::Acquire) {
        if let Some(current) = pipeline.as_ref() {
            if current.playing {
                let signalled = unsafe { WaitForSingleObject(current.output.event, EVENT_WAIT_MS) }
                    == WAIT_OBJECT_0;
                if signalled {
                    if let Err(error) = service_output(&mut pipeline, &proxy, start) {
                        post_failure(&proxy, &error);
                        break;
                    }
                }
            } else {
                // Before Start(), WASAPI does not signal its event. Commands are what grow the
                // rebuffering queue, so wait briefly for one rather than busy-spinning.
                match receiver.recv_timeout(Duration::from_millis(EVENT_WAIT_MS as u64)) {
                    Ok(command) => {
                        if !handle_command(command, &mut pipeline, &proxy, start) {
                            running = false;
                            continue;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
        } else {
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(command) => {
                    if !handle_command(command, &mut pipeline, &proxy, start) {
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }

        while worker_running.load(Ordering::Acquire) {
            match receiver.try_recv() {
                Ok(command) => {
                    if !handle_command(command, &mut pipeline, &proxy, start) {
                        running = false;
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    running = false;
                    break;
                }
            }
        }
        if running && worker_running.load(Ordering::Acquire) {
            if let Err(error) = service_output(&mut pipeline, &proxy, start) {
                post_failure(&proxy, &error);
                break;
            }
        }
    }
    drop(pipeline);
    unsafe { CoUninitialize() };
    debug!("Windows audio worker stopped");
}

fn handle_command(
    command: Command,
    pipeline: &mut Option<Pipeline>,
    proxy: &EventLoopProxy<Packet>,
    start: Instant,
) -> bool {
    match command {
        Command::Shutdown => return false,
        Command::Reset { sequence } => {
            debug!("resetting audio worker for sequence {sequence}");
            *pipeline = None;
            post_latency(proxy, 0);
        }
        Command::Configure { sequence, header, opus_head } => {
            match Pipeline::new(sequence, header, &opus_head, start.elapsed().as_millis() as u64) {
                Ok(value) => {
                    *pipeline = Some(value);
                    post_latency(proxy, 0);
                }
                Err(error) => {
                    post_failure(proxy, &error);
                    return false;
                }
            }
        }
        Command::Packet { sequence, data, timestamp_ns, duration_ns, arrival_ms } => {
            let Some(current) = pipeline.as_mut() else {
                trace!("dropping Opus packet received before OpusHead");
                return true;
            };
            if sequence != current.sequence {
                trace!("dropping Opus packet for old sequence {sequence}");
                return true;
            }
            if let Err(error) =
                current.decode_packet(data, timestamp_ns, duration_ns, arrival_ms)
            {
                post_failure(proxy, &error);
                return false;
            }
        }
        Command::End { sequence } => {
            if pipeline.as_ref().is_some_and(|current| current.sequence == sequence) {
                *pipeline = None;
                post_latency(proxy, 0);
            }
        }
    }
    true
}

fn service_output(
    pipeline: &mut Option<Pipeline>,
    proxy: &EventLoopProxy<Packet>,
    start: Instant,
) -> Result<(), String> {
    let Some(current) = pipeline.as_mut() else {
        return Ok(());
    };
    let now_ms = start.elapsed().as_millis() as u64;
    current.jitter.update_stable(now_ms);
    match current.service_output(now_ms) {
        Ok(total_ms) => {
            if let Some(total_ms) = current.latency_reporter.update(total_ms) {
                post_latency(proxy, total_ms);
            }
            Ok(())
        }
        Err(error) if error.invalidated => {
            warn!("audio output device was invalidated, rebuilding it");
            current.rebuild_output()
                .map_err(|e| format!("failed to recover the audio output device: {e}"))?;
            if let Some(total_ms) = current.latency_reporter.update(current.pcm.latency_ms()) {
                post_latency(proxy, total_ms);
            }
            Ok(())
        }
        Err(error) => Err(error.message),
    }
}

fn post_latency(proxy: &EventLoopProxy<Packet>, latency_ms: u32) {
    let _ = proxy.send_event(Packet {
        main: vec![
            YamlString::value("audio-latency"),
            yaml_rust2::Yaml::Integer(latency_ms as i64),
        ],
        raw: HashMap::new(),
        decode_time_us: None,
    });
}

fn post_failure(proxy: &EventLoopProxy<Packet>, error: &str) {
    let _ = proxy.send_event(Packet {
        main: vec![YamlString::value("audio-worker-failed"), YamlString::value(error)],
        raw: HashMap::new(),
        decode_time_us: None,
    });
}

struct YamlString;
impl YamlString {
    fn value(value: &str) -> yaml_rust2::Yaml {
        yaml_rust2::Yaml::String(value.to_string())
    }
}

fn probe_audio() -> Result<(), String> {
    let decoder = enumerate_opus_decoder()?;
    drop(decoder);
    // Exercise the same stereo, event-driven shared stream used by a normal Xpra stream. This
    // catches missing endpoints and drivers which reject rate adjustment before we advertise.
    let output = WasapiOutput::new(2)?;
    drop(output);
    Ok(())
}

fn enumerate_opus_decoder() -> Result<IMFTransform, String> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_Opus,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Audio,
            guidSubtype: MFAudioFormat_PCM,
        };
        let flags = MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_LOCALMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
        );
        let mut activations: *mut Option<IMFActivate> = ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_AUDIO_DECODER,
            flags,
            Some(&input),
            Some(&output),
            &mut activations,
            &mut count,
        )
        .map_err(|e| format!("failed to enumerate Opus decoders: {e}"))?;
        if activations.is_null() || count == 0 {
            if !activations.is_null() {
                CoTaskMemFree(Some(activations.cast()));
            }
            return Err("Windows Media Foundation has no Opus decoder".to_string());
        }
        let values = slice::from_raw_parts_mut(activations, count as usize);
        let first = values[0]
            .take()
            .ok_or_else(|| "Media Foundation returned an empty Opus decoder activation".to_string());
        let decoder = first.and_then(|activation| {
            activation
                .ActivateObject::<IMFTransform>()
                .map_err(|e| format!("failed to activate Opus decoder: {e}"))
        });
        for value in values {
            value.take();
        }
        CoTaskMemFree(Some(activations.cast()));
        decoder
    }
}

struct OpusDecoder {
    transform: IMFTransform,
    channels: u8,
    pre_skip_frames: usize,
    next_time_100ns: i64,
}

impl OpusDecoder {
    fn new(header: OpusHeader, _opus_head: &[u8]) -> Result<Self, String> {
        if header.channels > 2 {
            return Err(format!(
                "Windows Opus playback currently supports mono/stereo, not {} channels",
                header.channels,
            ));
        }
        let transform = enumerate_opus_decoder()?;
        unsafe {
            let input = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .and_then(|_| input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Opus))
                .and_then(|_| input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, header.channels as u32))
                .and_then(|_| input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE))
                .map_err(|e| format!("setting Opus input media type: {e}"))?;
            transform
                .SetInputType(0, &input, 0)
                .map_err(|e| format!("Opus decoder SetInputType: {e}"))?;

            let channels = header.channels as u32;
            let block_align = channels * 2;
            let output = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
            output
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .and_then(|_| output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM))
                .and_then(|_| output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels))
                .and_then(|_| output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, SAMPLE_RATE))
                .and_then(|_| output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16))
                .and_then(|_| output.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align))
                .and_then(|_| {
                    output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, SAMPLE_RATE * block_align)
                })
                .map_err(|e| format!("setting PCM output media type: {e}"))?;
            transform
                .SetOutputType(0, &output, 0)
                .map_err(|e| format!("Opus decoder SetOutputType(PCM): {e}"))?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|_| transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
                .map_err(|e| format!("starting Opus decoder: {e}"))?;
        }
        Ok(Self {
            transform,
            channels: header.channels,
            pre_skip_frames: header.pre_skip as usize,
            next_time_100ns: 0,
        })
    }

    fn decode(
        &mut self,
        data: &[u8],
        timestamp_ns: Option<i64>,
        duration_ns: Option<i64>,
    ) -> Result<Vec<i16>, String> {
        let frames = opus_packet_frames(data)
            .ok_or_else(|| "invalid Opus packet duration".to_string())?;
        let duration_100ns = duration_ns
            .filter(|duration| *duration > 0)
            .map(|duration| duration / 100)
            .unwrap_or(frames as i64 * 10_000_000 / SAMPLE_RATE as i64);
        let timestamp_100ns = timestamp_ns
            .filter(|timestamp| *timestamp >= 0)
            .map(|timestamp| timestamp / 100)
            .unwrap_or(self.next_time_100ns);
        self.next_time_100ns = timestamp_100ns.saturating_add(duration_100ns);
        let sample = make_input_sample(data, timestamp_100ns, duration_100ns)?;
        unsafe {
            self.transform
                .ProcessInput(0, &sample, 0)
                .map_err(|e| format!("Opus decoder ProcessInput: {e}"))?;
        }
        let mut result = Vec::new();
        loop {
            match self.process_output()? {
                DecoderOutput::NeedMoreInput => break,
                DecoderOutput::StreamChange => {
                    return Err("Opus decoder changed its PCM output format".to_string());
                }
                DecoderOutput::Pcm(mut samples) => result.append(&mut samples),
            }
        }
        if self.pre_skip_frames != 0 && !result.is_empty() {
            let frames = result.len() / self.channels as usize;
            let skip = frames.min(self.pre_skip_frames);
            result.drain(..skip * self.channels as usize);
            self.pre_skip_frames -= skip;
        }
        Ok(result)
    }

    fn process_output(&self) -> Result<DecoderOutput, String> {
        unsafe {
            let info = self.transform
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("Opus decoder GetOutputStreamInfo: {e}"))?;
            let provides = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let maximum_packet_bytes = 5760 * self.channels as u32 * 2;
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default(); 1];
            buffers[0].dwStreamID = 0;
            if !provides {
                buffers[0].pSample =
                    ManuallyDrop::new(Some(alloc_sample(info.cbSize.max(maximum_packet_bytes))?));
            }
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            match result {
                Ok(()) => {
                    let sample = ManuallyDrop::take(&mut buffers[0].pSample)
                        .ok_or_else(|| "Opus decoder produced no PCM sample".to_string())?;
                    Ok(DecoderOutput::Pcm(copy_pcm(&sample)?))
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    drop_output(&mut buffers[0]);
                    Ok(DecoderOutput::NeedMoreInput)
                }
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    drop_output(&mut buffers[0]);
                    Ok(DecoderOutput::StreamChange)
                }
                Err(error) => {
                    drop_output(&mut buffers[0]);
                    Err(format!("Opus decoder ProcessOutput: {error}"))
                }
            }
        }
    }
}

enum DecoderOutput {
    NeedMoreInput,
    StreamChange,
    Pcm(Vec<i16>),
}

struct Pipeline {
    sequence: u64,
    channels: u8,
    decoder: OpusDecoder,
    output: WasapiOutput,
    pcm: PcmBuffer,
    jitter: AdaptiveJitter,
    latency_reporter: LatencyReporter,
    playing: bool,
}

impl Pipeline {
    fn new(sequence: u64, header: OpusHeader, opus_head: &[u8], now_ms: u64)
        -> Result<Self, String>
    {
        let decoder = OpusDecoder::new(header, opus_head)?;
        let output = WasapiOutput::new(header.channels)?;
        Ok(Self {
            sequence,
            channels: header.channels,
            decoder,
            output,
            pcm: PcmBuffer::new(header.channels),
            jitter: AdaptiveJitter::new(now_ms),
            latency_reporter: LatencyReporter::default(),
            playing: false,
        })
    }

    fn decode_packet(
        &mut self,
        data: Vec<u8>,
        timestamp_ns: Option<i64>,
        duration_ns: Option<i64>,
        arrival_ms: u64,
    ) -> Result<(), String> {
        let packet_frames = opus_packet_frames(&data);
        let duration_ms = duration_ns
            .filter(|duration| *duration > 0)
            .map(|duration| (duration / 1_000_000) as u32)
            .or_else(|| packet_frames.map(|frames| frames * 1000 / SAMPLE_RATE))
            .unwrap_or(20);
        self.jitter.observe_packet(arrival_ms, duration_ms);
        let samples = self.decoder.decode(&data, timestamp_ns, duration_ns)?;
        self.pcm.push(samples);
        let dropped = self.pcm.enforce_hard_cap();
        if dropped != 0 {
            warn!(
                "audio PCM hard cap reached, discarded {}ms",
                dropped as u64 * 1000 / SAMPLE_RATE as u64,
            );
        }
        Ok(())
    }

    fn service_output(&mut self, now_ms: u64) -> Result<u32, OutputError> {
        if !self.playing && self.pcm.latency_ms() >= self.jitter.target_ms() {
            self.output.write_from(&mut self.pcm)?;
            self.output.start()?;
            self.playing = true;
        } else if self.playing {
            let available = self.output.available_frames()?;
            if available != 0 && self.pcm.frames() < available as usize {
                self.output.stop_and_reset()?;
                self.playing = false;
                self.jitter.underrun(now_ms);
                debug!(
                    "audio underrun: rebuffering to {}ms",
                    self.jitter.target_ms(),
                );
            } else if available != 0 {
                self.output.write_frames(&mut self.pcm, available)?;
            }
        }
        let padding_ms = self.output.padding_ms()?;
        let total_ms = self.pcm.latency_ms().saturating_add(padding_ms);
        self.output.set_rate(self.jitter.playback_rate(total_ms))?;
        Ok(total_ms)
    }

    fn rebuild_output(&mut self) -> Result<(), String> {
        self.output = WasapiOutput::new(self.channels)?;
        self.playing = false;
        Ok(())
    }
}

#[derive(Debug)]
struct OutputError {
    invalidated: bool,
    message: String,
}

impl OutputError {
    fn from_windows(context: &str, error: windows::core::Error) -> Self {
        Self {
            invalidated: error.code() == AUDCLNT_E_DEVICE_INVALIDATED,
            message: format!("{context}: {error}"),
        }
    }
}

struct WasapiOutput {
    client: IAudioClient3,
    render: IAudioRenderClient,
    adjustment: Option<IAudioClockAdjustment>,
    event: HANDLE,
    buffer_frames: u32,
    channels: u8,
    last_rate: f32,
}

impl WasapiOutput {
    fn new(channels: u8) -> Result<Self, String> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| format!("creating audio endpoint enumerator: {e}"))?;
            let endpoint = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| format!("opening default audio output: {e}"))?;
            let client: IAudioClient3 = endpoint
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("activating WASAPI output: {e}"))?;
            let block_align = channels as u16 * 2;
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: channels as u16,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * block_align as u32,
                nBlockAlign: block_align,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let event = CreateEventW(None, false, false, None)
                .map_err(|e| format!("creating WASAPI render event: {e}"))?;
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_RATEADJUST
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
                | AUDCLNT_STREAMFLAGS_NOPERSIST;
            let mut default_period = 0u32;
            let mut fundamental_period = 0u32;
            let mut minimum_period = 0u32;
            let mut maximum_period = 0u32;
            client.GetSharedModeEnginePeriod(
                &format,
                &mut default_period,
                &mut fundamental_period,
                &mut minimum_period,
                &mut maximum_period,
            ).map_err(|e| format!("querying shared WASAPI engine periods: {e}"))?;
            let fundamental_period = fundamental_period.max(1);
            let desired = DESIRED_ENDPOINT_FRAMES.clamp(minimum_period, maximum_period);
            let period_frames = (((desired + fundamental_period / 2) / fundamental_period)
                * fundamental_period)
                .clamp(minimum_period, maximum_period);
            debug!(
                "WASAPI shared period: requested={} frames, using={} (default={})",
                DESIRED_ENDPOINT_FRAMES,
                period_frames,
                default_period,
            );
            if let Err(error) = client.InitializeSharedAudioStream(
                flags,
                period_frames,
                &format,
                None,
            ) {
                let _ = CloseHandle(event);
                return Err(format!("initializing shared WASAPI output: {error}"));
            }
            if let Err(error) = client.SetEventHandle(event) {
                let _ = CloseHandle(event);
                return Err(format!("setting WASAPI render event: {error}"));
            }
            let buffer_frames = client
                .GetBufferSize()
                .map_err(|e| format!("reading WASAPI buffer size: {e}"))?;
            let render = client
                .GetService::<IAudioRenderClient>()
                .map_err(|e| format!("getting WASAPI render service: {e}"))?;
            // Clock adjustment is optional: endpoint/driver combinations which reject it still
            // get the full jitter buffer and event-driven rendering path.
            let adjustment = client.GetService::<IAudioClockAdjustment>().ok();
            Ok(Self {
                client,
                render,
                adjustment,
                event,
                buffer_frames,
                channels,
                last_rate: 1.0,
            })
        }
    }

    fn start(&self) -> Result<(), OutputError> {
        unsafe { self.client.Start() }
            .map_err(|e| OutputError::from_windows("starting WASAPI output", e))
    }

    fn stop_and_reset(&self) -> Result<(), OutputError> {
        unsafe { self.client.Stop() }
            .and_then(|_| unsafe { self.client.Reset() })
            .map_err(|e| OutputError::from_windows("resetting WASAPI output", e))
    }

    fn available_frames(&self) -> Result<u32, OutputError> {
        let padding = unsafe { self.client.GetCurrentPadding() }
            .map_err(|e| OutputError::from_windows("reading WASAPI padding", e))?;
        Ok(self.buffer_frames.saturating_sub(padding))
    }

    fn padding_ms(&self) -> Result<u32, OutputError> {
        let padding = unsafe { self.client.GetCurrentPadding() }
            .map_err(|e| OutputError::from_windows("reading WASAPI padding", e))?;
        Ok((padding as u64 * 1000 / SAMPLE_RATE as u64) as u32)
    }

    fn write_from(&self, pcm: &mut PcmBuffer) -> Result<(), OutputError> {
        let available = self.available_frames()?;
        self.write_frames(pcm, available.min(pcm.frames() as u32))
    }

    fn write_frames(&self, pcm: &mut PcmBuffer, frames: u32) -> Result<(), OutputError> {
        if frames == 0 {
            return Ok(());
        }
        unsafe {
            let output = self.render
                .GetBuffer(frames)
                .map_err(|e| OutputError::from_windows("locking WASAPI render buffer", e))?;
            let samples = slice::from_raw_parts_mut(
                output.cast::<i16>(),
                frames as usize * self.channels as usize,
            );
            let written = pcm.pop_into(samples, frames as usize);
            if written < frames as usize {
                samples[written * self.channels as usize..].fill(0);
            }
            self.render
                .ReleaseBuffer(frames, 0)
                .map_err(|e| OutputError::from_windows("releasing WASAPI render buffer", e))
        }
    }

    fn set_rate(&mut self, rate: f32) -> Result<(), OutputError> {
        let Some(adjustment) = &self.adjustment else {
            return Ok(());
        };
        if (rate - self.last_rate).abs() < 0.0001 {
            return Ok(());
        }
        unsafe { adjustment.SetSampleRate(SAMPLE_RATE as f32 * rate) }
            .map_err(|e| OutputError::from_windows("adjusting WASAPI playback rate", e))?;
        self.last_rate = rate;
        Ok(())
    }
}

impl Drop for WasapiOutput {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn make_input_sample(
    data: &[u8],
    timestamp_100ns: i64,
    duration_100ns: i64,
) -> Result<IMFSample, String> {
    unsafe {
        let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
        let buffer = MFCreateMemoryBuffer(data.len().max(1) as u32)
            .map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
        let mut output = ptr::null_mut();
        buffer
            .Lock(&mut output, None, None)
            .map_err(|e| format!("locking Opus input buffer: {e}"))?;
        ptr::copy_nonoverlapping(data.as_ptr(), output, data.len());
        let _ = buffer.Unlock();
        buffer
            .SetCurrentLength(data.len() as u32)
            .map_err(|e| format!("setting Opus input length: {e}"))?;
        sample.AddBuffer(&buffer).map_err(|e| format!("AddBuffer: {e}"))?;
        sample.SetSampleTime(timestamp_100ns)
            .and_then(|_| sample.SetSampleDuration(duration_100ns))
            .map_err(|e| format!("timing Opus input sample: {e}"))?;
        Ok(sample)
    }
}

fn alloc_sample(size: u32) -> Result<IMFSample, String> {
    unsafe {
        let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
        let buffer = MFCreateMemoryBuffer(size.max(1))
            .map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
        sample.AddBuffer(&buffer).map_err(|e| format!("AddBuffer: {e}"))?;
        Ok(sample)
    }
}

fn copy_pcm(sample: &IMFSample) -> Result<Vec<i16>, String> {
    unsafe {
        let buffer = sample
            .ConvertToContiguousBuffer()
            .map_err(|e| format!("coalescing decoded PCM: {e}"))?;
        let mut data = ptr::null_mut();
        let mut length = 0u32;
        buffer
            .Lock(&mut data, None, Some(&mut length))
            .map_err(|e| format!("locking decoded PCM: {e}"))?;
        let sample_count = length as usize / 2;
        let source = slice::from_raw_parts(data.cast::<i16>(), sample_count);
        let output = source.to_vec();
        let _ = buffer.Unlock();
        Ok(output)
    }
}

fn drop_output(buffer: &mut MFT_OUTPUT_DATA_BUFFER) {
    unsafe {
        ManuallyDrop::drop(&mut buffer.pSample);
        buffer.pSample = ManuallyDrop::new(None);
        if buffer.pEvents.is_some() {
            ManuallyDrop::drop(&mut buffer.pEvents);
            buffer.pEvents = ManuallyDrop::new(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured from Xpra's bare-opus GStreamer path (`opusenc`, stereo 48kHz, 20ms frames).
    const OPUS_HEAD: &[u8] = &[
        0x4f, 0x70, 0x75, 0x73, 0x48, 0x65, 0x61, 0x64, 0x01, 0x02,
        0x38, 0x01, 0x80, 0xbb, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    const OPUS_PACKET_HEX: &str = concat!(
        "fc7f57104d248db457040b87827c688992554a2048123b44022943a9f8372fb9",
        "4e4cee0ad31097744499dbb3cbd86700cf9ba066737c70169fddf688a74b7d57",
        "c7623fc7a6d5bf25297c338a83a7a7bf502af7e04cbdc345de419e0cde03f853",
        "14a8a1854509db6f8bbec592d57bc7bcab8bfb9ac19b23d4435be048d3e41391",
        "5cc954686083d63e4fa83f2b60000f03ffc3f333a3261363b90244252ad7698fd",
        "769f59d361f5a0023773aef7b1e73f5e0ef67fef518de9147ee9d463e4641b6e",
        "3f58a29b3dc692256e07a1ec34e61793c2d867eb734bc1f7da40fb3b87b77e4f",
        "02c2a458ddd191570a5a9ca1def69e93c3892c72318149f898a1a2a999bfc000",
        "06c03c3d7c04ad225",
    );

    fn decode_hex(value: &str) -> Vec<u8> {
        value.as_bytes().chunks_exact(2).map(|pair| {
            let digit = |value: u8| match value {
                b'0'..=b'9' => value - b'0',
                b'a'..=b'f' => value - b'a' + 10,
                _ => panic!("invalid hex digit"),
            };
            digit(pair[0]) << 4 | digit(pair[1])
        }).collect()
    }

    #[test]
    fn discovers_mft_and_decodes_captured_xpra_opus() {
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.expect("Media Foundation unavailable");
        let header = OpusHeader::parse(OPUS_HEAD).unwrap();
        let mut decoder = OpusDecoder::new(header, OPUS_HEAD)
            .expect("Windows Opus decoder MFT unavailable");
        let pcm = decoder
            .decode(&decode_hex(OPUS_PACKET_HEX), Some(0), Some(20_000_000))
            .expect("captured bare Opus packet did not decode");
        assert!(!pcm.is_empty());
        if initialized {
            unsafe { CoUninitialize() };
        }
    }
}
