//! The pixel buffers a session hands its callback.
//!
//! `destinationImageBufferAttributes` choose the format: 3-plane `y420` and
//! `f420`, NV12 `420v` and `420f`, or `BGRA`. A decoder's picture (NV12 or
//! I420) is copied into a fresh `CVPixelBuffer` of that format, its range
//! mapped where stream and format disagree, and converted to RGB for `BGRA`.

use corefoundation::base::CFTypeRef;
use corevideo::format::{BGRA, F420, NV12_FULL, NV12_VIDEO, Y420};
use corevideo::pixelbuffer;

use crate::backend::{Colour, Picture, Planes};

/// The formats a session can output, in no order of preference.
const SUPPORTED: [u32; 5] = [Y420, F420, NV12_VIDEO, NV12_FULL, BGRA];

/// What the attributes asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OutputSpec {
    /// The format asked for; `None` to choose from the first picture.
    format: Option<u32>,
    iosurface: bool,
    row_alignment: usize,
}

fn key(text: &str) -> CFTypeRef {
    corefoundation::base::interned_name(text) as usize as CFTypeRef
}

fn attribute(attributes: CFTypeRef, name: &str) -> CFTypeRef {
    if attributes.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the caller's dictionary and a key made here.
    unsafe { corefoundation::api::CFDictionaryGetValue(attributes, key(name)) }
}

fn integer(value: CFTypeRef) -> Option<i64> {
    // SAFETY: a CF object from the caller's dictionary.
    unsafe { corefoundation::collections::number(value) }.map(|n| n.as_i64())
}

fn fourcc(format: u32) -> String {
    corevideo::format::name(format)
}

impl OutputSpec {
    /// Read `destinationImageBufferAttributes` for a `width` by `height`
    /// stream, or say why the session cannot satisfy them.
    pub fn parse(attributes: CFTypeRef, width: usize, height: usize) -> Result<OutputSpec, String> {
        let mut spec = OutputSpec {
            format: None,
            iosurface: !attribute(attributes, pixelbuffer::IOSURFACE_PROPERTIES_KEY).is_null(),
            row_alignment: 0,
        };
        let requested = attribute(attributes, pixelbuffer::PIXEL_FORMAT_TYPE_KEY);
        if !requested.is_null() {
            let formats = formats(requested)?;
            spec.format = formats.iter().copied().find(|f| SUPPORTED.contains(f));
            if spec.format.is_none() {
                let names: Vec<String> = formats.iter().map(|&f| fourcc(f)).collect();
                return Err(format!(
                    "no output format of {} is supported (y420, f420, 420v, 420f and BGRA are)",
                    names.join(", ")
                ));
            }
        }
        for (name, size) in [
            (pixelbuffer::WIDTH_KEY, width),
            (pixelbuffer::HEIGHT_KEY, height),
        ] {
            let value = attribute(attributes, name);
            if value.is_null() {
                continue;
            }
            if integer(value) != Some(size as i64) {
                return Err(format!(
                    "a {name} of {} for a {width}x{height} stream: scaling is not supported",
                    integer(value).map_or("?".to_string(), |v| v.to_string())
                ));
            }
        }
        let alignment = attribute(attributes, pixelbuffer::BYTES_PER_ROW_ALIGNMENT_KEY);
        if !alignment.is_null() {
            spec.row_alignment = match integer(alignment) {
                Some(n @ 0..=4096) => n as usize,
                _ => return Err("the BytesPerRowAlignment is not a size".to_string()),
            };
        }
        Ok(spec)
    }

    /// The format pictures come out in, fixed by the first one when nothing
    /// was asked for.
    fn resolve(&mut self, colour: &Colour) -> u32 {
        *self.format.get_or_insert(match colour.full_range {
            Some(true) => NV12_FULL,
            _ => NV12_VIDEO,
        })
    }
}

