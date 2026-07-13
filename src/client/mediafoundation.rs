// Windows-only H.264 video decoder built on Media Foundation.
//
// Pipeline: xpra sends h264 as an Annex-B elementary stream (in-band SPS/PPS). We feed each
// encoded frame to the stock H.264 decoder MFT (`CLSID_CMSH264DecoderMFT`), which outputs NV12,
// then run that through the Video Processor MFT (`CLSID_VideoProcessorMFT`) to convert straight to
// RGB32 -- which, in memory, is exactly softbuffer's BGRX byte order (B,G,R,X little-endian), so
// `window::paint` can treat it identically to turbojpeg's BGRA output. No third-party codec is
// linked: the decoder itself lives in the OS (`mfplat.dll`/`msmpeg2vdec.dll`) and is frequently
// GPU-accelerated.
//
// One `H264Decoder` is stateful and lives per xpra window (H.264 is inter-frame predicted), on the
// decode thread only -- these COM objects never cross threads, so nothing here needs to be `Send`.
//
// NOTE: this compiles and the pipeline is wired per the MF docs, but it has NOT been verified
// against a live xpra server yet. The most likely things to need tuning against real frames are
// (a) colour matrix / range (we let the Video Processor pick BT.601/709 by frame size and assume
// limited range -- xpra can send `full-range` in the draw options), and (b) the assumption that
// the stream is Annex-B with in-band headers. Both are noted inline.

use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::Once;

use log::trace;
use windows::core::{GUID, HRESULT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};

// CLSIDs of the stock MFTs (not always surfaced as constants in the `windows` metadata, so define
// them explicitly): H.264 decoder and the video processor / colour converter.
const CLSID_CMSH264DECODER_MFT: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);
const CLSID_VIDEO_PROCESSOR_MFT: GUID = GUID::from_u128(0x88753b26_5b24_49bd_b2e7_0c445c78c982);

// HRESULTs returned by IMFTransform::ProcessOutput -- defined here so we don't depend on whether a
// given `windows` version re-exports them.
const MF_E_TRANSFORM_NEED_MORE_INPUT: HRESULT = HRESULT(0xC00D6D72u32 as i32);
const MF_E_TRANSFORM_STREAM_CHANGE: HRESULT = HRESULT(0xC00D6D61u32 as i32);

static MF_STARTUP: Once = Once::new();

fn ensure_mf_started() {
    // COM must be initialised on this (the decode) thread; MFStartup is process-wide. Neither is
    // ever torn down -- the decode thread lives for the whole process.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    MF_STARTUP.call_once(|| unsafe {
        MFStartup(MF_VERSION, MFSTARTUP_FULL).expect("MFStartup failed");
    });
}

pub struct H264Decoder {
    decoder: IMFTransform,   // H.264 Annex-B -> NV12
    processor: IMFTransform, // NV12 -> RGB32 (BGRA)
    started: bool,           // decoder input/output types configured + streaming started
    frame: i64,              // running sample timestamp (100ns units), just a monotonic counter
    vp_dirty: bool,          // Video Processor types need (re)building (dims changed / stream change)
    out_w: u32,              // target output size the Video Processor is currently configured for
    out_h: u32,
    full_range: bool,        // colour range of the NV12 (from xpra's `full-range`); MF's H.264
                             // decoder doesn't reliably surface the VUI flag, and xpra's encoders
                             // default to full range, so we track it ourselves and default to true.
}

impl H264Decoder {
    pub fn new() -> Result<Self, String> {
        ensure_mf_started();
        let decoder: IMFTransform = unsafe {
            CoCreateInstance(&CLSID_CMSH264DECODER_MFT, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("failed to create H.264 decoder MFT: {e}"))?
        };
        let processor: IMFTransform = unsafe {
            CoCreateInstance(&CLSID_VIDEO_PROCESSOR_MFT, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("failed to create Video Processor MFT: {e}"))?
        };
        Ok(H264Decoder {
            decoder,
            processor,
            started: false,
            frame: 0,
            vp_dirty: true,
            out_w: 0,
            out_h: 0,
            full_range: true,
        })
    }

