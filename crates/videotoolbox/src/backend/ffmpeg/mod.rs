//! FFmpeg's H.264 decoder with a hardware API underneath: one backend each
//! for Vulkan Video, VA-API, CUDA (NVDEC) and VDPAU.
//!
//! FFmpeg keeps the reference pictures and does the reordering; this side
//! feeds it Annex B access units, refuses its software fallback (a hardware
//! backend that quietly decodes on the CPU is not what was asked for), and
//! copies each picture back to main memory as NV12.

pub(crate) mod abi;
pub(crate) mod api;

use std::ffi::{CString, c_int, c_void};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use self::api::Api;
use super::{
    AccessUnit, Backend, Colour, DecodeError, Decoder, Picture, Planes, StreamConfig, check_stream,
};

type Ptr = *mut c_void;

pub(crate) struct FfmpegBackend {
    /// FFmpeg's name for the device type, which is also the backend's.
    name: &'static str,
    api: fn() -> Result<&'static Api, &'static str>,
    device: OnceLock<Result<Device, String>>,
    unhealthy: AtomicBool,
}

pub(crate) static VULKAN: FfmpegBackend = FfmpegBackend::new("vulkan");
pub(crate) static VAAPI: FfmpegBackend = FfmpegBackend::new("vaapi");
pub(crate) static CUDA: FfmpegBackend = FfmpegBackend::new("cuda");
pub(crate) static VDPAU: FfmpegBackend = FfmpegBackend::new("vdpau");

/// A hardware device that decoded the probe clip, shared by every session
/// on this backend for the rest of the process.
struct Device {
    /// `AVBufferRef *` of the `AVHWDeviceContext`, never released.
    buffer: usize,
    /// `const AVCodec *` of FFmpeg's `h264` decoder.
    codec: usize,
    /// The hardware pixel format decoded pictures are in.
    hw_format: c_int,
    /// `AV_PIX_FMT_NV12`, which pictures are copied back as.
    nv12: c_int,
    /// Which device it is, for messages.
    label: String,
}

impl FfmpegBackend {
    const fn new(name: &'static str) -> FfmpegBackend {
        FfmpegBackend {
            name,
            api: api::global,
            device: OnceLock::new(),
            unhealthy: AtomicBool::new(false),
        }
    }

    /// A backend whose library is never there.
    #[cfg(test)]
    pub(crate) fn bogus() -> &'static FfmpegBackend {
        Box::leak(Box::new(FfmpegBackend {
            api: api::bogus,
            ..FfmpegBackend::new("vulkan")
        }))
    }

    fn device(&self) -> Result<(&'static Api, &Device), String> {
        let api = (self.api)().map_err(str::to_string)?;
        let device = self.device.get_or_init(|| self.probe(api));
        device.as_ref().map(|d| (api, d)).map_err(Clone::clone)
    }

    /// The device to decode on, tried on the probe clip.
    fn probe(&self, api: &'static Api) -> Result<Device, String> {
        let name = CString::new(self.name).unwrap_or_default();
        // SAFETY: resolved entry points and NUL-terminated names.
        let (device_type, codec, nv12) = unsafe {
            (
                (api.av_hwdevice_find_type_by_name)(name.as_ptr()),
                (api.avcodec_find_decoder_by_name)(c"h264".as_ptr()),
                (api.av_get_pix_fmt)(c"nv12".as_ptr()),
            )
        };
        if device_type == abi::HWDEVICE_TYPE_NONE {
            return Err(format!("this FFmpeg has no {} support", self.name));
        }
        if codec.is_null() {
            return Err("this FFmpeg has no h264 decoder".to_string());
        }
        if nv12 == abi::PIX_FMT_NONE {
            return Err("this FFmpeg has no NV12".to_string());
        }
        let Some(hw_format) = hw_format(api, codec, device_type) else {
            return Err(format!(
                "FFmpeg's h264 decoder cannot use a {} device",
                self.name
            ));
        };
        let mut reasons = Vec::new();
        for node in candidates(self.name) {
            let label = node.clone().unwrap_or_else(|| "default device".to_string());
            let node = node.map(|n| CString::new(n).unwrap_or_default());
            let mut buffer: Ptr = std::ptr::null_mut();
            // SAFETY: an out-parameter, a device type FFmpeg named, and a
            // NUL-terminated device string or null for FFmpeg's choice.
            let status = unsafe {
                (api.av_hwdevice_ctx_create)(
                    &mut buffer,
                    device_type,
                    node.as_ref().map_or(std::ptr::null(), |n| n.as_ptr()),
                    std::ptr::null_mut(),
                    0,
                )
            };
            if status < 0 || buffer.is_null() {
                reasons.push(format!("{label}: no device (error {status})"));
                continue;
            }
            let device = Device {
                buffer: buffer as usize,
                codec: codec as usize,
                hw_format,
                nv12,
                label: label.clone(),
            };
            match probe_decode(api, &device) {
                Ok(()) => return Ok(device),
                Err(why) => {
                    // SAFETY: the reference this function made.
                    unsafe { (api.av_buffer_unref)(&mut buffer) };
                    reasons.push(format!("{label}: {why}"));
                }
            }
        }
        if reasons.is_empty() {
            reasons.push("no device to try".to_string());
        }
        Err(reasons.join(", "))
    }
}

