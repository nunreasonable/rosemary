use super::*;

use corefoundation::callbacks::Policy;
use corefoundation::collections::{Number, new_dictionary, new_number};

fn properties(pairs: &[(&str, i64)]) -> CFTypeRef {
    corefoundation::bootstrap();
    let pairs: Vec<(CFTypeRef, CFTypeRef)> = pairs
        .iter()
        .map(|(name, value)| (key(name), new_number(Number::Integer(*value))))
        .collect();
    new_dictionary(
        &pairs,
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    )
}

/// What Qt's `QIOSurfaceGraphicsBuffer` asks for.
fn bgra(width: i64, height: i64) -> CFTypeRef {
    // SAFETY: keys this crate made.
    let row = unsafe { IOSurfaceAlignProperty(key(BYTES_PER_ROW), width as usize * 4) };
    let size = unsafe { IOSurfaceAlignProperty(key(ALLOC_SIZE), row * height as usize) };
    let options = properties(&[
        (WIDTH, width),
        (HEIGHT, height),
        (PIXEL_FORMAT, i64::from(PIXEL_FORMAT_BGRA)),
        (BYTES_PER_ELEMENT, 4),
        (BYTES_PER_ROW, row as i64),
        (ALLOC_SIZE, size as i64),
    ]);
    // SAFETY: a dictionary of numbers.
    unsafe { IOSurfaceCreate(options) }
}

