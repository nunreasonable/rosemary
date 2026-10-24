//! CoreMedia's format descriptions: what a stream of samples is - its media
//! type, codec, picture size and codec configuration - as the immutable CF
//! object `CMFormatDescriptionRef`.
//!
//! Only video descriptions are made here, and of the codec-specific
//! constructors only H.264's. A description holds its extensions dictionary
//! as given, so everything a caller reads back with
//! `CMFormatDescriptionGetExtension` is what it, or the constructor, put in.
//!
//! Also the pieces a decoder is fed with: `CMTime`, block buffers and sample
//! buffers, in their own modules.

#![allow(non_snake_case, clippy::missing_safety_doc)]

pub mod attachment;
pub mod block;
pub mod h264;
pub mod sample;
pub mod time;

use std::ffi::c_void;
use std::sync::OnceLock;

use corefoundation::base::{CFTypeID, CFTypeRef};
use objc::class::Class;
use objc::object::Id;

/// `OSStatus` values from `CMFormatDescription.h`.
pub const INVALID_PARAMETER: i32 = -12710;
pub const ALLOCATION_FAILED: i32 = -12711;
pub const VALUE_NOT_AVAILABLE: i32 = -12718;
const NO_ERR: i32 = 0;

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// `kCMMediaType_Video`.
pub const MEDIA_VIDEO: u32 = fourcc(b"vide");
/// `kCMVideoCodecType_H264`.
pub const CODEC_H264: u32 = fourcc(b"avc1");

/// `CMVideoDimensions`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dimensions {
    pub width: i32,
    pub height: i32,
}

/// `CGRect`, which `CMVideoFormatDescriptionGetCleanAperture` returns.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// The keys a description's extensions use, as `(Mach-O name, text)`. The
/// texts are Apple's: a program may spell a key with `CFSTR` instead of
/// importing it, and the two have to compare equal. Several are CoreVideo's
/// image buffer keys under a second name, as `CMFormatDescription.h` defines
/// them.
const KEYS: &[(&str, &str)] = &[
    (
        "_kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms",
        ATOMS,
    ),
    ("_kCMFormatDescriptionExtension_FormatName", "FormatName"),
    ("_kCMFormatDescriptionExtension_Depth", "Depth"),
    (
        "_kCMFormatDescriptionExtension_FullRangeVideo",
        "FullRangeVideo",
    ),
    (
        "_kCMFormatDescriptionExtension_MasteringDisplayColorVolume",
        "MasteringDisplayColorVolume",
    ),
    (
        "_kCMFormatDescriptionExtension_ContentLightLevelInfo",
        "ContentLightLevelInfo",
    ),
    (
        "_kCMFormatDescriptionExtension_ColorPrimaries",
        "CVImageBufferColorPrimaries",
    ),
    (
        "_kCMFormatDescriptionExtension_TransferFunction",
        "CVImageBufferTransferFunction",
    ),
    (
        "_kCMFormatDescriptionExtension_YCbCrMatrix",
        "CVImageBufferYCbCrMatrix",
    ),
    (
        "_kCMFormatDescriptionExtension_GammaLevel",
        "CVImageBufferGammaLevel",
    ),
    (
        "_kCMFormatDescriptionExtension_CleanAperture",
        CLEAN_APERTURE,
    ),
    ("_kCMFormatDescriptionExtension_FieldCount", "CVFieldCount"),
    (
        "_kCMFormatDescriptionExtension_PixelAspectRatio",
        "CVPixelAspectRatio",
    ),
    (
        "_kCMFormatDescriptionColorPrimaries_ITU_R_709_2",
        "ITU_R_709_2",
    ),
    ("_kCMFormatDescriptionColorPrimaries_EBU_3213", "EBU_3213"),
    ("_kCMFormatDescriptionColorPrimaries_SMPTE_C", "SMPTE_C"),
    ("_kCMFormatDescriptionColorPrimaries_DCI_P3", "DCI_P3"),
    ("_kCMFormatDescriptionColorPrimaries_P3_D65", "P3_D65"),
    (
        "_kCMFormatDescriptionColorPrimaries_ITU_R_2020",
        "ITU_R_2020",
    ),
    ("_kCMFormatDescriptionColorPrimaries_P22", "P22"),
    (
        "_kCMFormatDescriptionTransferFunction_ITU_R_709_2",
        "ITU_R_709_2",
    ),
    (
        "_kCMFormatDescriptionTransferFunction_SMPTE_240M_1995",
        "SMPTE_240M_1995",
    ),
    ("_kCMFormatDescriptionTransferFunction_UseGamma", "UseGamma"),
    (
        "_kCMFormatDescriptionTransferFunction_ITU_R_2020",
        "ITU_R_2020",
    ),
    (
        "_kCMFormatDescriptionTransferFunction_SMPTE_ST_428_1",
        "SMPTE_ST_428_1",
    ),
    (
        "_kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ",
        "SMPTE_ST_2084_PQ",
    ),
    (
        "_kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG",
        "ITU_R_2100_HLG",
    ),
    ("_kCMFormatDescriptionTransferFunction_Linear", "Linear"),
    ("_kCMFormatDescriptionTransferFunction_sRGB", "IEC_sRGB"),
    (
        "_kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2",
        "ITU_R_709_2",
    ),
    (
        "_kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4",
        "ITU_R_601_4",
    ),
    (
        "_kCMFormatDescriptionYCbCrMatrix_SMPTE_240M_1995",
        "SMPTE_240M_1995",
    ),
    ("_kCMFormatDescriptionYCbCrMatrix_ITU_R_2020", "ITU_R_2020"),
];

