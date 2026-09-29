//! H.264 encoding with Media Foundation.
//!
//! Encoder choice: hardware MFTs first (NVIDIA NVENC, Intel Quick Sync, AMD
//! AMF register themselves as Media Foundation transforms), sorted by
//! Windows' merit ordering. If none exists or none accepts our
//! configuration, fall back to Microsoft's software "H264 Encoder MFT". The
//! chosen encoder is logged and reported by [`H264Encoder::info`].
//!
//! Output is H.264 Constrained Baseline (no B-frames, CAVLC), low-latency
//! mode, in Annex B. Baseline keeps the stream decodable by the portable
//! OpenH264 decoder the viewer uses on every platform.
//!
//! Bitrate is changed at runtime through `ICodecAPI` ([`H264Encoder::set_bitrate`]),
//! which Phase 9's adaptive bitrate will drive.
//!
//! Hardware MFTs are usually *asynchronous*: they signal "need input" and
//! "have output" through events instead of answering `ProcessInput`/
//! `ProcessOutput` directly. Both models are handled here. The async path
//! could not be exercised on the development VM, which has no GPU encoder.

use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

use crate::media::h264::{self, ParamSets};
use crate::media::nv12::Nv12Frame;

/// How long an async encoder may take to produce output for one input.
const ASYNC_OUTPUT_WAIT: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInfo {
    pub name: String,
    pub hardware: bool,
}

pub struct EncodedAu {
    pub keyframe: bool,
    pub data: Vec<u8>,
}

pub struct H264Encoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    info: EncoderInfo,
    width: u32,
    height: u32,
    fps: u32,
    provides_samples: bool,
    output_size: u32,
    input_credits: u32,
    param_sets: ParamSets,
}

// SAFETY: the encoder is created and used on one thread (the helper's
// capture thread); MF objects here are free-threaded COM objects.
unsafe impl Send for H264Encoder {}

fn startup() -> windows::core::Result<()> {
    // SAFETY: COM + MF initialisation; both are reference counted per process.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_NOSOCKET)
    }
}

fn enum_encoders(flags: MFT_ENUM_FLAG) -> windows::core::Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: MF allocates `array` with CoTaskMemAlloc; we take ownership of
    // each element and free the array.
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            Some(&input),
            Some(&output),
            &mut array,
            &mut count,
        )?;
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            if let Some(a) = (*array.add(i)).take() {
                out.push(a);
            }
        }
        if !array.is_null() {
            CoTaskMemFree(Some(array.cast()));
        }
        Ok(out)
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut name = windows::core::PWSTR::null();
    let mut len = 0u32;
    // SAFETY: MF allocates the string; freed with CoTaskMemFree.
    unsafe {
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut name, &mut len)
            .is_ok()
        {
            let s = name.to_string().unwrap_or_default();
            CoTaskMemFree(Some(name.0.cast()));
            return s;
        }
    }
    "unknown encoder".into()
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