#[test]
fn a_surface_has_the_layout_it_was_aligned_for() {
    let surface = bgra(600, 300);
    assert!(!surface.is_null());
    // SAFETY: a surface this test made.
    unsafe {
        let row = IOSurfaceAlignProperty(key(BYTES_PER_ROW), 600 * 4);
        assert_eq!(row % ROW_ALIGNMENT, 0);
        assert!(row >= 2400);
        assert_eq!(IOSurfaceGetBytesPerRow(surface), row);
        assert_eq!(IOSurfaceGetWidth(surface), 600);
        assert_eq!(IOSurfaceGetHeight(surface), 300);
        assert_eq!(IOSurfaceGetBytesPerElement(surface), 4);
        assert_eq!(IOSurfaceGetPixelFormat(surface), PIXEL_FORMAT_BGRA);
        assert!(IOSurfaceGetAllocSize(surface) >= row * 300);
        assert_eq!(IOSurfaceGetAllocSize(surface) % PAGE, 0);
        let base = IOSurfaceGetBaseAddress(surface) as *mut u8;
        assert!(!base.is_null());
        // The whole of the last row is writable.
        base.add(row * 299 + 2399).write(7);
        assert_eq!(
            IOSurfaceGetTypeID(),
            corefoundation::api::CFGetTypeID(surface)
        );
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn a_surface_without_a_size_is_not_made() {
    // SAFETY: a dictionary of numbers.
    assert!(unsafe { IOSurfaceCreate(properties(&[(WIDTH, 4)])) }.is_null());
    assert!(unsafe { IOSurfaceCreate(properties(&[(WIDTH, 4), (HEIGHT, -1)])) }.is_null());
}

/// `kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange`.
const NV12: u32 = u32::from_be_bytes(*b"420v");

/// A surface whose `kIOSurfacePlaneInfo` is `plane_info`, whatever that is.
fn with_plane_info(width: i64, height: i64, plane_info: CFTypeRef) -> CFTypeRef {
    corefoundation::bootstrap();
    let options = new_dictionary(
        &[
            (key(WIDTH), new_number(Number::Integer(width))),
            (key(HEIGHT), new_number(Number::Integer(height))),
            (
                key(PIXEL_FORMAT),
                new_number(Number::Integer(i64::from(NV12))),
            ),
            (key(PLANE_INFO), plane_info),
        ],
        Policy::from_key_callbacks(),
        Policy::from_type_callbacks(),
        false,
    );
    // SAFETY: a dictionary.
    unsafe { IOSurfaceCreate(options) }
}

fn planar(width: i64, height: i64, planes: &[CFTypeRef]) -> CFTypeRef {
    let list = corefoundation::collections::new_array(planes, Policy::from_type_callbacks(), false);
    with_plane_info(width, height, list)
}

/// Luma, and chroma interleaved at half the size each way, with the rows,
/// sizes and offsets left to the surface.
fn nv12(width: i64, height: i64) -> CFTypeRef {
    planar(
        width,
        height,
        &[
            properties(&[
                (PLANE_WIDTH, width),
                (PLANE_HEIGHT, height),
                (PLANE_BYTES_PER_ELEMENT, 1),
            ]),
            properties(&[
                (PLANE_WIDTH, (width + 1) / 2),
                (PLANE_HEIGHT, (height + 1) / 2),
                (PLANE_BYTES_PER_ELEMENT, 2),
            ]),
        ],
    )
}

#[test]
fn a_biplanar_surface_has_a_layout_per_plane() {
    let surface = nv12(6, 4);
    assert!(!surface.is_null());
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceGetPlaneCount(surface), 2);
        assert_eq!(IOSurfaceGetWidthOfPlane(surface, 0), 6);
        assert_eq!(IOSurfaceGetHeightOfPlane(surface, 0), 4);
        assert_eq!(IOSurfaceGetBytesPerElementOfPlane(surface, 0), 1);
        assert_eq!(IOSurfaceGetBytesPerRowOfPlane(surface, 0), ROW_ALIGNMENT);
        assert_eq!(IOSurfaceGetWidthOfPlane(surface, 1), 3);
        assert_eq!(IOSurfaceGetHeightOfPlane(surface, 1), 2);
        assert_eq!(IOSurfaceGetBytesPerElementOfPlane(surface, 1), 2);
        assert_eq!(IOSurfaceGetBytesPerRowOfPlane(surface, 1), ROW_ALIGNMENT);
        // The surface keeps its size and format; its element and row are the
        // planes', not its own.
        assert_eq!(IOSurfaceGetWidth(surface), 6);
        assert_eq!(IOSurfaceGetHeight(surface), 4);
        assert_eq!(IOSurfaceGetPixelFormat(surface), NV12);
        assert_eq!(IOSurfaceGetBytesPerElement(surface), 0);
        assert_eq!(IOSurfaceGetBytesPerRow(surface), 0);
        assert_eq!(IOSurfaceGetElementWidth(surface), 0);
        assert_eq!(IOSurfaceGetElementHeight(surface), 0);
        // A page for each plane, one after the other.
        assert_eq!(IOSurfaceGetAllocSize(surface), 2 * PAGE);
        let (luma, chroma) = (
            IOSurfaceGetBaseAddressOfPlane(surface, 0) as *mut u8,
            IOSurfaceGetBaseAddressOfPlane(surface, 1) as *mut u8,
        );
        assert_eq!(luma, IOSurfaceGetBaseAddress(surface) as *mut u8);
        assert_eq!(chroma as usize - luma as usize, PAGE);
        // The last byte of the last row of each is writable, and apart.
        luma.add(ROW_ALIGNMENT * 3 + 5).write(1);
        chroma.add(ROW_ALIGNMENT + 5).write(2);
        // No third plane.
        assert_eq!(IOSurfaceGetWidthOfPlane(surface, 2), 0);
        assert_eq!(IOSurfaceGetBytesPerRowOfPlane(surface, 2), 0);
        assert!(IOSurfaceGetBaseAddressOfPlane(surface, 2).is_null());
        corefoundation::api::CFRelease(surface);
    }
}