/// The hardware pixel format FFmpeg's decoder produces on `device_type`.
fn hw_format(api: &Api, codec: *const c_void, device_type: c_int) -> Option<c_int> {
    for index in 0.. {
        // SAFETY: FFmpeg's decoder; a null entry ends the list.
        let config = unsafe { (api.avcodec_get_hw_config)(codec, index) };
        if config.is_null() {
            return None;
        }
        // SAFETY: an `AVCodecHWConfig` FFmpeg returned.
        let (pix_fmt, methods, kind) = unsafe {
            (
                abi::read::<c_int>(config, abi::hw_config::PIX_FMT),
                abi::read::<c_int>(config, abi::hw_config::METHODS),
                abi::read::<c_int>(config, abi::hw_config::DEVICE_TYPE),
            )
        };
        if methods & abi::METHOD_HW_DEVICE_CTX != 0 && kind == device_type {
            return Some(pix_fmt);
        }
    }
    None
}

/// The device strings to try: FFmpeg's choice, or for VA-API each render
/// node by its PCI path, since the numbered nodes are not stable.
fn candidates(name: &str) -> Vec<Option<String>> {
    if name != "vaapi" {
        return vec![None];
    }
    let mut nodes: Vec<String> = std::fs::read_dir("/dev/dri/by-path")
        .map(|dir| {
            dir.filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.ends_with("-render"))
                })
                .filter_map(|path| path.to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    nodes.into_iter().map(Some).collect()
}