const ATOMS: &str = "SampleDescriptionExtensionAtoms";
const CLEAN_APERTURE: &str = "CVCleanAperture";
/// `kCVImageBufferCleanAperture*Key`, inside the clean aperture dictionary.
const APERTURE_WIDTH: &str = "Width";
const APERTURE_HEIGHT: &str = "Height";
const APERTURE_HORIZONTAL_OFFSET: &str = "HorizontalOffset";
const APERTURE_VERTICAL_OFFSET: &str = "VerticalOffset";

/// `CMTime`: value, timescale, flags, epoch.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Time {
    pub value: i64,
    pub timescale: i32,
    pub flags: u32,
    pub epoch: i64,
}

/// `kCMTimeInvalid`: all zeros, which is a time without
/// `kCMTimeFlags_Valid`.
pub static TIME_INVALID: Time = Time {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

/// The data symbols this crate publishes.
pub fn data_symbols() -> Vec<(String, u64)> {
    corefoundation::bootstrap();
    let mut out = corefoundation::base::string_globals(KEYS);
    out.extend(corefoundation::base::string_globals(sample::KEYS));
    out.push((
        "_kCMTimeInvalid".to_string(),
        &TIME_INVALID as *const Time as usize as u64,
    ));
    out
}

#[repr(C)]
struct Description {
    isa: *const c_void,
    state: State,
}

struct State {
    media_type: u32,
    subtype: u32,
    dimensions: Dimensions,
    /// An immutable dictionary this description owns, or null.
    extensions: CFTypeRef,
}

impl Drop for State {
    fn drop(&mut self) {
        release(self.extensions);
    }
}

fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        corefoundation::bootstrap();
        let class = corefoundation::base::register(
            c"CMFormatDescription",
            None,
            size_of::<Description>(),
            true,
        );
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(p as *const Class) }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: `-dealloc` of a `CMFormatDescription`, whose body is `Description`.
    let description = unsafe { &mut *(receiver as *mut Description) };
    // SAFETY: written once when the description was made, dropped only here.
    unsafe { std::ptr::drop_in_place(&mut description.state) };
    objc::arc::finalize(receiver);
}

fn state<'a>(description: CFTypeRef) -> Option<&'a State> {
    // SAFETY: null or an object a guest received from this runtime.
    let is_description =
        !description.is_null() && unsafe { corefoundation::base::is_kind_of(description, class()) };
    // SAFETY: checked to be a `CMFormatDescription`, whose body is `Description`.
    is_description.then(|| unsafe { &(*(description as *const Description)).state })
}

fn release(object: CFTypeRef) {
    if !object.is_null() {
        // SAFETY: an object this crate owns a reference to.
        unsafe { corefoundation::api::CFRelease(object) };
    }
}

fn key(text: &str) -> CFTypeRef {
    corefoundation::base::interned_name(text) as usize as CFTypeRef
}