/// What Chromium's `CreateIOSurface` asks for an NV12 buffer: every plane
/// property spelled out, aligned as the surface said to.
#[test]
fn the_plane_layout_a_program_asked_for_is_the_one_it_gets() {
    let (width, height) = (1536, 864);
    // SAFETY: keys this crate made.
    let aligned =
        |name, value: i64| unsafe { IOSurfaceAlignProperty(key(name), value as usize) } as i64;
    let luma_row = aligned(PLANE_BYTES_PER_ROW, width);
    let luma_size = aligned(PLANE_SIZE, luma_row * height);
    let chroma_row = aligned(PLANE_BYTES_PER_ROW, width / 2 * 2);
    let chroma_size = aligned(PLANE_SIZE, chroma_row * height / 2);
    let surface = planar(
        width,
        height,
        &[
            properties(&[
                (PLANE_WIDTH, width),
                (PLANE_HEIGHT, height),
                (PLANE_BYTES_PER_ELEMENT, 1),
                (PLANE_BYTES_PER_ROW, luma_row),
                (PLANE_SIZE, luma_size),
                (PLANE_OFFSET, 0),
            ]),
            properties(&[
                (PLANE_WIDTH, width / 2),
                (PLANE_HEIGHT, height / 2),
                (PLANE_BYTES_PER_ELEMENT, 2),
                (PLANE_BYTES_PER_ROW, chroma_row),
                (PLANE_SIZE, chroma_size),
                (PLANE_OFFSET, luma_size),
            ]),
        ],
    );
    assert!(!surface.is_null());
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceGetBytesPerRowOfPlane(surface, 0) as i64, luma_row);
        assert_eq!(
            IOSurfaceGetBytesPerRowOfPlane(surface, 1) as i64,
            chroma_row
        );
        assert_eq!(
            IOSurfaceGetAllocSize(surface) as i64,
            luma_size + chroma_size
        );
        let luma = IOSurfaceGetBaseAddressOfPlane(surface, 0) as usize;
        let chroma = IOSurfaceGetBaseAddressOfPlane(surface, 1) as usize;
        assert_eq!((chroma - luma) as i64, luma_size);
        corefoundation::api::CFRelease(surface);

        assert_eq!(
            IOSurfaceAlignProperty(key(PLANE_BYTES_PER_ROW), 5),
            ROW_ALIGNMENT
        );
        assert_eq!(IOSurfaceAlignProperty(key(PLANE_SIZE), 1), PAGE);
        assert_eq!(IOSurfaceGetPropertyAlignment(key(PLANE_SIZE)), PAGE);
        assert_eq!(IOSurfaceGetPropertyMaximum(key(PLANE_WIDTH)), MAX_EXTENT);
    }
}

#[test]
fn a_surface_that_is_one_plane_answers_plane_zero_as_itself() {
    let surface = bgra(4, 3);
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceGetPlaneCount(surface), 0);
        assert_eq!(IOSurfaceGetWidthOfPlane(surface, 0), 4);
        assert_eq!(IOSurfaceGetHeightOfPlane(surface, 0), 3);
        assert_eq!(IOSurfaceGetBytesPerElementOfPlane(surface, 0), 4);
        assert_eq!(
            IOSurfaceGetBytesPerRowOfPlane(surface, 0),
            IOSurfaceGetBytesPerRow(surface)
        );
        assert_eq!(
            IOSurfaceGetBaseAddressOfPlane(surface, 0),
            IOSurfaceGetBaseAddress(surface)
        );
        assert_eq!(IOSurfaceGetWidthOfPlane(surface, 1), 0);
        assert!(IOSurfaceGetBaseAddressOfPlane(surface, 1).is_null());
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn a_compositor_reads_a_plane_as_a_surface_of_its_own() {
    let surface = nv12(6, 4);
    // SAFETY: a surface this test made.
    let chroma_base = unsafe { IOSurfaceGetBaseAddressOfPlane(surface, 1) } as *const u8;
    let chroma = pixels_of_plane(surface, 1).expect("a second plane");
    assert_eq!(
        (chroma.width, chroma.height, chroma.bytes_per_element),
        (3, 2, 2)
    );
    assert_eq!(chroma.bytes_per_row, ROW_ALIGNMENT);
    assert_eq!(chroma.data, chroma_base);
    assert_eq!(chroma.pixel_format, NV12);
    let luma = pixels(surface).expect("a surface");
    assert_eq!((luma.width, luma.height, luma.bytes_per_element), (6, 4, 1));
    assert!(pixels_of_plane(surface, 2).is_none());
    assert!(pixels_of_plane(std::ptr::null(), 0).is_none());
    // SAFETY: a surface this test made.
    unsafe { corefoundation::api::CFRelease(surface) };
}