/// Decode the probe clip on `device`: all six pictures, the first with the
/// luma it was made with.
fn probe_decode(api: &'static Api, device: &Device) -> Result<(), String> {
    let units = crate::bitstream::split_annex_b(super::PROBE_CLIP);
    let (sets, slices) = units.split_at(2);
    let sps = coremedia::h264::parse_sps(sets[0]).ok_or("the probe clip has no SPS")?;
    check_stream(&sps)?;
    let mut decoder = FfmpegDecoder::new(api, device)?;
    let mut seen: Vec<(u64, u8)> = Vec::new();
    let mut sink = |picture: Picture<'_>| {
        let luma = match picture.planes {
            Planes::Nv12 { y, .. } | Planes::I420 { y, .. } => y.first().copied().unwrap_or(0),
        };
        seen.push((picture.token, luma));
    };
    for (token, slice) in slices.iter().enumerate() {
        let mut annex_b = Vec::new();
        let parts: Vec<&[u8]> = match token {
            0 => vec![sets[0], sets[1], slice],
            _ => vec![slice],
        };
        for part in parts {
            annex_b.extend_from_slice(&[0, 0, 0, 1]);
            annex_b.extend_from_slice(part);
        }
        let unit = AccessUnit {
            annex_b,
            idr: token == 0,
        };
        let decoded = decoder
            .send(&unit, token as u64)
            .and_then(|()| decoder.receive(&mut sink));
        if decoder.refused() {
            return Err("the driver cannot decode the probe's High profile stream".to_string());
        }
        decoded.map_err(|e| format!("decoding failed ({e:?})"))?;
    }
    decoder
        .drain(&mut sink)
        .map_err(|e| format!("draining failed ({e:?})"))?;
    drop(sink);
    match seen.first() {
        _ if seen.len() != slices.len() => Err(format!(
            "{} of {} pictures came out",
            seen.len(),
            slices.len()
        )),
        Some(&(0, luma)) if luma.abs_diff(40) <= 2 => Ok(()),
        Some(&(token, luma)) => Err(format!(
            "the first picture came out wrong (access unit {token}, luma {luma}, not 40)"
        )),
        None => Err("no pictures came out".to_string()),
    }
}

impl Backend for FfmpegBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn is_hardware(&self) -> bool {
        true
    }

    fn available(&self) -> Result<(), String> {
        if self.unhealthy.load(Ordering::Relaxed) {
            return Err("it failed earlier in this process".to_string());
        }
        self.device().map(|_| ())
    }

    fn open(&self, config: &StreamConfig) -> Result<Box<dyn Decoder>, String> {
        check_stream(config.sps)?;
        let (api, device) = self.device()?;
        Ok(Box::new(FfmpegDecoder::new(api, device)?))
    }

    fn set_unhealthy(&self) {
        self.unhealthy.store(true, Ordering::Relaxed);
    }

    fn description(&self) -> String {
        match self.device() {
            Ok((api, device)) => format!(
                "FFmpeg (libavcodec {}) through {} on the {}",
                api.version(),
                self.name,
                device.label
            ),
            Err(_) => self.name.to_string(),
        }
    }
}

/// What `get_format` should pick, and whether it had to refuse.
struct Wanted {
    format: c_int,
    refused: AtomicBool,
}

/// `AVCodecContext.get_format`: the hardware format, or nothing - never
/// FFmpeg's software fallback.
unsafe extern "C" fn get_format(context: Ptr, formats: *const c_int) -> c_int {
    // SAFETY: the context this backend opened, whose `opaque` it set to a
    // `Wanted` that outlives the context.
    let wanted = unsafe { abi::read::<*const Wanted>(context, abi::context::OPAQUE) };
    if wanted.is_null() || formats.is_null() {
        return abi::PIX_FMT_NONE;
    }
    // SAFETY: as above.
    let wanted = unsafe { &*wanted };
    for index in 0.. {
        // SAFETY: FFmpeg's list, ended by `AV_PIX_FMT_NONE`, read no further.
        let format = unsafe { *formats.add(index) };
        if format == abi::PIX_FMT_NONE {
            break;
        }
        if format == wanted.format {
            return format;
        }
    }
    wanted.refused.store(true, Ordering::Relaxed);
    abi::PIX_FMT_NONE
}

/// One open `AVCodecContext` and the packet and frames it is driven with.
pub(crate) struct FfmpegDecoder {
    api: &'static Api,
    context: Ptr,
    packet: Ptr,
    hw: Ptr,
    sw: Ptr,
    hw_format: c_int,
    nv12: c_int,
    /// Read through the context's `opaque` by [`get_format`].
    wanted: Box<Wanted>,
    /// The packet was refused for now and has to be sent again.
    held: bool,
}

// SAFETY: the FFmpeg objects are used by one thread at a time, which is all
// FFmpeg asks of a codec context; the session's lock sees to it.
unsafe impl Send for FfmpegDecoder {}