/// The pixel formats in a `PixelFormatType` value: a number, or an array of
/// them.
fn formats(value: CFTypeRef) -> Result<Vec<u32>, String> {
    if let Some(n) = integer(value) {
        return Ok(vec![n as u32]);
    }
    // SAFETY: a CF object from the caller's dictionary.
    unsafe {
        if corefoundation::api::CFGetTypeID(value) != corefoundation::api::CFArrayGetTypeID() {
            return Err("the PixelFormatType is neither a number nor an array".to_string());
        }
        let count = corefoundation::api::CFArrayGetCount(value);
        (0..count)
            .map(|i| {
                integer(corefoundation::api::CFArrayGetValueAtIndex(value, i))
                    .map(|n| n as u32)
                    .ok_or_else(|| "the PixelFormatType array holds a non-number".to_string())
            })
            .collect()
    }
}

/// A 256-entry table from one range to the other, or `None` when they are
/// the same. Luma video range is 16-235, chroma 16-240 around 128.
fn range_table(from_full: bool, to_full: bool, luma: bool) -> Option<[u8; 256]> {
    if from_full == to_full {
        return None;
    }
    let mut table = [0u8; 256];
    for (i, out) in table.iter_mut().enumerate() {
        let v = i as f64;
        let mapped = match (luma, to_full) {
            (true, false) => 16.0 + v * 219.0 / 255.0,
            (true, true) => (v - 16.0) * 255.0 / 219.0,
            (false, false) => 128.0 + (v - 128.0) * 224.0 / 255.0,
            (false, true) => 128.0 + (v - 128.0) * 255.0 / 224.0,
        };
        *out = mapped.round().clamp(0.0, 255.0) as u8;
    }
    Some(table)
}

fn map(table: &Option<[u8; 256]>, value: u8) -> u8 {
    table.as_ref().map_or(value, |t| t[usize::from(value)])
}

/// The chroma of a picture, read one sample at a time or a row at a time.
enum Chroma<'a> {
    Interleaved {
        uv: &'a [u8],
        stride: usize,
    },
    Separate {
        u: &'a [u8],
        u_stride: usize,
        v: &'a [u8],
        v_stride: usize,
    },
}

impl Chroma<'_> {
    fn at(&self, x: usize, y: usize) -> (u8, u8) {
        match *self {
            Chroma::Interleaved { uv, stride } => {
                (uv[y * stride + 2 * x], uv[y * stride + 2 * x + 1])
            }
            Chroma::Separate {
                u,
                u_stride,
                v,
                v_stride,
            } => (u[y * u_stride + x], v[y * v_stride + x]),
        }
    }
}

/// The picture's luma and chroma planes.
fn split<'a>(planes: &'a Planes<'a>) -> (&'a [u8], usize, Chroma<'a>) {
    match *planes {
        Planes::Nv12 {
            y,
            y_stride,
            uv,
            uv_stride,
        } => (
            y,
            y_stride,
            Chroma::Interleaved {
                uv,
                stride: uv_stride,
            },
        ),
        Planes::I420 {
            y,
            y_stride,
            u,
            u_stride,
            v,
            v_stride,
        } => (
            y,
            y_stride,
            Chroma::Separate {
                u,
                u_stride,
                v,
                v_stride,
            },
        ),
    }
}

/// `CVReturn` or VideoToolbox status of a picture that could not be made.
pub(crate) type MakeError = i32;

