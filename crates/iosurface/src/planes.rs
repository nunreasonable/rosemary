//! Planar surfaces, and surfaces whose layout someone else worked out.
//!
//! Only CoreVideo makes these: it lays a pixel buffer out once and hands the
//! offsets here, so the surface and the buffer cannot disagree about where a
//! plane is. `IOSurfaceCreate` still refuses `kIOSurfacePlaneInfo`.
//!
//! FIXME (ACCURACY): a planar surface answers zero for the whole-surface
//! bytes per row and bytes per element; what Apple answers is not verified.

use std::alloc::Layout;
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, AtomicU32};

use corefoundation::base::CFTypeRef;

use super::{MAX_BYTES_PER_ELEMENT, MAX_EXTENT, PAGE, State, align_up, publish, state};

/// One plane of a surface, as its maker laid it out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlaneSpec {
    /// From the surface's base address.
    pub offset: usize,
    pub width: usize,
    pub height: usize,
    pub bytes_per_row: usize,
    pub bytes_per_element: usize,
}

pub(crate) type Plane = PlaneSpec;

impl PlaneSpec {
    /// One past the plane's last byte, or `None` for a plane that does not
    /// hold its own rows.
    fn end(&self) -> Option<usize> {
        let row = self.width.checked_mul(self.bytes_per_element)?;
        if self.width == 0 || self.height == 0 || row > self.bytes_per_row {
            return None;
        }
        self.bytes_per_row
            .checked_mul(self.height)?
            .checked_add(self.offset)
    }
}

/// Page-aligned, zeroed memory of at least `alloc_size` bytes.
fn allocate(alloc_size: usize) -> Option<(*mut u8, Layout, usize)> {
    let size = align_up(alloc_size.max(1), PAGE)?;
    let layout = Layout::from_size_align(size, PAGE).ok()?;
    // SAFETY: a non-zero size.
    let memory = unsafe { std::alloc::alloc_zeroed(layout) };
    (!memory.is_null()).then_some((memory, layout, size))
}

fn surface(
    pixel_format: u32,
    width: usize,
    height: usize,
    alloc_size: usize,
    packed: Option<(usize, usize)>,
    planes: Vec<Plane>,
) -> CFTypeRef {
    if width == 0 || height == 0 || width > MAX_EXTENT || height > MAX_EXTENT {
        return std::ptr::null();
    }
    let Some((memory, layout, alloc_size)) = allocate(alloc_size) else {
        return std::ptr::null();
    };
    let (bytes_per_element, bytes_per_row) = packed.unwrap_or((0, 0));
    publish(State {
        id: 0,
        width,
        height,
        bytes_per_element,
        bytes_per_row,
        element_width: 1,
        element_height: 1,
        pixel_format,
        offset: 0,
        alloc_size,
        memory,
        layout,
        use_count: AtomicI32::new(0),
        seed: AtomicU32::new(0),
        locks: AtomicI32::new(0),
        values: Mutex::new(Vec::new()),
        planes,
    })
}

/// A planar surface of `planes`, inside `alloc_size` bytes (rounded up to a
/// page). Null if a plane does not fit or the memory cannot be had.
pub fn create_planar(
    pixel_format: u32,
    width: usize,
    height: usize,
    alloc_size: usize,
    planes: &[PlaneSpec],
) -> CFTypeRef {
    let fits = planes.iter().all(|p| {
        p.bytes_per_element <= MAX_BYTES_PER_ELEMENT && p.end().is_some_and(|end| end <= alloc_size)
    });
    if planes.is_empty() || !fits {
        return std::ptr::null();
    }
    surface(
        pixel_format,
        width,
        height,
        alloc_size,
        None,
        planes.to_vec(),
    )
}

/// A surface of one plane laid out by its maker: the same surface
/// `IOSurfaceCreate` would make, with these row bytes exactly.
pub fn create_packed(
    pixel_format: u32,
    width: usize,
    height: usize,
    bytes_per_element: usize,
    bytes_per_row: usize,
    alloc_size: usize,
) -> CFTypeRef {
    let plane = PlaneSpec {
        offset: 0,
        width,
        height,
        bytes_per_row,
        bytes_per_element,
    };
    let fits = bytes_per_element <= MAX_BYTES_PER_ELEMENT
        && plane.end().is_some_and(|end| end <= alloc_size);
    if !fits {
        return std::ptr::null();
    }
    surface(
        pixel_format,
        width,
        height,
        alloc_size,
        Some((bytes_per_element, bytes_per_row)),
        Vec::new(),
    )
}