impl Drop for FfmpegDecoder {
    fn drop(&mut self) {
        // SAFETY: objects this decoder allocated, each freed once; FFmpeg's
        // free functions take null.
        unsafe {
            (self.api.avcodec_free_context)(&mut self.context);
            (self.api.av_packet_free)(&mut self.packet);
            (self.api.av_frame_free)(&mut self.hw);
            (self.api.av_frame_free)(&mut self.sw);
        }
    }
}

impl FfmpegDecoder {
    fn new(api: &'static Api, device: &Device) -> Result<Self, String> {
        let codec = device.codec as *const c_void;
        // SAFETY: resolved entry points; each result is checked below.
        let decoder = unsafe {
            FfmpegDecoder {
                api,
                context: (api.avcodec_alloc_context3)(codec),
                packet: (api.av_packet_alloc)(),
                hw: (api.av_frame_alloc)(),
                sw: (api.av_frame_alloc)(),
                hw_format: device.hw_format,
                nv12: device.nv12,
                wanted: Box::new(Wanted {
                    format: device.hw_format,
                    refused: AtomicBool::new(false),
                }),
                held: false,
            }
        };
        if [decoder.context, decoder.packet, decoder.hw, decoder.sw]
            .iter()
            .any(|p| p.is_null())
        {
            return Err("FFmpeg is out of memory".to_string());
        }
        let context = decoder.context;
        // SAFETY: a fresh context, its fields at the offsets `abi` checks,
        // and a device reference the context takes over.
        let status = unsafe {
            let device_ref = (api.av_buffer_ref)(device.buffer as *const c_void);
            if device_ref.is_null() {
                return Err("FFmpeg is out of memory".to_string());
            }
            abi::write::<Ptr>(context, abi::context::HW_DEVICE_CTX, device_ref);
            abi::write::<unsafe extern "C" fn(Ptr, *const c_int) -> c_int>(
                context,
                abi::context::GET_FORMAT,
                get_format,
            );
            abi::write::<*const Wanted>(context, abi::context::OPAQUE, &*decoder.wanted);
            (api.av_opt_set_int)(context, c"threads".as_ptr(), 1, 0);
            (api.avcodec_open2)(context, codec, std::ptr::null_mut())
        };
        if status < 0 {
            return Err(format!("FFmpeg did not open the decoder (error {status})"));
        }
        Ok(decoder)
    }

    /// A failure that is not the input's: the whole backend's, unless
    /// `get_format` had to refuse, which this stream caused.
    fn failure(&self) -> DecodeError {
        DecodeError::Malfunction {
            global: !self.refused(),
        }
    }

    /// Whether `get_format` had to refuse: the hardware cannot decode this
    /// stream (FFmpeg then reports each picture as bad data), so the
    /// session should stop rather than drop every frame.
    fn refused(&self) -> bool {
        self.wanted.refused.load(Ordering::Relaxed)
    }

    /// What bad data from FFmpeg means.
    fn bad_data(&self) -> DecodeError {
        match self.refused() {
            true => self.failure(),
            false => DecodeError::BadData,
        }
    }