/// A new pixel buffer holding `picture` in the session's format, with one
/// reference for the caller.
pub(crate) fn make_image(picture: &Picture, spec: &mut OutputSpec) -> Result<CFTypeRef, MakeError> {
    let format = spec.resolve(&picture.colour);
    let (width, height) = (picture.width, picture.height);
    let image = pixelbuffer::create(
        format,
        width,
        height,
        pixelbuffer::Options {
            iosurface: spec.iosurface,
            row_alignment: spec.row_alignment,
        },
    )?;
    let Some(mut planes) = pixelbuffer::planes_mut(image) else {
        release(image);
        return Err(pixelbuffer::ERROR);
    };
    let (y, y_stride, chroma) = split(&picture.planes);
    let full = picture.colour.full_range.unwrap_or(false);
    let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
    let copied = match format {
        BGRA => {
            let (out, stride) = &mut planes[0];
            to_bgra(
                y,
                y_stride,
                &chroma,
                width,
                height,
                &picture.colour,
                out,
                *stride,
            );
            true
        }
        Y420 | F420 | NV12_VIDEO | NV12_FULL => {
            let to_full = corevideo::format::is_full_range(format);
            let luma = range_table(full, to_full, true);
            let chroma_table = range_table(full, to_full, false);
            let (luma_plane, chroma_planes) = planes.split_at_mut(1);
            let (out, stride) = &mut luma_plane[0];
            for row in 0..height {
                let from = &y[row * y_stride..row * y_stride + width];
                let to = &mut out[row * *stride..row * *stride + width];
                match &luma {
                    None => to.copy_from_slice(from),
                    Some(t) => to
                        .iter_mut()
                        .zip(from)
                        .for_each(|(d, &s)| *d = t[usize::from(s)]),
                }
            }
            match chroma_planes {
                [(uv, uv_stride)] => {
                    for row in 0..ch {
                        let to = &mut uv[row * *uv_stride..row * *uv_stride + 2 * cw];
                        for x in 0..cw {
                            let (u, v) = chroma.at(x, row);
                            to[2 * x] = map(&chroma_table, u);
                            to[2 * x + 1] = map(&chroma_table, v);
                        }
                    }
                    true
                }
                [(u_out, u_stride), (v_out, v_stride)] => {
                    for row in 0..ch {
                        for x in 0..cw {
                            let (u, v) = chroma.at(x, row);
                            u_out[row * *u_stride + x] = map(&chroma_table, u);
                            v_out[row * *v_stride + x] = map(&chroma_table, v);
                        }
                    }
                    true
                }
                _ => false,
            }
        }
        _ => false,
    };
    drop(planes);
    if !copied {
        release(image);
        return Err(pixelbuffer::INVALID_PIXEL_FORMAT);
    }
    attach_colour(image, &picture.colour);
    Ok(image)
}

fn release(object: CFTypeRef) {
    // SAFETY: a reference this module holds.
    unsafe { corefoundation::api::CFRelease(object) };
}

/// `(Kr, Kb)` of the picture's matrix.
fn coefficients(colour: &Colour, height: usize) -> (f64, f64) {
    const BT601: (f64, f64) = (0.299, 0.114);
    const BT709: (f64, f64) = (0.2126, 0.0722);
    const BT2020: (f64, f64) = (0.2627, 0.0593);
    match colour.matrix {
        1 => BT709,
        5 | 6 => BT601,
        9 => BT2020,
        // FIXME (ACCURACY): what Apple assumes for a stream that does not
        // say is not verified; this is the usual HD/SD split.
        _ if height >= 720 => BT709,
        _ => BT601,
    }
}

/// Y'CbCr to `BGRA`, in 16.16 fixed point.
///
/// FIXME (ACCURACY): chroma is upsampled nearest-neighbour, where Apple's
/// conversion interpolates.
#[allow(clippy::too_many_arguments)]
fn to_bgra(
    y: &[u8],
    y_stride: usize,
    chroma: &Chroma,
    width: usize,
    height: usize,
    colour: &Colour,
    out: &mut [u8],
    stride: usize,
) {
    let full = colour.full_range.unwrap_or(false);
    let (kr, kb) = coefficients(colour, height);
    let kg = 1.0 - kr - kb;
    let (luma_scale, chroma_scale, luma_offset) = match full {
        true => (1.0, 1.0, 0),
        false => (255.0 / 219.0, 255.0 / 224.0, 16),
    };
    let fixed = |v: f64| (v * 65536.0).round() as i64;
    let ys = fixed(luma_scale);
    let r_cr = fixed(2.0 * (1.0 - kr) * chroma_scale);
    let b_cb = fixed(2.0 * (1.0 - kb) * chroma_scale);
    let g_cb = fixed(2.0 * kb * (1.0 - kb) / kg * chroma_scale);
    let g_cr = fixed(2.0 * kr * (1.0 - kr) / kg * chroma_scale);
    let clamp = |v: i64| ((v + 32768) >> 16).clamp(0, 255) as u8;
    for row in 0..height {
        let line = &mut out[row * stride..row * stride + 4 * width];
        for x in 0..width {
            let luma = (i64::from(y[row * y_stride + x]) - luma_offset) * ys;
            let (cb, cr) = chroma.at(x / 2, row / 2);
            let (cb, cr) = (i64::from(cb) - 128, i64::from(cr) - 128);
            let pixel = &mut line[4 * x..4 * x + 4];
            pixel[0] = clamp(luma + b_cb * cb);
            pixel[1] = clamp(luma - g_cb * cb - g_cr * cr);
            pixel[2] = clamp(luma + r_cr * cr);
            pixel[3] = 255;
        }
    }
}