/// Plane `index` of `surface`. A surface that is not planar answers for
/// index 0 with its whole self.
fn plane(surface: CFTypeRef, index: usize) -> Option<Plane> {
    let state = state(surface)?;
    match state.planes.is_empty() {
        false => state.planes.get(index).copied(),
        true if index == 0 => Some(Plane {
            offset: state.offset,
            width: state.width,
            height: state.height,
            bytes_per_row: state.bytes_per_row,
            bytes_per_element: state.bytes_per_element,
        }),
        true => None,
    }
}

/// `size_t IOSurfaceGetPlaneCount(IOSurfaceRef)`: zero for a surface that is
/// not planar.
pub unsafe extern "C" fn IOSurfaceGetPlaneCount(surface: CFTypeRef) -> usize {
    state(surface).map_or(0, |s| s.planes.len())
}

pub unsafe extern "C" fn IOSurfaceGetWidthOfPlane(surface: CFTypeRef, index: usize) -> usize {
    plane(surface, index).map_or(0, |p| p.width)
}

pub unsafe extern "C" fn IOSurfaceGetHeightOfPlane(surface: CFTypeRef, index: usize) -> usize {
    plane(surface, index).map_or(0, |p| p.height)
}

pub unsafe extern "C" fn IOSurfaceGetBytesPerRowOfPlane(surface: CFTypeRef, index: usize) -> usize {
    plane(surface, index).map_or(0, |p| p.bytes_per_row)
}

pub unsafe extern "C" fn IOSurfaceGetBytesPerElementOfPlane(
    surface: CFTypeRef,
    index: usize,
) -> usize {
    plane(surface, index).map_or(0, |p| p.bytes_per_element)
}

pub unsafe extern "C" fn IOSurfaceGetElementWidthOfPlane(
    surface: CFTypeRef,
    index: usize,
) -> usize {
    plane(surface, index).map_or(0, |_| 1)
}

pub unsafe extern "C" fn IOSurfaceGetElementHeightOfPlane(
    surface: CFTypeRef,
    index: usize,
) -> usize {
    plane(surface, index).map_or(0, |_| 1)
}

pub unsafe extern "C" fn IOSurfaceGetBaseAddressOfPlane(
    surface: CFTypeRef,
    index: usize,
) -> *mut c_void {
    match (state(surface), plane(surface, index)) {
        // SAFETY: the plane lies inside the allocation, checked when it was
        // made.
        (Some(state), Some(plane)) => unsafe { state.memory.add(plane.offset) }.cast(),
        _ => std::ptr::null_mut(),
    }
}