    /// Hand the decoded picture in `hw` to `sink`.
    fn emit(&mut self, sink: &mut dyn FnMut(Picture<'_>)) -> Result<(), DecodeError> {
        let api = self.api;
        let (hw, sw) = (self.hw, self.sw);
        // SAFETY: a frame `avcodec_receive_frame` just filled.
        let format = unsafe { abi::read::<c_int>(hw, abi::frame::FORMAT) };
        if format != self.hw_format {
            return Err(DecodeError::UnsupportedOutput);
        }
        // SAFETY: the decoded frame, and an empty frame for FFmpeg to
        // allocate in the first format it can copy to.
        let status = unsafe {
            (api.av_frame_unref)(sw);
            (api.av_hwframe_transfer_data)(sw, hw, 0)
        };
        if status < 0 {
            return Err(DecodeError::Malfunction { global: true });
        }
        let result = (|| {
            // SAFETY: the decoded frame and its copy, at `abi`'s offsets.
            let (sw_format, width, height, crop_top, crop_left, pts, colour) = unsafe {
                (
                    abi::read::<c_int>(sw, abi::frame::FORMAT),
                    abi::read::<c_int>(hw, abi::frame::WIDTH),
                    abi::read::<c_int>(hw, abi::frame::HEIGHT),
                    abi::read::<usize>(hw, abi::frame::CROP_TOP),
                    abi::read::<usize>(hw, abi::frame::CROP_LEFT),
                    abi::read::<i64>(hw, abi::frame::PTS),
                    colour(hw),
                )
            };
            if sw_format != self.nv12 {
                return Err(DecodeError::UnsupportedOutput);
            }
            // FFmpeg crops a hardware frame on the right and bottom by
            // shrinking it; the left and top are left to whoever reads it.
            let (Some(width), Some(height)) = (
                usize::try_from(width)
                    .ok()
                    .and_then(|w| w.checked_sub(crop_left)),
                usize::try_from(height)
                    .ok()
                    .and_then(|h| h.checked_sub(crop_top)),
            ) else {
                return Err(DecodeError::UnsupportedOutput);
            };
            if width == 0 || height == 0 || crop_left % 2 != 0 || crop_top % 2 != 0 {
                return Err(DecodeError::UnsupportedOutput);
            }
            // SAFETY: the copy's plane pointers and row sizes.
            let (y, uv, y_stride, uv_stride) = unsafe {
                (
                    abi::read::<*const u8>(sw, abi::frame::DATA),
                    abi::read::<*const u8>(sw, abi::frame::DATA + 8),
                    abi::read::<c_int>(sw, abi::frame::LINESIZE),
                    abi::read::<c_int>(sw, abi::frame::LINESIZE + 4),
                )
            };
            let (Ok(y_stride), Ok(uv_stride)) =
                (usize::try_from(y_stride), usize::try_from(uv_stride))
            else {
                return Err(DecodeError::UnsupportedOutput);
            };
            let chroma_width = width.div_ceil(2) * 2;
            let chroma_height = height.div_ceil(2);
            if y.is_null() || uv.is_null() || y_stride < width || uv_stride < chroma_width {
                return Err(DecodeError::UnsupportedOutput);
            }
            // SAFETY: FFmpeg allocated the copy at least the frame's coded
            // size, of which the cropped picture is a part.
            let (y, uv) = unsafe {
                (
                    std::slice::from_raw_parts(
                        y.add(crop_top * y_stride + crop_left),
                        (height - 1) * y_stride + width,
                    ),
                    std::slice::from_raw_parts(
                        uv.add(crop_top / 2 * uv_stride + crop_left),
                        (chroma_height - 1) * uv_stride + chroma_width,
                    ),
                )
            };
            sink(Picture {
                token: pts as u64,
                width,
                height,
                planes: Planes::Nv12 {
                    y,
                    y_stride,
                    uv,
                    uv_stride,
                },
                colour,
            });
            Ok(())
        })();
        // SAFETY: the copy, no longer borrowed.
        unsafe { (api.av_frame_unref)(sw) };
        result
    }

    /// Receive frames until FFmpeg has none, sending a held packet when it
    /// makes room. `Ok(true)` once the end of a drain has been reached.
    fn pump(&mut self, sink: &mut dyn FnMut(Picture<'_>)) -> Result<bool, DecodeError> {
        let api = self.api;
        loop {
            // SAFETY: the open context and this decoder's frame.
            let status = unsafe { (api.avcodec_receive_frame)(self.context, self.hw) };
            match status {
                0 => {
                    let emitted = self.emit(sink);
                    // SAFETY: as above.
                    unsafe { (api.av_frame_unref)(self.hw) };
                    emitted?;
                }
                abi::EAGAIN if self.held => {
                    self.held = false;
                    // SAFETY: the packet FFmpeg refused for now.
                    match unsafe { (api.avcodec_send_packet)(self.context, self.packet) } {
                        0 => {}
                        abi::INVALIDDATA => return Err(self.bad_data()),
                        _ => return Err(self.failure()),
                    }
                }
                abi::EAGAIN => return Ok(false),
                abi::EOF => return Ok(true),
                abi::INVALIDDATA => return Err(self.bad_data()),
                _ => return Err(self.failure()),
            }
        }
    }
}

/// The colour description FFmpeg found for `frame`.
///
/// # Safety
///
/// `frame` must be a live `AVFrame`.
unsafe fn colour(frame: *const c_void) -> Colour {
    let code = |offset| {
        // SAFETY: as the caller promises.
        let value = unsafe { abi::read::<c_int>(frame, offset) };
        u8::try_from(value).unwrap_or(2)
    };
    // SAFETY: as the caller promises.
    let range = unsafe { abi::read::<c_int>(frame, abi::frame::COLOR_RANGE) };
    Colour {
        full_range: match range {
            abi::RANGE_JPEG => Some(true),
            abi::RANGE_MPEG => Some(false),
            _ => None,
        },
        matrix: code(abi::frame::COLORSPACE),
        primaries: code(abi::frame::COLOR_PRIMARIES),
        transfer: code(abi::frame::COLOR_TRC),
    }
}

impl Decoder for FfmpegDecoder {
    fn send(&mut self, unit: &AccessUnit, token: u64) -> Result<(), DecodeError> {
        let api = self.api;
        if self.held {
            // The previous packet was never taken; nothing more can be.
            return Err(self.failure());
        }
        let Ok(size) = c_int::try_from(unit.annex_b.len()) else {
            return Err(DecodeError::BadData);
        };
        // SAFETY: this decoder's packet, filled with a fresh, padded buffer
        // of `size` bytes, which the copy below fits.
        let status = unsafe {
            (api.av_packet_unref)(self.packet);
            if (api.av_new_packet)(self.packet, size) < 0 {
                return Err(DecodeError::Malfunction { global: false });
            }
            let data = abi::read::<*mut u8>(self.packet, abi::packet::DATA);
            if data.is_null() || abi::read::<c_int>(self.packet, abi::packet::SIZE) != size {
                return Err(DecodeError::Malfunction { global: true });
            }
            std::ptr::copy_nonoverlapping(unit.annex_b.as_ptr(), data, unit.annex_b.len());
            abi::write::<i64>(self.packet, abi::packet::PTS, token as i64);
            (api.avcodec_send_packet)(self.context, self.packet)
        };
        match status {
            0 => Ok(()),
            abi::EAGAIN => {
                self.held = true;
                Ok(())
            }
            abi::INVALIDDATA => Err(self.bad_data()),
            _ => Err(self.failure()),
        }
    }

    fn receive(&mut self, sink: &mut dyn FnMut(Picture<'_>)) -> Result<(), DecodeError> {
        self.pump(sink).map(|_| ())
    }

    fn drain(&mut self, sink: &mut dyn FnMut(Picture<'_>)) -> Result<(), DecodeError> {
        let api = self.api;
        let mut result = self.pump(sink).map(|_| ());
        // SAFETY: the open context; a null packet starts draining.
        let status = unsafe { (api.avcodec_send_packet)(self.context, std::ptr::null()) };
        if status < 0 && status != abi::EOF {
            result = result.and(Err(self.failure()));
        } else {
            // A broken picture is skipped and the rest still come out; the
            // bound is for a decoder that reports the same one forever.
            for _ in 0..64 {
                match self.pump(sink) {
                    Err(DecodeError::BadData) => continue,
                    Err(e) => result = result.and(Err(e)),
                    // Draining ended, or stalled with nothing left to give.
                    Ok(_) => {}
                }
                break;
            }
        }
        // SAFETY: the open context.
        unsafe { (api.avcodec_flush_buffers)(self.context) };
        self.held = false;
        result
    }
}