/// The `kCVImageBuffer*` value text for an H.273 code point, where Apple
/// has one.
fn matrix_name(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("ITU_R_709_2"),
        5 | 6 => Some("ITU_R_601_4"),
        9 => Some("ITU_R_2020"),
        _ => None,
    }
}

fn primaries_name(code: u8) -> Option<&'static str> {
    match code {
        1 => Some("ITU_R_709_2"),
        5 => Some("EBU_3213"),
        6 => Some("SMPTE_C"),
        9 => Some("ITU_R_2020"),
        _ => None,
    }
}

fn transfer_name(code: u8) -> Option<&'static str> {
    match code {
        // SMPTE 170M's curve is BT.709's.
        1 | 6 => Some("ITU_R_709_2"),
        16 => Some("SMPTE_ST_2084_PQ"),
        18 => Some("ITU_R_2100_HLG"),
        _ => None,
    }
}

/// Describe the picture's colour on the buffer, as the attachments a
/// consumer that propagates them reads.
fn attach_colour(image: CFTypeRef, colour: &Colour) {
    let pairs = [
        ("CVImageBufferYCbCrMatrix", matrix_name(colour.matrix)),
        (
            "CVImageBufferColorPrimaries",
            primaries_name(colour.primaries),
        ),
        (
            "CVImageBufferTransferFunction",
            transfer_name(colour.transfer),
        ),
    ];
    for (name, value) in pairs {
        if let Some(value) = value {
            corevideo::buffer::set_attachment(
                image,
                key(name),
                key(value),
                corevideo::buffer::SHOULD_PROPAGATE,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_tables_map_the_ends_of_each_range() {
        let up = range_table(false, true, true).unwrap();
        assert_eq!((up[16], up[235], up[0], up[255]), (0, 255, 0, 255));
        let down = range_table(true, false, true).unwrap();
        assert_eq!((down[0], down[255]), (16, 235));
        let chroma_down = range_table(true, false, false).unwrap();
        assert_eq!(
            (chroma_down[0], chroma_down[128], chroma_down[255]),
            (16, 128, 240)
        );
        let chroma_up = range_table(false, true, false).unwrap();
        // 16 is half a step above full range's 0, which rounds up.
        assert_eq!(
            (chroma_up[16], chroma_up[128], chroma_up[240]),
            (1, 128, 255)
        );
        assert!(range_table(true, true, true).is_none());
    }

    #[test]
    fn unsupported_or_scaled_outputs_are_refused() {
        use corefoundation::callbacks::Policy;
        use corefoundation::collections::{Number, new_dictionary, new_number};
        corefoundation::bootstrap();
        let dictionary = |pairs: &[(&str, i64)]| {
            let numbers: Vec<CFTypeRef> = pairs
                .iter()
                .map(|(_, n)| new_number(Number::Integer(*n)))
                .collect();
            let entries: Vec<(CFTypeRef, CFTypeRef)> = pairs
                .iter()
                .zip(&numbers)
                .map(|((k, _), n)| (key(k), *n))
                .collect();
            new_dictionary(
                &entries,
                Policy::from_key_callbacks(),
                Policy::from_type_callbacks(),
                false,
            )
        };
        let rgb24 = dictionary(&[("PixelFormatType", 24)]);
        let why = OutputSpec::parse(rgb24, 64, 64).unwrap_err();
        assert!(why.contains("no output format"), "{why}");
        let scaled = dictionary(&[("Width", 32)]);
        let why = OutputSpec::parse(scaled, 64, 64).unwrap_err();
        assert!(why.contains("scaling"), "{why}");
        let same = dictionary(&[
            ("Width", 64),
            ("Height", 48),
            ("PixelFormatType", BGRA as i64),
        ]);
        let spec = OutputSpec::parse(same, 64, 48).unwrap();
        assert_eq!(spec.format, Some(BGRA));
        assert!(!spec.iosurface);
        assert_eq!(
            OutputSpec::parse(std::ptr::null(), 64, 48).unwrap().format,
            None
        );
    }
}
