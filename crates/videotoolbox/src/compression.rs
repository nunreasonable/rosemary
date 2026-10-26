#![allow(non_snake_case)]

use std::ffi::c_void;

use crate::constants::{kVTCouldNotFindVideoDecoderErr, kVTVideoDecoderMalfunctionErr};
use crate::types::{OSStatus, VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionSessionRef};
use corefoundation::{api::CFAllocatorRef, collections::CFDictionaryRef};
use coremedia::format_description::CMVideoFormatDescriptionRef;
use coremedia::sample_buffer::CMSampleBufferRef;

pub(crate) const KEYS: &[(&str, &str)] = &[
    (
        "_kVTCompressionPropertyKey_AllowFrameReordering",
        "AllowFrameReordering",
    ),
    (
        "_kVTCompressionPropertyKey_AverageBitRate",
        "AverageBitRate",
    ),
    (
        "_kVTCompressionPropertyKey_MaxKeyFrameInterval",
        "MaxKeyFrameInterval",
    ),
    ("_kVTCompressionPropertyKey_RealTime", "RealTime"),
    (
        "_kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder",
        "UsingHardwareAcceleratedVideoEncoder",
    ),
    ("_kVTDecompressionPropertyKey_RealTime", "RealTime"),
    (
        "_kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder",
        "UsingHardwareAcceleratedVideoDecoder",
    ),
    (
        "_kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder",
        "EnableHardwareAcceleratedVideoDecoder",
    ),
    (
        "_kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder",
        "RequireHardwareAcceleratedVideoDecoder",
    ),
    ("_kVTVideoEncoderList_CodecType", "CodecType"),
];

#[repr(C)]
pub struct VTDecompressionOutputCallbackRecord {
    pub decompression_output_callback: Option<unsafe extern "C" fn()>,
    pub decompression_output_ref_con: *mut std::ffi::c_void,
}

pub unsafe extern "C" fn VTDecompressionSessionCreate(
    _allocator: CFAllocatorRef,
    _videoFormatDescription: CMVideoFormatDescriptionRef,
    _videoDecoderSpecification: CFDictionaryRef,
    _destinationImageBufferAttributes: CFDictionaryRef,
    _outputCallback: *const VTDecompressionOutputCallbackRecord,
    decompression_session_out: *mut VTDecompressionSessionRef,
) -> OSStatus {
    if !decompression_session_out.is_null() {
        // SAFETY: the caller provides a writable session output.
        unsafe { decompression_session_out.write(std::ptr::null_mut()) };
    }
    kVTCouldNotFindVideoDecoderErr
}

pub unsafe extern "C" fn VTDecompressionSessionDecodeFrame(
    _session: VTDecompressionSessionRef,
    _sampleBuffer: CMSampleBufferRef,
    _decodeFlags: VTDecodeFrameFlags,
    _sourceFrameRefCon: *mut std::ffi::c_void,
    _infoFlagsOut: *mut VTDecodeInfoFlags,
) -> OSStatus {
    kVTVideoDecoderMalfunctionErr
}

pub unsafe extern "C" fn VTDecompressionSessionInvalidate(_session: VTDecompressionSessionRef) {}

pub unsafe extern "C" fn VTDecompressionSessionWaitForAsynchronousFrames(
    _session: VTDecompressionSessionRef,
) -> OSStatus {
    kVTVideoDecoderMalfunctionErr
}

pub unsafe extern "C" fn VTDecompressionSessionCanAcceptFormatDescription(
    _session: VTDecompressionSessionRef,
    _newFormatDescription: CMVideoFormatDescriptionRef,
) -> bool {
    false
}

exports::exports! {
    functions {
        "VTRegisterSupplementalVideoDecoderIfAvailable" => VTRegisterSupplementalVideoDecoderIfAvailable,
        "VTIsHardwareDecodeSupported" => VTIsHardwareDecodeSupported,
        "VTCompressionSessionCreate" => VTCompressionSessionCreate,
        "VTDecompressionSessionCreate" => VTDecompressionSessionCreate,
        "VTDecompressionSessionDecodeFrame" => VTDecompressionSessionDecodeFrame,
        "VTDecompressionSessionInvalidate" => VTDecompressionSessionInvalidate,
        "VTDecompressionSessionWaitForAsynchronousFrames" => VTDecompressionSessionWaitForAsynchronousFrames,
        "VTDecompressionSessionCanAcceptFormatDescription" => VTDecompressionSessionCanAcceptFormatDescription,
    }
}

/// `void VTRegisterSupplementalVideoDecoderIfAvailable(CMVideoCodecType)`.
pub unsafe extern "C" fn VTRegisterSupplementalVideoDecoderIfAvailable(_codec: u32) {}

/// `Boolean VTIsHardwareDecodeSupported(CMVideoCodecType)`: no.
pub unsafe extern "C" fn VTIsHardwareDecodeSupported(_codec: u32) -> u8 {
    0
}

/// `OSStatus VTCompressionSessionCreate(CFAllocatorRef, int32_t width, int32_t
/// height, CMVideoCodecType, CFDictionaryRef encoderSpecification,
/// CFDictionaryRef sourceImageBufferAttributes, CFAllocatorRef,
/// VTCompressionOutputCallback, void *, VTCompressionSessionRef *)`: no
/// encoder either, which sends a program to its software encoder.
///
/// # Safety
///
/// `out` is null or writable.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn VTCompressionSessionCreate(
    _allocator: *const c_void,
    _width: i32,
    _height: i32,
    _codec: u32,
    _specification: *const c_void,
    _attributes: *const c_void,
    _compressed_allocator: *const c_void,
    _callback: *const c_void,
    _refcon: *const c_void,
    out: *mut *const c_void,
) -> i32 {
    /// `kVTCouldNotFindVideoEncoderErr`.
    const NO_ENCODER: i32 = -12908;
    if !out.is_null() {
        // SAFETY: writable, per the caller.
        unsafe { out.write(std::ptr::null()) };
    }
    NO_ENCODER
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_decoder_clears_the_session_output() {
        let mut session = std::ptr::dangling_mut::<u8>().cast();
        let status = unsafe {
            VTDecompressionSessionCreate(
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                &mut session,
            )
        };
        assert_eq!(status, kVTCouldNotFindVideoDecoderErr);
        assert!(session.is_null());
    }
}
