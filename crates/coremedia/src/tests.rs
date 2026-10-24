use super::*;

/// Chromium's 480p probe stream: the sets its GPU process checks hardware
/// decoding with.
const SPS: &[u8] = &[
    0x67, 0x64, 0x00, 0x1e, 0xac, 0xd9, 0x80, 0xd4, 0x3d, 0xa1, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00,
    0x00, 0x03, 0x00, 0x30, 0x8f, 0x16, 0x2d, 0x9a,
];
const PPS: &[u8] = &[0x68, 0xe9, 0x7b, 0xcb];

fn from_sets(sets: &[&[u8]], header_length: i32) -> (i32, CFTypeRef) {
    let pointers: Vec<*const u8> = sets.iter().map(|s| s.as_ptr()).collect();
    let sizes: Vec<usize> = sets.iter().map(|s| s.len()).collect();
    let mut out: CFTypeRef = std::ptr::null();
    // SAFETY: two arrays of `sets.len()` entries and a live out-parameter.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromH264ParameterSets(
            std::ptr::null(),
            sets.len(),
            pointers.as_ptr(),
            sizes.as_ptr(),
            header_length,
            &mut out,
        )
    };
    (status, out)
}

#[test]
fn a_description_from_parameter_sets_is_h264_video_of_the_coded_size() {
    let (status, description) = from_sets(&[SPS, PPS], 4);
    assert_eq!(status, NO_ERR);
    // SAFETY: a description just made, released at the end.
    unsafe {
        assert_eq!(
            corefoundation::api::CFGetTypeID(description),
            CMFormatDescriptionGetTypeID()
        );
        assert_eq!(CMFormatDescriptionGetMediaType(description), MEDIA_VIDEO);
        assert_eq!(CMFormatDescriptionGetMediaSubType(description), CODEC_H264);
        assert_eq!(
            CMVideoFormatDescriptionGetDimensions(description),
            Dimensions {
                width: 848,
                height: 480
            }
        );
        assert_eq!(
            CMVideoFormatDescriptionGetCleanAperture(description, 1),
            Rect {
                x: 0.0,
                y: 0.0,
                width: 848.0,
                height: 480.0
            }
        );
        corefoundation::api::CFRelease(description);
    }
}

#[test]
fn the_parameter_sets_come_back_by_index_from_the_avcc_atom() {
    let (_, description) = from_sets(&[SPS, PPS], 4);
    let (mut set, mut size, mut count, mut length) = (std::ptr::null(), 0usize, 0usize, 0i32);
    // SAFETY: live out-parameters, then null ones.
    unsafe {
        let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description,
            1,
            &mut set,
            &mut size,
            &mut count,
            &mut length,
        );
        assert_eq!(status, NO_ERR);
        assert_eq!((count, length), (2, 4));
        assert_eq!(std::slice::from_raw_parts(set, size), PPS);
        let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description,
            0,
            &mut set,
            &mut size,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        assert_eq!(status, NO_ERR);
        assert_eq!(std::slice::from_raw_parts(set, size), SPS);
        assert_eq!(
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                description,
                2,
                &mut set,
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            INVALID_PARAMETER
        );

        let atoms = CMFormatDescriptionGetExtension(description, key(ATOMS));
        assert!(
            !atoms.is_null(),
            "the atom is an extension a caller can read"
        );
        corefoundation::api::CFRelease(description);
    }
}

#[test]
fn parameter_sets_are_checked_before_anything_is_made() {
    assert_eq!(
        from_sets(&[SPS], 4).0,
        INVALID_PARAMETER,
        "one set is too few"
    );
    assert_eq!(from_sets(&[PPS, PPS], 4).0, INVALID_PARAMETER, "no SPS");
    assert_eq!(
        from_sets(&[SPS, PPS], 3).0,
        INVALID_PARAMETER,
        "a length of 3"
    );
    assert_eq!(
        from_sets(&[SPS, &[0x65, 0x88]], 4).0,
        INVALID_PARAMETER,
        "a slice is not a parameter set"
    );
    assert_eq!(
        from_sets(&[&SPS[..5], PPS], 4).0,
        INVALID_PARAMETER,
        "a truncated SPS"
    );
}