/// A new description, owning `extensions` (already a reference of its own).
fn make(media_type: u32, subtype: u32, dimensions: Dimensions, extensions: CFTypeRef) -> CFTypeRef {
    // SAFETY: `Description` is the registered layout.
    let description = unsafe { corefoundation::base::allocate::<Description>(class()) };
    // SAFETY: a fresh, zeroed allocation whose state is written once here.
    unsafe {
        std::ptr::write(
            &mut (*description).state,
            State {
                media_type,
                subtype,
                dimensions,
                extensions,
            },
        )
    };
    description as CFTypeRef
}

/// An immutable copy of `extensions`, or null for none.
fn copy_extensions(extensions: CFTypeRef) -> CFTypeRef {
    match extensions.is_null() {
        true => std::ptr::null(),
        // SAFETY: a dictionary the caller passed.
        false => unsafe {
            corefoundation::api::CFDictionaryCreateCopy(std::ptr::null(), extensions)
        },
    }
}

/// `CFTypeID CMFormatDescriptionGetTypeID(void)`.
pub unsafe extern "C" fn CMFormatDescriptionGetTypeID() -> CFTypeID {
    corefoundation::base::type_id(class())
}

/// `CMMediaType CMFormatDescriptionGetMediaType(CMFormatDescriptionRef)`.
pub unsafe extern "C" fn CMFormatDescriptionGetMediaType(description: CFTypeRef) -> u32 {
    state(description).map_or(0, |s| s.media_type)
}

/// `FourCharCode CMFormatDescriptionGetMediaSubType(CMFormatDescriptionRef)`.
pub unsafe extern "C" fn CMFormatDescriptionGetMediaSubType(description: CFTypeRef) -> u32 {
    state(description).map_or(0, |s| s.subtype)
}

/// `CFDictionaryRef CMFormatDescriptionGetExtensions(CMFormatDescriptionRef)`:
/// not retained, and null when there are none.
pub unsafe extern "C" fn CMFormatDescriptionGetExtensions(description: CFTypeRef) -> CFTypeRef {
    state(description).map_or(std::ptr::null(), |s| s.extensions)
}

/// `CFPropertyListRef CMFormatDescriptionGetExtension(CMFormatDescriptionRef,
/// CFStringRef)`: not retained.
pub unsafe extern "C" fn CMFormatDescriptionGetExtension(
    description: CFTypeRef,
    extension_key: CFTypeRef,
) -> CFTypeRef {
    match state(description) {
        Some(s) if !s.extensions.is_null() && !extension_key.is_null() => {
            // SAFETY: the description's own dictionary and the caller's key.
            unsafe { corefoundation::api::CFDictionaryGetValue(s.extensions, extension_key) }
        }
        _ => std::ptr::null(),
    }
}

/// `Boolean CMFormatDescriptionEqual(CMFormatDescriptionRef,
/// CMFormatDescriptionRef)`: the media type, subtype, extensions and, for
/// video, dimensions.
pub unsafe extern "C" fn CMFormatDescriptionEqual(a: CFTypeRef, b: CFTypeRef) -> u8 {
    let (Some(first), Some(second)) = (state(a), state(b)) else {
        return u8::from(a == b && !a.is_null());
    };
    let extensions_equal = match (first.extensions.is_null(), second.extensions.is_null()) {
        (true, true) => true,
        // SAFETY: two dictionaries the descriptions own.
        (false, false) => unsafe {
            corefoundation::api::CFEqual(first.extensions, second.extensions) != 0
        },
        _ => false,
    };
    u8::from(
        first.media_type == second.media_type
            && first.subtype == second.subtype
            && first.dimensions == second.dimensions
            && extensions_equal,
    )
}

/// `OSStatus CMVideoFormatDescriptionCreate(CFAllocatorRef, CMVideoCodecType,
/// int32_t width, int32_t height, CFDictionaryRef extensions,
/// CMVideoFormatDescriptionRef *)`.
pub unsafe extern "C" fn CMVideoFormatDescriptionCreate(
    _allocator: CFTypeRef,
    codec: u32,
    width: i32,
    height: i32,
    extensions: CFTypeRef,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() {
        return INVALID_PARAMETER;
    }
    let made = make(
        MEDIA_VIDEO,
        codec,
        Dimensions { width, height },
        copy_extensions(extensions),
    );
    // SAFETY: the caller's out-parameter, checked above.
    unsafe { out.write(made) };
    NO_ERR
}