#[test]
fn a_plane_list_that_cannot_be_laid_out_is_refused() {
    corefoundation::bootstrap();
    let plane = |pairs: &[(&str, i64)]| properties(pairs);
    let sized = |width, height| plane(&[(PLANE_WIDTH, width), (PLANE_HEIGHT, height)]);
    let array = |planes: &[CFTypeRef]| {
        corefoundation::collections::new_array(planes, Policy::from_type_callbacks(), false)
    };
    let cases: [(&str, CFTypeRef); 8] = [
        ("no planes", array(&[])),
        (
            "too many planes",
            array(&[
                sized(4, 4),
                sized(4, 4),
                sized(4, 4),
                sized(4, 4),
                sized(4, 4),
            ]),
        ),
        (
            "a plane with no width",
            array(&[plane(&[(PLANE_HEIGHT, 4)])]),
        ),
        (
            "a negative offset",
            array(&[plane(&[
                (PLANE_WIDTH, 4),
                (PLANE_HEIGHT, 4),
                (PLANE_OFFSET, -1),
            ])]),
        ),
        (
            "planes on top of each other",
            array(&[
                sized(4, 4),
                plane(&[(PLANE_WIDTH, 4), (PLANE_HEIGHT, 4), (PLANE_OFFSET, 100)]),
            ]),
        ),
        ("a plane past the maximum", array(&[sized(20000, 4)])),
        (
            "a list of numbers",
            array(&[new_number(Number::Integer(1))]),
        ),
        ("a number for a list", new_number(Number::Integer(1))),
    ];
    for (name, plane_info) in cases {
        assert!(
            with_plane_info(4, 4, plane_info).is_null(),
            "{name} is not a surface"
        );
    }
}