impl H264Encoder {
    /// Create an encoder for `width x height` NV12 input, preferring hardware.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> windows::core::Result<Self> {
        startup()?;
        let hardware =
            enum_encoders(MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER).unwrap_or_default();
        for activate in hardware {
            let name = friendly_name(&activate);
            match Self::configure(&activate, true, width, height, fps, bitrate) {
                Ok(enc) => return Ok(enc),
                Err(e) => tracing::warn!(encoder = %name, "hardware encoder rejected: {e}"),
            }
        }
        tracing::info!("no usable hardware H.264 encoder; using software");
        let mut last_err = None;
        for activate in enum_encoders(MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER)? {
            match Self::configure(&activate, false, width, height, fps, bitrate) {
                Ok(enc) => return Ok(enc),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| windows::core::Error::from(MF_E_TOPO_CODEC_NOT_FOUND)))
    }

    fn configure(
        activate: &IMFActivate,
        hardware: bool,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> windows::core::Result<Self> {
        let name = friendly_name(activate);
        // SAFETY: COM calls on objects we own.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject()?;
            let attributes = transform.GetAttributes().ok();
            let is_async = attributes
                .as_ref()
                .and_then(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).ok())
                .unwrap_or(0)
                != 0;
            if is_async {
                // Async MFTs refuse to be driven until explicitly unlocked.
                attributes
                    .as_ref()
                    .expect("async MFT has attributes")
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                if let Some(a) = &attributes {
                    let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
                }
            }

            // Codec properties first: low-latency mode and B-frames decide
            // the encoder's pipeline depth when the media types are set.
            // Without low latency the software encoder holds several frames
            // back, and on an idle desktop they would never come out.
            let codec: Option<ICodecAPI> = transform.cast().ok();
            if let Some(c) = &codec {
                // Best effort: not every encoder supports every property.
                let _ = c.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ = c.SetValue(
                    &CODECAPI_AVEncCommonRateControlMode,
                    &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
                );
                let _ = c.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(bitrate));
                // Long GOP: keyframes are requested when a viewer joins.
                let _ = c.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps * 60));
                let _ = c.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0u32));
            }

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            transform.SetOutputType(0, &output, 0)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &input, 0)?;

            let stream_info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = stream_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                    | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let events = if is_async {
                Some(transform.cast::<IMFMediaEventGenerator>()?)
            } else {
                None
            };

            tracing::info!(encoder = %name, hardware, async_mft = is_async, width, height, fps, bitrate, "H.264 encoder ready");
            Ok(Self {
                transform,
                codec,
                events,
                info: EncoderInfo { name, hardware },
                width,
                height,
                fps,
                provides_samples,
                output_size: stream_info.cbSize.max(width * height),
                input_credits: 0,
                param_sets: ParamSets::default(),
            })
        }
    }

    pub fn info(&self) -> &EncoderInfo {
        &self.info
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Change the target bitrate while streaming.
    pub fn set_bitrate(&mut self, bits_per_second: u32) -> windows::core::Result<()> {
        let codec = self
            .codec
            .as_ref()
            .ok_or_else(|| windows::core::Error::from(windows::Win32::Foundation::E_NOTIMPL))?;
        // SAFETY: COM call with a stack VARIANT.
        unsafe {
            codec.SetValue(
                &CODECAPI_AVEncCommonMeanBitRate,
                &VARIANT::from(bits_per_second),
            )
        }
    }

    /// Encode one frame. May return zero, one or more access units (encoders
    /// can lag by a frame).
    pub fn encode(
        &mut self,
        frame: &Nv12Frame,
        pts_us: u64,
        force_keyframe: bool,
    ) -> windows::core::Result<Vec<EncodedAu>> {
        debug_assert_eq!((frame.width, frame.height), (self.width, self.height));
        if force_keyframe {
            if let Some(c) = &self.codec {
                // SAFETY: COM call with a stack VARIANT.
                let _ =
                    unsafe { c.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
            }
        }
        let sample = self.input_sample(frame, pts_us)?;
        let raw = if self.events.is_some() {
            self.encode_async(&sample)?
        } else {
            self.encode_sync(&sample)?
        };
        Ok(raw
            .into_iter()
            .map(|data| {
                let data = self.param_sets.fix_up(data);
                EncodedAu {
                    keyframe: h264::is_keyframe(&data),
                    data,
                }
            })
            .collect())
    }

    fn input_sample(&self, frame: &Nv12Frame, pts_us: u64) -> windows::core::Result<IMFSample> {
        // SAFETY: the buffer is locked only while copying into it.
        unsafe {
            let buffer = MFCreateMemoryBuffer(frame.data.len() as u32)?;
            let mut ptr = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(frame.data.as_ptr(), ptr, frame.data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(frame.data.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            // Media Foundation time is in 100 ns units.
            sample.SetSampleTime((pts_us * 10) as i64)?;
            sample.SetSampleDuration(10_000_000 / i64::from(self.fps))?;
            Ok(sample)
        }
    }

    fn encode_sync(&mut self, sample: &IMFSample) -> windows::core::Result<Vec<Vec<u8>>> {
        // SAFETY: COM call on our transform.
        unsafe { self.transform.ProcessInput(0, sample, 0)? };
        let mut out = Vec::new();
        while let Some(au) = self.pull_output()? {
            out.push(au);
        }
        Ok(out)
    }

    fn encode_async(&mut self, sample: &IMFSample) -> windows::core::Result<Vec<Vec<u8>>> {
        let events = self.events.clone().expect("async encoder has events");
        let mut out = Vec::new();
        // Wait until the encoder asks for input, collecting any output first.
        while self.input_credits == 0 {
            // SAFETY: blocking GetEvent on the MFT's own event queue.
            let event = unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))? };
            self.handle_event(&event, &mut out)?;
        }
        self.input_credits -= 1;
        // SAFETY: COM call on our transform.
        unsafe { self.transform.ProcessInput(0, sample, 0)? };

        // Give it a moment to produce this frame's output, without blocking
        // forever (an idle desktop may not send another frame for a while).
        let deadline = Instant::now() + ASYNC_OUTPUT_WAIT;
        let produced_before = out.len();
        while Instant::now() < deadline {
            // SAFETY: non-blocking GetEvent.
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => {
                    self.handle_event(&event, &mut out)?;
                    if out.len() > produced_before && self.input_credits > 0 {
                        break;
                    }
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if out.len() > produced_before {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn handle_event(
        &mut self,
        event: &IMFMediaEvent,
        out: &mut Vec<Vec<u8>>,
    ) -> windows::core::Result<()> {
        // SAFETY: COM call on an event we own.
        let kind = unsafe { event.GetType()? };
        if kind == METransformNeedInput.0 as u32 {
            self.input_credits += 1;
        } else if kind == METransformHaveOutput.0 as u32 {
            if let Some(au) = self.pull_output()? {
                out.push(au);
            }
        }
        Ok(())
    }

    /// One `ProcessOutput` call. `None` when the encoder needs more input.
    fn pull_output(&mut self) -> windows::core::Result<Option<Vec<u8>>> {
        // SAFETY: we allocate the output sample when the MFT does not; the
        // ManuallyDrop fields are dropped explicitly below.
        unsafe {
            let own_sample = if self.provides_samples {
                None
            } else {
                let buffer = MFCreateMemoryBuffer(self.output_size)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                Some(sample)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: std::mem::ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: std::mem::ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = self.transform.ProcessOutput(0, &mut buffers, &mut status);
            let sample = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
            drop(std::mem::ManuallyDrop::take(&mut buffers[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder picked a different output format; accept it.
                    let t = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &t, 0)?;
                    return self.pull_output();
                }
                Err(e) => return Err(e),
            }
            let Some(sample) = sample else {
                return Ok(None);
            };
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            Ok(Some(data))
        }
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: shutting down our own transform; MF is reference counted.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let _ = MFShutdown();
        }
    }
}