/// `OSStatus CMVideoFormatDescriptionCreateFromH264ParameterSets(CFAllocatorRef,
/// size_t parameterSetCount, const uint8_t *const *parameterSetPointers,
/// const size_t *parameterSetSizes, int NALUnitHeaderLength,
/// CMFormatDescriptionRef *)`.
///
/// At least an SPS and a PPS, as the header requires, and a NAL unit length
/// of 1, 2 or 4 bytes. The picture size is the SPS's after its frame
/// cropping, and the sets travel as the `avcC` atom, which is where
/// `CMVideoFormatDescriptionGetH264ParameterSetAtIndex` reads them back.
/// FIXME (ACCURACY): Apple does not document which other extensions it derives
/// from the SPS (its VUI colour description, for one); none are added here.
pub unsafe extern "C" fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
    _allocator: CFTypeRef,
    count: usize,
    pointers: *const *const u8,
    sizes: *const usize,
    nal_unit_header_length: i32,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() || pointers.is_null() || sizes.is_null() || count < 2 {
        return INVALID_PARAMETER;
    }
    if !matches!(nal_unit_header_length, 1 | 2 | 4) {
        return INVALID_PARAMETER;
    }
    let mut record = h264::Record {
        length_size: nal_unit_header_length as u8,
        ..h264::Record::default()
    };
    for i in 0..count {
        // SAFETY: the caller's two arrays of `count` entries.
        let (pointer, size) = unsafe { (*pointers.add(i), *sizes.add(i)) };
        if pointer.is_null() || size == 0 || size > usize::from(u16::MAX) {
            return INVALID_PARAMETER;
        }
        // SAFETY: `size` bytes at `pointer`, as the caller states.
        let set = unsafe { std::slice::from_raw_parts(pointer, size) }.to_vec();
        match h264::nal_type(&set) {
            Some(h264::NAL_SPS) => record.sps.push(set),
            Some(h264::NAL_PPS) => record.pps.push(set),
            Some(h264::NAL_SPS_EXTENSION) => record.sps_extensions.push(set),
            _ => return INVALID_PARAMETER,
        }
    }
    if record.sps.is_empty() || record.pps.is_empty() || record.sps.len() > 31 {
        return INVALID_PARAMETER;
    }
    let Some(sps) = h264::parse_sps(&record.sps[0]) else {
        return INVALID_PARAMETER;
    };
    let avcc = corefoundation::collections::new_data(record.encode(&sps));
    let atoms = dictionary(&[(key("avcC"), avcc)]);
    release(avcc);
    let extensions = dictionary(&[(key(ATOMS), atoms)]);
    release(atoms);
    let dimensions = Dimensions {
        width: sps.width() as i32,
        height: sps.height() as i32,
    };
    let made = make(MEDIA_VIDEO, CODEC_H264, dimensions, extensions);
    // SAFETY: the caller's out-parameter, checked above.
    unsafe { out.write(made) };
    NO_ERR
}

/// An immutable dictionary of CF keys and values, both retained.
fn dictionary(pairs: &[(CFTypeRef, CFTypeRef)]) -> CFTypeRef {
    let keys: Vec<CFTypeRef> = pairs.iter().map(|(k, _)| *k).collect();
    let values: Vec<CFTypeRef> = pairs.iter().map(|(_, v)| *v).collect();
    // SAFETY: two arrays of `pairs.len()` entries, and CF's own callbacks.
    unsafe {
        corefoundation::api::CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            pairs.len() as isize,
            &corefoundation::callbacks::TYPE_KEY_CALLBACKS,
            &corefoundation::callbacks::TYPE_VALUE_CALLBACKS,
        )
    }
}

/// `CMVideoDimensions CMVideoFormatDescriptionGetDimensions(CMVideoFormatDescriptionRef)`.
pub unsafe extern "C" fn CMVideoFormatDescriptionGetDimensions(
    description: CFTypeRef,
) -> Dimensions {
    match state(description) {
        Some(s) if s.media_type == MEDIA_VIDEO => s.dimensions,
        _ => Dimensions::default(),
    }
}