#[test]
fn planes_arrive_in_another_process_as_they_left() {
    let surface = nv12(6, 4);
    let sent = state(surface).expect("a surface");
    // What `IOSurfaceLookupFromMachPort` does with the file it is given.
    let receive = || make(sent.file.clone(), sent.mapped, None);
    let received = receive().expect("a surface");
    let there = state(received).expect("a surface");
    assert_eq!(there.planes, sent.planes);
    assert_eq!(there.planes.len(), 2);
    assert_eq!(there.bytes_per_row, 0);
    // SAFETY: two surfaces this test made, over the same memory.
    unsafe {
        (IOSurfaceGetBaseAddressOfPlane(surface, 1) as *mut u8).write(9);
        assert_eq!(
            (IOSurfaceGetBaseAddressOfPlane(received, 1) as *const u8).read(),
            9
        );
        corefoundation::api::CFRelease(received);
    }

    // A header that describes planes the allocation cannot hold is refused,
    // not read from.
    let header = sent.mapping as *mut Header;
    let corrupt = |change: &dyn Fn(&mut Header)| {
        // SAFETY: the first page of the surface's own mapping.
        let before = unsafe { header.read() };
        let mut after = before;
        change(&mut after);
        // SAFETY: as above.
        unsafe { header.write(after) };
        let refused = receive().is_none();
        // SAFETY: as above.
        unsafe { header.write(before) };
        refused
    };
    assert!(corrupt(&|h| h.plane_count = MAX_PLANES as u32 + 1));
    assert!(corrupt(&|h| h.planes[1].offset = h.alloc_size));
    assert!(corrupt(&|h| h.planes[1].offset = 0));
    assert!(corrupt(&|h| h.planes[0].bytes_per_row = u64::MAX));
    assert!(corrupt(&|h| h.offset = h.alloc_size + 1));
    let intact = receive().expect("a surface");
    // SAFETY: surfaces this test made.
    unsafe {
        corefoundation::api::CFRelease(intact);
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn defaults_are_one_byte_per_element() {
    // SAFETY: a dictionary of numbers.
    let surface = unsafe { IOSurfaceCreate(properties(&[(WIDTH, 3), (HEIGHT, 2)])) };
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceGetBytesPerElement(surface), 1);
        assert_eq!(IOSurfaceGetElementWidth(surface), 1);
        assert_eq!(IOSurfaceGetBytesPerRow(surface), ROW_ALIGNMENT);
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn a_write_unlock_moves_the_seed_and_a_read_unlock_does_not() {
    let surface = bgra(2, 2);
    let mut seed = u32::MAX;
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceUnlock(surface, 0, &mut seed), NOT_LOCKED);
        assert_eq!(IOSurfaceLock(surface, 0, &mut seed), SUCCESS);
        assert_eq!(seed, 0);
        assert_eq!(IOSurfaceUnlock(surface, 0, &mut seed), SUCCESS);
        assert_eq!(seed, 1);
        IOSurfaceLock(surface, LOCK_READ_ONLY, std::ptr::null_mut());
        IOSurfaceUnlock(surface, LOCK_READ_ONLY, &mut seed);
        assert_eq!(seed, 1);
        assert_eq!(IOSurfaceGetSeed(surface), 1);
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn the_use_count_is_what_in_use_reports() {
    let surface = bgra(2, 2);
    // SAFETY: a surface this test made.
    unsafe {
        assert_eq!(IOSurfaceIsInUse(surface), 0);
        IOSurfaceIncrementUseCount(surface);
        assert_eq!(IOSurfaceIsInUse(surface), 1);
        assert_eq!(IOSurfaceGetUseCount(surface), 1);
        IOSurfaceDecrementUseCount(surface);
        IOSurfaceDecrementUseCount(surface);
        assert_eq!(IOSurfaceGetUseCount(surface), 0, "never below zero");
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn values_are_set_replaced_copied_and_removed() {
    let surface = bgra(2, 2);
    let name = key("IOSurfaceColorSpace");
    let first = new_number(Number::Integer(1));
    let second = new_number(Number::Integer(2));
    // SAFETY: a surface and CF objects this test made.
    unsafe {
        IOSurfaceSetValue(surface, name, first);
        IOSurfaceSetValue(surface, name, second);
        let copied = IOSurfaceCopyValue(surface, name);
        assert_eq!(copied, second);
        corefoundation::api::CFRelease(copied);
        let all = IOSurfaceCopyAllValues(surface);
        assert_eq!(corefoundation::api::CFDictionaryGetCount(all), 1);
        corefoundation::api::CFRelease(all);
        IOSurfaceRemoveValue(surface, name);
        assert!(IOSurfaceCopyValue(surface, name).is_null());
        corefoundation::api::CFRelease(surface);
    }
}

#[test]
fn a_surface_is_found_by_its_id_until_it_goes() {
    let surface = bgra(2, 2);
    // SAFETY: a surface this test made.
    unsafe {
        let id = IOSurfaceGetID(surface);
        assert_ne!(id, 0);
        let found = IOSurfaceLookup(id);
        assert_eq!(found, surface);
        corefoundation::api::CFRelease(found);
        corefoundation::api::CFRelease(surface);
        assert!(IOSurfaceLookup(id).is_null());
    }
}

#[test]
fn every_key_is_published_as_a_cell_holding_its_string() {
    let symbols = data_symbols();
    let (_, cell) = symbols
        .iter()
        .find(|(name, _)| name == "_kIOSurfaceBytesPerRow")
        .expect("published");
    // SAFETY: a cell `global_cell` leaked, holding a string.
    let string = unsafe { *(*cell as *const u64) } as CFTypeRef;
    // SAFETY: two CF strings.
    assert_ne!(
        unsafe { corefoundation::api::CFEqual(string, key(BYTES_PER_ROW)) },
        0
    );
}