    // Decode one encoded frame. Returns Ok(Some(bgra)) when a frame is ready (exactly w*h*4 bytes),
    // Ok(None) when the input was consumed but no frame is available yet (decoder warm-up) -- the
    // caller must still ack the draw sequence -- or Err on failure.
    pub fn decode(&mut self, data: &[u8], w: u32, h: u32, full_range: Option<bool>)
        -> Result<Option<Vec<u8>>, String>
    {
        if w == 0 || h == 0 {
            return Err("invalid zero frame size".to_string());
        }
        // `full_range` is only present on transitions/keyframes; None means "unchanged". A change
        // forces the Video Processor to be rebuilt with the new nominal range.
        if let Some(fr) = full_range {
            if fr != self.full_range {
                self.full_range = fr;
                self.vp_dirty = true;
            }
        }
        if !self.started {
            self.configure_decoder(w, h)?;
            self.started = true;
        }

        let sample = self.make_input_sample(data)?;
        unsafe {
            self.decoder
                .ProcessInput(0, &sample, 0)
                .map_err(|e| format!("decoder ProcessInput failed: {e}"))?;
        }

        // Drain all output the decoder can currently produce; keep only the most recent frame
        // (dropping any older buffered frames is fine -- we only present the latest, and ack once).
        let mut latest: Option<Vec<u8>> = None;
        loop {
            match self.decoder_process_output()? {
                DecoderOutput::NeedMoreInput => break,
                DecoderOutput::StreamChange => {
                    // The decoder learned the real format/size from the bitstream: reset its NV12
                    // output type and rebuild the Video Processor to match.
                    self.set_decoder_output_nv12()?;
                    self.vp_dirty = true;
                    continue;
                }
                DecoderOutput::Frame(nv12) => {
                    self.ensure_processor(w, h)?;
                    latest = Some(self.convert_to_bgra(&nv12, w, h)?);
                }
            }
        }
        Ok(latest)
    }

    // ---- decoder configuration -------------------------------------------------------------