/// A clean aperture value: a number, or a rational as a two-element array.
fn aperture_value(aperture: CFTypeRef, name: &str) -> Option<f64> {
    // SAFETY: a dictionary from the description's extensions, and a key this
    // crate made.
    let value = unsafe { corefoundation::api::CFDictionaryGetValue(aperture, key(name)) };
    if value.is_null() {
        return None;
    }
    // SAFETY: a CF object from the dictionary.
    if let Some(number) = unsafe { corefoundation::collections::number(value) } {
        return Some(number.as_f64());
    }
    // SAFETY: as above; an array is the only other form the key takes.
    unsafe {
        if corefoundation::api::CFGetTypeID(value) != corefoundation::api::CFArrayGetTypeID()
            || corefoundation::api::CFArrayGetCount(value) != 2
        {
            return None;
        }
        let part = |i| {
            corefoundation::collections::number(corefoundation::api::CFArrayGetValueAtIndex(
                value, i,
            ))
        };
        let (numerator, denominator) = (part(0)?.as_f64(), part(1)?.as_f64());
        (denominator != 0.0).then_some(numerator / denominator)
    }
}

/// `CGRect CMVideoFormatDescriptionGetCleanAperture(CMVideoFormatDescriptionRef,
/// Boolean originIsAtTopLeft)`.
///
/// The whole picture unless the description has a clean aperture extension,
/// whose offsets are of the aperture's centre from the picture's centre, with
/// positive values right and down.
pub unsafe extern "C" fn CMVideoFormatDescriptionGetCleanAperture(
    description: CFTypeRef,
    origin_is_at_top_left: u8,
) -> Rect {
    let Some(s) = state(description).filter(|s| s.media_type == MEDIA_VIDEO) else {
        return Rect::default();
    };
    let (width, height) = (
        f64::from(s.dimensions.width),
        f64::from(s.dimensions.height),
    );
    let whole = Rect {
        x: 0.0,
        y: 0.0,
        width,
        height,
    };
    // SAFETY: the description, and a key this crate made.
    let aperture = unsafe { CMFormatDescriptionGetExtension(description, key(CLEAN_APERTURE)) };
    if aperture.is_null() {
        return whole;
    }
    let (Some(clean_width), Some(clean_height)) = (
        aperture_value(aperture, APERTURE_WIDTH),
        aperture_value(aperture, APERTURE_HEIGHT),
    ) else {
        return whole;
    };
    let horizontal = aperture_value(aperture, APERTURE_HORIZONTAL_OFFSET).unwrap_or(0.0);
    let vertical = aperture_value(aperture, APERTURE_VERTICAL_OFFSET).unwrap_or(0.0);
    let x = (width - clean_width) / 2.0 + horizontal;
    let from_top = (height - clean_height) / 2.0 + vertical;
    Rect {
        x,
        y: match origin_is_at_top_left != 0 {
            true => from_top,
            false => height - from_top - clean_height,
        },
        width: clean_width,
        height: clean_height,
    }
}

/// `OSStatus CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
/// CMFormatDescriptionRef, size_t parameterSetIndex, const uint8_t **,
/// size_t *size, size_t *count, int *NALUnitHeaderLength)`.
///
/// Read from the description's `avcC` atom, so it answers for a description
/// made with the atom in `CMVideoFormatDescriptionCreate`'s extensions as well.
/// Every out-parameter may be null; the pointer is into the atom, and lives as
/// long as the description.
pub unsafe extern "C" fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
    description: CFTypeRef,
    index: usize,
    set_out: *mut *const u8,
    size_out: *mut usize,
    count_out: *mut usize,
    header_length_out: *mut i32,
) -> i32 {
    let Some(s) = state(description) else {
        return INVALID_PARAMETER;
    };
    if s.subtype != CODEC_H264 {
        return INVALID_PARAMETER;
    }
    // SAFETY: the description, and keys this crate made.
    let avcc = unsafe {
        let atoms = CMFormatDescriptionGetExtension(description, key(ATOMS));
        match atoms.is_null() {
            true => std::ptr::null(),
            false => corefoundation::api::CFDictionaryGetValue(atoms, key("avcC")),
        }
    };
    if avcc.is_null() {
        return VALUE_NOT_AVAILABLE;
    }
    // SAFETY: a CF object from the atoms dictionary.
    let Some(bytes) = (unsafe { corefoundation::collections::data(avcc) }) else {
        return VALUE_NOT_AVAILABLE;
    };
    let Some((length_size, ranges)) = h264::locate(bytes) else {
        return VALUE_NOT_AVAILABLE;
    };
    // SAFETY: each out-parameter is the caller's, or null.
    unsafe {
        if !count_out.is_null() {
            count_out.write(ranges.len());
        }
        if !header_length_out.is_null() {
            header_length_out.write(i32::from(length_size));
        }
    }
    let wanted = !set_out.is_null() || !size_out.is_null();
    match ranges.get(index) {
        Some(range) => {
            // SAFETY: as above; the range is inside the atom's bytes.
            unsafe {
                if !set_out.is_null() {
                    set_out.write(bytes.as_ptr().add(range.start));
                }
                if !size_out.is_null() {
                    size_out.write(range.len());
                }
            }
            NO_ERR
        }
        // Asking only for the count and length is how a caller learns them.
        None if !wanted => NO_ERR,
        None => INVALID_PARAMETER,
    }
}