exports::exports! {
    functions {
        "IOSurfaceGetPlaneCount" => IOSurfaceGetPlaneCount,
        "IOSurfaceGetWidthOfPlane" => IOSurfaceGetWidthOfPlane,
        "IOSurfaceGetHeightOfPlane" => IOSurfaceGetHeightOfPlane,
        "IOSurfaceGetBytesPerRowOfPlane" => IOSurfaceGetBytesPerRowOfPlane,
        "IOSurfaceGetBytesPerElementOfPlane" => IOSurfaceGetBytesPerElementOfPlane,
        "IOSurfaceGetElementWidthOfPlane" => IOSurfaceGetElementWidthOfPlane,
        "IOSurfaceGetElementHeightOfPlane" => IOSurfaceGetElementHeightOfPlane,
        "IOSurfaceGetBaseAddressOfPlane" => IOSurfaceGetBaseAddressOfPlane,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    const NV12: u32 = u32::from_be_bytes(*b"420v");

    /// What CoreVideo lays out for a 64 by 33 `420v` buffer.
    fn nv12() -> CFTypeRef {
        corefoundation::bootstrap();
        let planes = [
            PlaneSpec {
                offset: 64,
                width: 64,
                height: 33,
                bytes_per_row: 64,
                bytes_per_element: 1,
            },
            PlaneSpec {
                offset: 64 + 64 * 33,
                width: 32,
                height: 17,
                bytes_per_row: 64,
                bytes_per_element: 2,
            },
        ];
        create_planar(NV12, 64, 33, 64 + 64 * 33 + 64 * 17, &planes)
    }

    #[test]
    fn planar_surface_reports_each_plane() {
        let surface = nv12();
        assert!(!surface.is_null());
        // SAFETY: a surface this test made.
        unsafe {
            assert_eq!(IOSurfaceGetPlaneCount(surface), 2);
            assert_eq!(IOSurfaceGetWidth(surface), 64);
            assert_eq!(IOSurfaceGetHeight(surface), 33);
            assert_eq!(IOSurfaceGetPixelFormat(surface), NV12);
            assert_eq!(IOSurfaceGetBytesPerRow(surface), 0);
            assert_eq!(IOSurfaceGetAllocSize(surface) % PAGE, 0);
            assert_eq!(IOSurfaceGetWidthOfPlane(surface, 1), 32);
            assert_eq!(IOSurfaceGetHeightOfPlane(surface, 1), 17);
            assert_eq!(IOSurfaceGetBytesPerRowOfPlane(surface, 1), 64);
            assert_eq!(IOSurfaceGetBytesPerElementOfPlane(surface, 1), 2);
            assert_eq!(IOSurfaceGetElementWidthOfPlane(surface, 1), 1);
            assert_eq!(IOSurfaceGetElementHeightOfPlane(surface, 0), 1);
            assert_eq!(IOSurfaceGetWidthOfPlane(surface, 2), 0);
            assert!(IOSurfaceGetBaseAddressOfPlane(surface, 2).is_null());
            let base = IOSurfaceGetBaseAddress(surface) as *mut u8;
            let chroma = IOSurfaceGetBaseAddressOfPlane(surface, 1) as *mut u8;
            assert_eq!(chroma, base.add(64 + 64 * 33));
            // The chroma plane's last byte is writable.
            chroma.add(64 * 17 - 1).write(1);
            corefoundation::api::CFRelease(surface);
        }
    }

    #[test]
    fn planar_surface_is_not_compositor_pixels() {
        let surface = nv12();
        assert!(is_surface(surface));
        assert!(pixels(surface).is_none());
        let packed = create_packed(PIXEL_FORMAT_BGRA, 3, 2, 4, 64, 128);
        assert!(pixels(packed).is_some());
        // SAFETY: surfaces this test made.
        unsafe {
            assert_eq!(IOSurfaceGetPlaneCount(packed), 0);
            assert_eq!(IOSurfaceGetBytesPerRowOfPlane(packed, 0), 64);
            assert_eq!(IOSurfaceGetBytesPerRow(packed), 64);
            assert_eq!(
                IOSurfaceGetBaseAddressOfPlane(packed, 0),
                IOSurfaceGetBaseAddress(packed)
            );
            corefoundation::api::CFRelease(packed);
            corefoundation::api::CFRelease(surface);
        }
    }

    #[test]
    fn a_plane_outside_the_allocation_is_refused() {
        corefoundation::bootstrap();
        let plane = PlaneSpec {
            offset: 64,
            width: 64,
            height: 2,
            bytes_per_row: 64,
            bytes_per_element: 1,
        };
        assert!(create_planar(NV12, 64, 2, 128, &[plane]).is_null());
        let narrow = PlaneSpec {
            bytes_per_row: 32,
            ..plane
        };
        assert!(create_planar(NV12, 64, 2, 4096, &[narrow]).is_null());
    }

    #[test]
    fn iosurface_create_still_refuses_plane_info() {
        use corefoundation::callbacks::Policy;
        use corefoundation::collections::{Number, new_dictionary, new_number};
        corefoundation::bootstrap();
        let planes =
            corefoundation::collections::new_array(&[], Policy::from_type_callbacks(), false);
        let options = new_dictionary(
            &[
                (key(WIDTH), new_number(Number::Integer(4))),
                (key(HEIGHT), new_number(Number::Integer(4))),
                (key(PLANE_INFO), planes),
            ],
            Policy::from_key_callbacks(),
            Policy::from_type_callbacks(),
            false,
        );
        // SAFETY: a dictionary.
        assert!(unsafe { IOSurfaceCreate(options) }.is_null());
    }
}