    fn configure_decoder(&mut self, w: u32, h: u32) -> Result<(), String> {
        unsafe {
            let input = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
            input
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264))
                .and_then(|_| input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))
                .and_then(|_| input.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(w, h)))
                .map_err(|e| format!("setting H.264 input type: {e}"))?;
            self.decoder
                .SetInputType(0, &input, 0)
                .map_err(|e| format!("decoder SetInputType: {e}"))?;
        }
        self.set_decoder_output_nv12()?;
        unsafe {
            self.decoder
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .and_then(|_| self.decoder.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0))
                .map_err(|e| format!("decoder begin streaming: {e}"))?;
        }
        Ok(())
    }

    // Pick the decoder's NV12 output type from the ones it offers (it must have an input type set
    // first). Called again after every MF_E_TRANSFORM_STREAM_CHANGE.
    fn set_decoder_output_nv12(&mut self) -> Result<(), String> {
        unsafe {
            let mut i = 0u32;
            loop {
                let candidate = match self.decoder.GetOutputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(_) => return Err("decoder offers no NV12 output type".to_string()),
                };
                let subtype = candidate
                    .GetGUID(&MF_MT_SUBTYPE)
                    .map_err(|e| format!("reading output subtype: {e}"))?;
                if subtype == MFVideoFormat_NV12 {
                    self.decoder
                        .SetOutputType(0, &candidate, 0)
                        .map_err(|e| format!("decoder SetOutputType(NV12): {e}"))?;
                    return Ok(());
                }
                i += 1;
            }
        }
    }

    fn decoder_process_output(&mut self) -> Result<DecoderOutput, String> {
        unsafe {
            let info = self
                .decoder
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("decoder GetOutputStreamInfo: {e}"))?;
            let provides = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;

            let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default(); 1];
            buffers[0].dwStreamID = 0;
            if !provides {
                let sample = alloc_sample(info.cbSize)?;
                buffers[0].pSample = ManuallyDrop::new(Some(sample));
            }
            let mut status = 0u32;
            let hr = self.decoder.ProcessOutput(0, &mut buffers, &mut status);
            match hr {
                Ok(()) => {
                    let sample = ManuallyDrop::take(&mut buffers[0].pSample)
                        .ok_or_else(|| "decoder produced no sample".to_string())?;
                    Ok(DecoderOutput::Frame(sample))
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    drop_output(&mut buffers[0]);
                    Ok(DecoderOutput::NeedMoreInput)
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    drop_output(&mut buffers[0]);
                    Ok(DecoderOutput::StreamChange)
                }
                Err(e) => {
                    drop_output(&mut buffers[0]);
                    Err(format!("decoder ProcessOutput: {e}"))
                }
            }
        }
    }

    // ---- video processor (NV12 -> BGRA) ----------------------------------------------------

    fn ensure_processor(&mut self, w: u32, h: u32) -> Result<(), String> {
        if !self.vp_dirty && self.out_w == w && self.out_h == h {
            return Ok(());
        }
        unsafe {
            // Input = whatever NV12 type the decoder is currently producing (carries the real,
            // possibly padded, decoded frame size).
            let in_type = self
                .decoder
                .GetOutputCurrentType(0)
                .map_err(|e| format!("decoder GetOutputCurrentType: {e}"))?;
            // Tell the Video Processor the true colour range rather than relying on whatever (if
            // anything) the H.264 decoder copied from the VUI -- otherwise full-range content
            // (xpra's default) gets treated as studio-range and comes out washed-out.
            let nominal_range = if self.full_range {
                MFNominalRange_0_255
            } else {
                MFNominalRange_16_235
            };
            in_type
                .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, nominal_range.0 as u32)
                .map_err(|e| format!("setting nominal range: {e}"))?;
            self.processor
                .SetInputType(0, &in_type, 0)
                .map_err(|e| format!("processor SetInputType(NV12): {e}"))?;

            // Output = RGB32 at exactly the target window size, top-down and tightly packed so the
            // copy-out is a straight memcpy and paint() gets exactly w*h*4 bytes.
            let out = MFCreateMediaType().map_err(|e| format!("MFCreateMediaType: {e}"))?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .and_then(|_| out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32))
                .and_then(|_| out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32))
                .and_then(|_| out.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(w, h)))
                .and_then(|_| out.SetUINT32(&MF_MT_DEFAULT_STRIDE, w * 4))
                .map_err(|e| format!("setting RGB32 output type: {e}"))?;
            self.processor
                .SetOutputType(0, &out, 0)
                .map_err(|e| format!("processor SetOutputType(RGB32): {e}"))?;

            self.processor
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| format!("processor begin streaming: {e}"))?;
        }
        self.out_w = w;
        self.out_h = h;
        self.vp_dirty = false;
        Ok(())
    }

    fn convert_to_bgra(&mut self, nv12: &IMFSample, w: u32, h: u32) -> Result<Vec<u8>, String> {
        unsafe {
            self.processor
                .ProcessInput(0, nv12, 0)
                .map_err(|e| format!("processor ProcessInput: {e}"))?;

            let info = self
                .processor
                .GetOutputStreamInfo(0)
                .map_err(|e| format!("processor GetOutputStreamInfo: {e}"))?;
            let provides = (info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32) != 0;
            let cb = (w * h * 4).max(info.cbSize);

            let mut buffers = [MFT_OUTPUT_DATA_BUFFER::default(); 1];
            buffers[0].dwStreamID = 0;
            if !provides {
                buffers[0].pSample = ManuallyDrop::new(Some(alloc_sample(cb)?));
            }
            let mut status = 0u32;
            let hr = self.processor.ProcessOutput(0, &mut buffers, &mut status);
            if let Err(e) = hr {
                drop_output(&mut buffers[0]);
                if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT {
                    return Err("processor unexpectedly wants more input".to_string());
                }
                return Err(format!("processor ProcessOutput: {e}"));
            }
            let sample = ManuallyDrop::take(&mut buffers[0].pSample)
                .ok_or_else(|| "processor produced no sample".to_string())?;

            let buffer = sample
                .ConvertToContiguousBuffer()
                .map_err(|e| format!("ConvertToContiguousBuffer: {e}"))?;
            let mut ptr: *mut u8 = ptr::null_mut();
            let mut cur: u32 = 0;
            buffer
                .Lock(&mut ptr, None, Some(&mut cur))
                .map_err(|e| format!("locking output buffer: {e}"))?;
            let want = (w * h * 4) as usize;
            let avail = (cur as usize).min(want);
            let mut out = vec![0u8; want];
            ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), avail);
            let _ = buffer.Unlock();
            trace!("convert_to_bgra: {}x{} -> {} bytes", w, h, out.len());
            Ok(out)
        }
    }

    fn make_input_sample(&mut self, data: &[u8]) -> Result<IMFSample, String> {
        unsafe {
            let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
            let buffer = MFCreateMemoryBuffer(data.len() as u32)
                .map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
            let mut ptr: *mut u8 = ptr::null_mut();
            buffer
                .Lock(&mut ptr, None, None)
                .map_err(|e| format!("locking input buffer: {e}"))?;
            ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            let _ = buffer.Unlock();
            buffer
                .SetCurrentLength(data.len() as u32)
                .map_err(|e| format!("SetCurrentLength: {e}"))?;
            sample.AddBuffer(&buffer).map_err(|e| format!("AddBuffer: {e}"))?;
            // xpra sends ~one encoded frame per draw packet with no B-frames; timestamps only need
            // to be monotonic. Use a nominal 60fps spacing in 100ns units.
            let pts = self.frame * 166_667;
            self.frame += 1;
            let _ = sample.SetSampleTime(pts);
            let _ = sample.SetSampleDuration(166_667);
            Ok(sample)
        }
    }
}

enum DecoderOutput {
    NeedMoreInput,
    StreamChange,
    Frame(IMFSample),
}

fn pack_size(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | (h as u64)
}

fn alloc_sample(size: u32) -> Result<IMFSample, String> {
    unsafe {
        let sample = MFCreateSample().map_err(|e| format!("MFCreateSample: {e}"))?;
        let buffer =
            MFCreateMemoryBuffer(size.max(1)).map_err(|e| format!("MFCreateMemoryBuffer: {e}"))?;
        sample.AddBuffer(&buffer).map_err(|e| format!("AddBuffer: {e}"))?;
        Ok(sample)
    }
}

// Release the sample/events an MFT_OUTPUT_DATA_BUFFER may still hold after a non-Ok ProcessOutput.
fn drop_output(buf: &mut MFT_OUTPUT_DATA_BUFFER) {
    unsafe {
        ManuallyDrop::drop(&mut buf.pSample);
        buf.pSample = ManuallyDrop::new(None);
        if buf.pEvents.is_some() {
            ManuallyDrop::drop(&mut buf.pEvents);
            buf.pEvents = ManuallyDrop::new(None);
        }
    }
}
