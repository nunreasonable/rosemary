//! Pixel formats a `CVPixelBuffer` can hold, and the one place their memory
//! layout is worked out.
//!
//! A planar buffer starts with the big-endian `CVPlanarPixelBufferInfo` that
//! `CVPixelBuffer.h` describes, which is what `CVPixelBufferGetBaseAddress`
//! returns for one; each plane follows at a 64-byte boundary.

const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// `kCVPixelFormatType_32BGRA`.
pub const BGRA: u32 = fourcc(b"BGRA");
/// `kCVPixelFormatType_420YpCbCr8Planar`: Y, Cb and Cr planes, video range.
pub const Y420: u32 = fourcc(b"y420");
/// `kCVPixelFormatType_420YpCbCr8PlanarFullRange`.
pub const F420: u32 = fourcc(b"f420");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`: Y and interleaved CbCr.
pub const NV12_VIDEO: u32 = fourcc(b"420v");
/// `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`.
pub const NV12_FULL: u32 = fourcc(b"420f");

/// Planes start, and rows are padded, to this many bytes at least.
pub const ALIGNMENT: usize = 64;

/// The largest width or height a buffer is made with.
pub const MAX_EXTENT: usize = 16384;

/// One plane of a format: its size as a divisor of the picture's, and its
/// bytes per element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneShape {
    pub width_divisor: usize,
    pub height_divisor: usize,
    pub bytes_per_element: usize,
}

const fn shape(width_divisor: usize, height_divisor: usize, bytes: usize) -> PlaneShape {
    PlaneShape {
        width_divisor,
        height_divisor,
        bytes_per_element: bytes,
    }
}

/// The planes of `format`, or `None` for a format this runtime cannot make.
pub fn planes(format: u32) -> Option<&'static [PlaneShape]> {
    const PACKED: &[PlaneShape] = &[shape(1, 1, 4)];
    const BIPLANAR: &[PlaneShape] = &[shape(1, 1, 1), shape(2, 2, 2)];
    const PLANAR: &[PlaneShape] = &[shape(1, 1, 1), shape(2, 2, 1), shape(2, 2, 1)];
    match format {
        BGRA => Some(PACKED),
        NV12_VIDEO | NV12_FULL => Some(BIPLANAR),
        Y420 | F420 => Some(PLANAR),
        _ => None,
    }
}

/// Whether `format`'s luma uses the full 0-255 range.
pub fn is_full_range(format: u32) -> bool {
    matches!(format, F420 | NV12_FULL)
}

/// The four characters of a format, for a diagnostic.
pub fn name(format: u32) -> String {
    let bytes = format.to_be_bytes();
    match bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ') {
        true => format!("'{}'", String::from_utf8_lossy(&bytes)),
        false => format!("{format:#010x}"),
    }
}

/// One plane's place in a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
    /// From the start of the buffer, which is the planar header's start too.
    pub offset: usize,
    pub width: usize,
    pub height: usize,
    pub bytes_per_row: usize,
    pub bytes_per_element: usize,
}

impl PlaneLayout {
    /// The bytes the plane covers.
    pub fn len(&self) -> usize {
        self.bytes_per_row * self.height
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The planar header's size, zero for a packed format.
    pub header: usize,
    pub planes: Vec<PlaneLayout>,
    /// Every byte the buffer needs.
    pub size: usize,
}

impl Layout {
    pub fn is_planar(&self) -> bool {
        self.header != 0
    }

    /// The big-endian `CVPlanarPixelBufferInfo_YCbCrPlanar` or `_BiPlanar`
    /// for this layout: an `int32_t` offset and a `uint32_t` row size per
    /// plane, each offset from the header's start.
    pub fn header_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.planes.len() * 8);
        for plane in &self.planes {
            out.extend_from_slice(&(plane.offset as i32).to_be_bytes());
            out.extend_from_slice(&(plane.bytes_per_row as u32).to_be_bytes());
        }
        out
    }
}

fn align_up(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|v| v / alignment * alignment)
}

/// The layout of a `width` by `height` buffer of `format`, with rows aligned
/// to `row_alignment` where that is above [`ALIGNMENT`].
///
/// `None` for an unknown format, an empty or oversized picture, or a size
/// that overflows.
pub fn layout(format: u32, width: usize, height: usize, row_alignment: usize) -> Option<Layout> {
    let shapes = planes(format)?;
    if width == 0 || height == 0 || width > MAX_EXTENT || height > MAX_EXTENT {
        return None;
    }
    let row_alignment = row_alignment.max(ALIGNMENT);
    let header = match shapes.len() {
        1 => 0,
        n => align_up(n * 8, ALIGNMENT)?,
    };
    let mut offset = header;
    let mut out = Vec::with_capacity(shapes.len());
    for shape in shapes {
        let plane_width = width.div_ceil(shape.width_divisor);
        let plane_height = height.div_ceil(shape.height_divisor);
        let bytes_per_row = align_up(
            plane_width.checked_mul(shape.bytes_per_element)?,
            row_alignment,
        )?;
        offset = align_up(offset, ALIGNMENT)?;
        out.push(PlaneLayout {
            offset,
            width: plane_width,
            height: plane_height,
            bytes_per_row,
            bytes_per_element: shape.bytes_per_element,
        });
        offset = offset.checked_add(bytes_per_row.checked_mul(plane_height)?)?;
    }
    Some(Layout {
        header,
        planes: out,
        size: offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_packed_layout_has_no_header() {
        let layout = layout(BGRA, 10, 3, 0).unwrap();
        assert!(!layout.is_planar());
        assert_eq!(layout.planes.len(), 1);
        assert_eq!(layout.planes[0].offset, 0);
        assert_eq!(layout.planes[0].bytes_per_row, 64);
        assert_eq!(layout.size, 64 * 3);
    }

    #[test]
    fn a_requested_row_alignment_above_the_minimum_is_kept() {
        let layout = layout(Y420, 10, 2, 96).unwrap();
        for plane in &layout.planes {
            assert_eq!(plane.bytes_per_row % 96, 0);
            assert_eq!(plane.offset % ALIGNMENT, 0);
        }
    }

    #[test]
    fn unknown_formats_and_sizes_have_no_layout() {
        assert!(layout(fourcc(b"x420"), 2, 2, 0).is_none());
        assert!(layout(BGRA, 0, 2, 0).is_none());
        assert!(layout(BGRA, MAX_EXTENT + 1, 2, 0).is_none());
        assert_eq!(name(Y420), "'y420'");
    }
}