#[test]
fn a_clean_aperture_is_centred_with_its_offsets() {
    // SAFETY: CF objects made and released here.
    unsafe {
        let number = |value: i32| {
            corefoundation::api::CFNumberCreate(
                std::ptr::null(),
                corefoundation::collections::number_type::SINT32,
                (&raw const value).cast(),
            )
        };
        let values = [number(1280), number(700), number(-10), number(4)];
        let aperture = dictionary(&[
            (key(APERTURE_WIDTH), values[0]),
            (key(APERTURE_HEIGHT), values[1]),
            (key(APERTURE_HORIZONTAL_OFFSET), values[2]),
            (key(APERTURE_VERTICAL_OFFSET), values[3]),
        ]);
        let extensions = dictionary(&[(key(CLEAN_APERTURE), aperture)]);
        let mut description = std::ptr::null();
        assert_eq!(
            CMVideoFormatDescriptionCreate(
                std::ptr::null(),
                CODEC_H264,
                1280,
                720,
                extensions,
                &mut description
            ),
            NO_ERR
        );
        // Ten columns left of centre, and four rows below it.
        let top_left = CMVideoFormatDescriptionGetCleanAperture(description, 1);
        assert_eq!(
            top_left,
            Rect {
                x: -10.0,
                y: 14.0,
                width: 1280.0,
                height: 700.0
            }
        );
        let bottom_left = CMVideoFormatDescriptionGetCleanAperture(description, 0);
        assert_eq!(bottom_left.y, 6.0);
        for object in values
            .into_iter()
            .chain([aperture, extensions, description])
        {
            corefoundation::api::CFRelease(object);
        }
    }
}

#[test]
fn descriptions_with_the_same_contents_are_equal() {
    // `CFEqual` of two dictionaries is Foundation's `-isEqual:`.
    foundation::bootstrap();
    let (_, a) = from_sets(&[SPS, PPS], 4);
    let (_, b) = from_sets(&[SPS, PPS], 4);
    let (_, c) = from_sets(&[SPS, PPS], 2);
    // SAFETY: descriptions just made, released here.
    unsafe {
        assert_ne!(a, b);
        assert_eq!(CMFormatDescriptionEqual(a, b), 1);
        assert_eq!(CMFormatDescriptionEqual(a, c), 0, "the atom differs");
        for object in [a, b, c] {
            corefoundation::api::CFRelease(object);
        }
    }
}

#[test]
fn the_extension_keys_are_one_object_each() {
    let symbols = data_symbols();
    let (_, cell) = symbols
        .iter()
        .find(|(name, _)| name == "_kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms")
        .expect("published");
    // SAFETY: a global cell holding a string object.
    let object = unsafe { *(*cell as *const CFTypeRef) };
    assert_eq!(object, key(ATOMS));
    let (_, invalid) = symbols
        .iter()
        .find(|(name, _)| name == "_kCMTimeInvalid")
        .expect("published");
    // SAFETY: the static `CMTime` the symbol names.
    let time = unsafe { &*(*invalid as *const Time) };
    assert_eq!(
        (time.value, time.timescale, time.flags, time.epoch),
        (0, 0, 0, 0)
    );
}

#[test]
fn hevc_parameter_sets_are_refused_for_lack_of_a_parser() {
    let vps: &[u8] = &[0x40, 0x01];
    let pointers: [*const u8; 1] = [vps.as_ptr()];
    let sizes: [usize; 1] = [vps.len()];
    let mut out: CFTypeRef = 1 as CFTypeRef;
    // SAFETY: one array of one entry each and a live out-parameter.
    let status = unsafe {
        CMVideoFormatDescriptionCreateFromHEVCParameterSets(
            std::ptr::null(),
            1,
            pointers.as_ptr(),
            sizes.as_ptr(),
            4,
            std::ptr::null(),
            &mut out,
        )
    };
    assert_eq!(status, INVALID_PARAMETER);
    assert!(out.is_null(), "no description is ever made");
}

#[test]
fn hevc_parameter_set_lookup_is_refused_on_any_description() {
    let (status, description) = from_sets(&[SPS, PPS], 4);
    assert_eq!(status, NO_ERR, "an H.264 description to ask the wrong way");
    // SAFETY: the description just made, released at the end.
    unsafe {
        assert_eq!(
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                description,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            ),
            INVALID_PARAMETER
        );
        corefoundation::api::CFRelease(description);
    }
}