/// `OSStatus CMVideoFormatDescriptionCreateFromHEVCParameterSets(CFAllocatorRef,
/// size_t parameterSetCount, const uint8_t *const *parameterSetPointers,
/// const size_t *parameterSetSizes, int NALUnitHeaderLength, CFDictionaryRef
/// extensions, CMVideoFormatDescriptionRef *)`.
///
/// There is no HEVC parameter-set parser here - nothing reads an HEVC SPS for
/// its picture size the way [`h264::parse_sps`] does for H.264's - so this can
/// never build a description. The error is the one Apple's own parser returns
/// for a parameter set it cannot make sense of, and `out` is left null.
pub unsafe extern "C" fn CMVideoFormatDescriptionCreateFromHEVCParameterSets(
    _allocator: CFTypeRef,
    _parameter_set_count: usize,
    _parameter_set_pointers: *const *const u8,
    _parameter_set_sizes: *const usize,
    _nal_unit_header_length: i32,
    _extensions: CFTypeRef,
    out: *mut CFTypeRef,
) -> i32 {
    if !out.is_null() {
        // SAFETY: the caller's out-parameter, checked above.
        unsafe { out.write(std::ptr::null()) };
    }
    INVALID_PARAMETER
}

/// `OSStatus CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
/// CMFormatDescriptionRef, size_t parameterSetIndex, const uint8_t **, size_t
/// *parameterSetSizeOut, size_t *parameterSetCountOut, int
/// *NALUnitHeaderLengthOut)`.
///
/// No description this crate makes is ever HEVC -
/// [`CMVideoFormatDescriptionCreateFromHEVCParameterSets`] never succeeds - so
/// there is never a parameter set to answer with. Every out-parameter is left
/// as the caller passed it.
pub unsafe extern "C" fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
    _description: CFTypeRef,
    _parameter_set_index: usize,
    _parameter_set_pointer_out: *mut *const u8,
    _parameter_set_size_out: *mut usize,
    _parameter_set_count_out: *mut usize,
    _nal_unit_header_length_out: *mut i32,
) -> i32 {
    INVALID_PARAMETER
}

exports::exports! {
    functions {
        "CMFormatDescriptionGetTypeID" => CMFormatDescriptionGetTypeID,
        "CMFormatDescriptionGetMediaType" => CMFormatDescriptionGetMediaType,
        "CMFormatDescriptionGetMediaSubType" => CMFormatDescriptionGetMediaSubType,
        "CMFormatDescriptionGetExtensions" => CMFormatDescriptionGetExtensions,
        "CMFormatDescriptionGetExtension" => CMFormatDescriptionGetExtension,
        "CMFormatDescriptionEqual" => CMFormatDescriptionEqual,
        "CMVideoFormatDescriptionCreate" => CMVideoFormatDescriptionCreate,
        "CMVideoFormatDescriptionCreateFromH264ParameterSets" =>
            CMVideoFormatDescriptionCreateFromH264ParameterSets,
        "CMVideoFormatDescriptionCreateFromHEVCParameterSets" =>
            CMVideoFormatDescriptionCreateFromHEVCParameterSets,
        "CMVideoFormatDescriptionGetDimensions" => CMVideoFormatDescriptionGetDimensions,
        "CMVideoFormatDescriptionGetCleanAperture" => CMVideoFormatDescriptionGetCleanAperture,
        "CMVideoFormatDescriptionGetH264ParameterSetAtIndex" =>
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
        "CMVideoFormatDescriptionGetHEVCParameterSetAtIndex" =>
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    }
    groups { time, block, sample, attachment }
}

#[cfg(test)]
mod tests;
