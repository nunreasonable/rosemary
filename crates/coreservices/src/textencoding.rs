//! `TextEncoding` arithmetic, the Script Manager's upgrade path to it, and the
//! Text Encoding Converter (`TEC*`) that moves bytes between two of them.
//!
//! `TextEncoding` and `CFStringEncoding` share Apple's numbering for every
//! encoding this tree answers - see `corefoundation::string::encoding` - so the
//! conversions here work directly in that space rather than keeping a second
//! table.

use std::ffi::c_void;

use corefoundation::string::encoding;

type OSStatus = i32;

const NO_ERR: OSStatus = 0;
/// `paramErr`.
const PARAM_ERR: OSStatus = -50;
/// `kTECOutputBufferFullStatus`: there was more to convert than `outputBuffer`
/// could hold, and what fits has been written.
const OUTPUT_BUFFER_FULL: OSStatus = -8785;

/// `TextEncoding CreateTextEncoding(TextEncodingBase, TextEncodingVariant,
/// TextEncodingFormat)`.
///
/// A base, a variant and a format packed into one `UInt32` - sixteen bits,
/// eight bits, eight bits - which is also how every encoding this tree already
/// answers is laid out: `kTextEncodingMacRoman` is base 0 with the default
/// variant and format, `kTextEncodingUTF8` is base 0x0100 (Unicode) in format 8,
/// and so on through `corefoundation::string::encoding`.
pub unsafe extern "C" fn CreateTextEncoding(base: u32, variant: u32, format: u32) -> u32 {
    create_text_encoding(base, variant, format)
}

fn create_text_encoding(base: u32, variant: u32, format: u32) -> u32 {
    (base & 0xffff) | ((variant & 0xff) << 16) | ((format & 0xff) << 24)
}

/// `Fixed Long2Fix(long)`.
///
/// A `Fixed` is 16.16 fixed point, so this is the integer in the top half and
/// zero below it - the same wraparound on overflow Apple's own macro has,
/// since neither clamps.
pub unsafe extern "C" fn Long2Fix(the_long: i32) -> i32 {
    the_long.wrapping_mul(0x1_0000)
}

/// `OSStatus UpgradeScriptInfoToTextEncoding(ScriptCode scriptCode, LangCode
/// langCode, RegionCode regionCode, ConstStr255Param scriptName, TextEncoding
/// *textEncoding)`
///
/// The Script Manager's thirty-three base scripts (`smRoman` through
/// `smUninterp`) are numbered exactly as the `kTextEncodingMacXXX` bases they
/// upgrade to, so a script code in that range is the base encoding itself.
///
/// FIXME (ACCURACY): `langCode` and `regionCode` can steer Apple toward a more
/// specific *variant* of the same base (a different 8-bit table for the one
/// script) when the base alone is ambiguous; that steering is not modeled, so
/// every script answers its default variant and format. `scriptName` is a
/// fallback used only when `scriptCode` is `smScriptUnspecified` or similarly
/// unusable, which this tree never receives from its own callers and does not
/// parse.
///
/// # Safety
///
/// `text_encoding` is null or writable for a `TextEncoding`.
pub unsafe extern "C" fn UpgradeScriptInfoToTextEncoding(
    script_code: i16,
    _lang_code: i16,
    _region_code: i16,
    _script_name: *const u8,
    text_encoding: *mut u32,
) -> OSStatus {
    if text_encoding.is_null() {
        return PARAM_ERR;
    }
    let Ok(base) = u16::try_from(script_code) else {
        return PARAM_ERR;
    };
    if base > 32 {
        return PARAM_ERR;
    }
    // SAFETY: checked non-null above.
    unsafe { *text_encoding = create_text_encoding(u32::from(base), 0, 0) };
    NO_ERR
}

/// A Pascal string's text: the first byte is the length, the rest are its
/// characters in Mac OS Roman, which is how the Script Manager spelled a
/// charset or script name.
///
/// # Safety
///
/// `pascal` is null or a byte, then that many more.
unsafe fn pascal_string(pascal: *const u8) -> Option<String> {
    if pascal.is_null() {
        return None;
    }
    // SAFETY: the caller states at least one byte is readable.
    let length = usize::from(unsafe { *pascal });
    // SAFETY: the caller states `length` more bytes follow.
    let bytes = unsafe { std::slice::from_raw_parts(pascal.add(1), length) };
    let units: Vec<u16> = bytes
        .iter()
        .filter_map(|b| corefoundation::eightbit::decode(*b, encoding::MAC_ROMAN))
        .collect();
    if units.len() != bytes.len() {
        return None;
    }
    String::from_utf16(&units).ok()
}

/// `OSStatus TECGetTextEncodingFromInternetName(TextEncoding
/// *encoding, ConstStr255Param internetName)`
///
/// # Safety
///
/// `text_encoding` is null or writable for a `TextEncoding`; `internet_name`
/// is a Pascal string.
pub unsafe extern "C" fn TECGetTextEncodingFromInternetName(
    text_encoding: *mut u32,
    internet_name: *const u8,
) -> OSStatus {
    if text_encoding.is_null() {
        return PARAM_ERR;
    }
    // SAFETY: a Pascal string, by the caller's contract.
    let Some(name) = (unsafe { pascal_string(internet_name) }) else {
        return PARAM_ERR;
    };
    let Some(found) = corefoundation::api::encoding_for_charset_name(&name) else {
        return PARAM_ERR;
    };
    // SAFETY: checked non-null above.
    unsafe { *text_encoding = found };
    NO_ERR
}

/// A live `TECObjectRef`: the two encodings a conversion moves between.
///
/// Stateless on purpose - nothing this tree converts needs a shift-state
/// carried across calls - so [`TECClearConverterContextInfo`] has nothing to
/// clear and every call to [`TECConvertText`] starts fresh.
struct Converter {
    input: u32,
    output: u32,
}

/// `OSStatus TECCreateConverter(TECObjectRef *tec, TextEncoding
/// inputEncoding, TextEncoding outputEncoding)`
///
/// # Safety
///
/// `tec` is writable for a `TECObjectRef`.
pub unsafe extern "C" fn TECCreateConverter(
    tec: *mut *mut c_void,
    input_encoding: u32,
    output_encoding: u32,
) -> OSStatus {
    if tec.is_null() {
        return PARAM_ERR;
    }
    let converter = Box::into_raw(Box::new(Converter {
        input: input_encoding,
        output: output_encoding,
    }));
    // SAFETY: checked non-null above.
    unsafe { *tec = converter.cast() };
    NO_ERR
}

/// `OSStatus TECDisposeConverter(TECObjectRef tec)`
///
/// # Safety
///
/// `tec` is null or a `TECObjectRef` [`TECCreateConverter`] made and that has
/// not already been disposed.
pub unsafe extern "C" fn TECDisposeConverter(tec: *mut c_void) -> OSStatus {
    if tec.is_null() {
        return PARAM_ERR;
    }
    // SAFETY: the caller's contract.
    drop(unsafe { Box::from_raw(tec.cast::<Converter>()) });
    NO_ERR
}

/// `OSStatus TECClearConverterContextInfo(TECObjectRef tec)`
///
/// There is no shift state here to clear - see [`Converter`] - so this only
/// validates that `tec` is one.
///
/// # Safety
///
/// `tec` is null or live.
pub unsafe extern "C" fn TECClearConverterContextInfo(tec: *mut c_void) -> OSStatus {
    match tec.is_null() {
        true => PARAM_ERR,
        false => NO_ERR,
    }
}

/// The character at the start of `bytes` under `code`, and how many bytes of
/// `bytes` it took - or `None` for an empty, truncated or malformed lead.
///
/// UTF-8 is checked before the generic eight-bit case below even though
/// `eightbit::is_table` also answers true for it: that table is one byte per
/// character, which is only UTF-8's answer for the half of it that is ASCII.
fn decode_one(bytes: &[u8], code: u32) -> Option<(char, usize)> {
    if code == encoding::UTF8 {
        return (1..=bytes.len().min(4)).find_map(|n| {
            std::str::from_utf8(&bytes[..n])
                .ok()
                .and_then(|s| s.chars().next())
                .map(|c| (c, n))
        });
    }
    if corefoundation::eightbit::is_table(code) {
        let byte = *bytes.first()?;
        let unit = corefoundation::eightbit::decode(byte, code)?;
        return char::from_u32(u32::from(unit)).map(|c| (c, 1));
    }
    match code {
        encoding::UTF16 | encoding::UTF16_BE | encoding::UTF16_LE => {
            let read = |at: &[u8]| -> Option<u16> {
                let pair = <[u8; 2]>::try_from(at.get(..2)?).ok()?;
                Some(match code == encoding::UTF16_BE {
                    true => u16::from_be_bytes(pair),
                    // Order-unspecified `UTF16` is taken as host order, as a
                    // stream with no byte-order mark is.
                    false => u16::from_le_bytes(pair),
                })
            };
            let high = read(bytes)?;
            if !(0xd800..=0xdbff).contains(&high) {
                return char::from_u32(u32::from(high)).map(|c| (c, 2));
            }
            let low = read(bytes.get(2..)?)?;
            if !(0xdc00..=0xdfff).contains(&low) {
                return None;
            }
            let scalar = 0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
            char::from_u32(scalar).map(|c| (c, 4))
        }
        encoding::UTF32 | encoding::UTF32_BE | encoding::UTF32_LE => {
            let quad = <[u8; 4]>::try_from(bytes.get(..4)?).ok()?;
            let scalar = match code == encoding::UTF32_BE {
                true => u32::from_be_bytes(quad),
                false => u32::from_le_bytes(quad),
            };
            char::from_u32(scalar).map(|c| (c, 4))
        }
        _ => None,
    }
}

/// `ch` as bytes under `code`.
///
/// FIXME (ACCURACY): a character `code` has no byte for - any non-Latin
/// character in an eight-bit encoding - is substituted with `?` rather than
/// failing the conversion with `kTECUnmappableElementErr`, which is what Apple
/// does unless the caller opted into fallback substitution. Substituting keeps
/// every call making progress; a caller that needs to know a character was
/// lost is the one case this approximates.
fn encode_one(ch: char, code: u32) -> Vec<u8> {
    // UTF-8 before the generic eight-bit case, for the reason `decode_one`
    // checks it first.
    if code == encoding::UTF8 {
        return ch.to_string().into_bytes();
    }
    if corefoundation::eightbit::is_table(code) {
        let unit = u16::try_from(ch as u32).ok();
        let byte = unit.and_then(|u| corefoundation::eightbit::encode(u, code));
        return vec![byte.unwrap_or(b'?')];
    }
    match code {
        encoding::UTF16 | encoding::UTF16_BE | encoding::UTF16_LE => {
            let mut units = [0_u16; 2];
            ch.encode_utf16(&mut units)
                .iter()
                .flat_map(|unit| match code == encoding::UTF16_BE {
                    true => unit.to_be_bytes(),
                    false => unit.to_le_bytes(),
                })
                .collect()
        }
        encoding::UTF32 | encoding::UTF32_BE | encoding::UTF32_LE => {
            let scalar = ch as u32;
            match code == encoding::UTF32_BE {
                true => scalar.to_be_bytes().to_vec(),
                false => scalar.to_le_bytes().to_vec(),
            }
        }
        // Anything this tree does not otherwise recognize converts as UTF-8,
        // which is at least never a lossier answer than refusing outright.
        _ => ch.to_string().into_bytes(),
    }
}

/// `OSStatus TECConvertText(TECObjectRef tec, const UInt8 *inputBuffer,
/// ByteCount inputBufferLength, ByteCount *actualInputLength, UInt8
/// *outputBuffer, ByteCount outputBufferLength, ByteCount *actualOutputLength)`
///
/// Converts whole characters at a time, stopping and reporting
/// [`OUTPUT_BUFFER_FULL`] the moment one more would not fit - never writing
/// past `outputBufferLength`, and never reporting a character consumed whose
/// output was not written.
///
/// # Safety
///
/// `tec` is a live `TECObjectRef`; `input_buffer` is null or readable for
/// `input_buffer_length` bytes; `output_buffer` is null or writable for
/// `output_buffer_length` bytes; the two actual-length outputs are null or
/// writable for a `ByteCount`.
pub unsafe extern "C" fn TECConvertText(
    tec: *mut c_void,
    input_buffer: *const u8,
    input_buffer_length: u64,
    actual_input_length: *mut u64,
    output_buffer: *mut u8,
    output_buffer_length: u64,
    actual_output_length: *mut u64,
) -> OSStatus {
    if tec.is_null() {
        return PARAM_ERR;
    }
    // SAFETY: a converter `TECCreateConverter` made and not yet disposed.
    let converter = unsafe { &*tec.cast::<Converter>() };
    let input = match input_buffer.is_null() {
        true => &[][..],
        // SAFETY: the caller's contract.
        false => unsafe { std::slice::from_raw_parts(input_buffer, input_buffer_length as usize) },
    };
    let mut status = NO_ERR;
    let mut consumed = 0_usize;
    let mut produced = 0_usize;
    while consumed < input.len() {
        let Some((ch, used)) = decode_one(&input[consumed..], converter.input) else {
            // A lead byte or unit this encoding does not have: nothing further
            // in the buffer can be trusted either, so the conversion stops
            // here rather than skip or guess at a resync point.
            break;
        };
        let bytes = encode_one(ch, converter.output);
        if produced + bytes.len() > output_buffer_length as usize {
            status = OUTPUT_BUFFER_FULL;
            break;
        }
        if !output_buffer.is_null() {
            // SAFETY: the capacity check above, and the caller's contract.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    output_buffer.add(produced),
                    bytes.len(),
                )
            };
        }
        produced += bytes.len();
        consumed += used;
    }
    if !actual_input_length.is_null() {
        // SAFETY: the caller's contract.
        unsafe { *actual_input_length = consumed as u64 };
    }
    if !actual_output_length.is_null() {
        unsafe { *actual_output_length = produced as u64 };
    }
    status
}

exports::exports! {
    functions {
        "CreateTextEncoding" => CreateTextEncoding,
        "Long2Fix" => Long2Fix,
        "UpgradeScriptInfoToTextEncoding" => UpgradeScriptInfoToTextEncoding,
        "TECGetTextEncodingFromInternetName" => TECGetTextEncodingFromInternetName,
        "TECCreateConverter" => TECCreateConverter,
        "TECDisposeConverter" => TECDisposeConverter,
        "TECClearConverterContextInfo" => TECClearConverterContextInfo,
        "TECConvertText" => TECConvertText,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three pieces land where the bits of every encoding this tree
    /// already answers put them - see `corefoundation::string::encoding`.
    #[test]
    fn create_text_encoding_matches_the_known_constants() {
        assert_eq!(create_text_encoding(0, 0, 0), encoding::MAC_ROMAN);
        assert_eq!(create_text_encoding(0x0100, 0, 8), encoding::UTF8);
        assert_eq!(create_text_encoding(0x0100, 0, 0x0c), encoding::UTF32);
    }

    #[test]
    fn long_2_fix_shifts_into_the_integer_half() {
        assert_eq!(unsafe { Long2Fix(1) }, 0x1_0000);
        assert_eq!(unsafe { Long2Fix(-1) }, -0x1_0000);
    }

    /// `smRoman` upgrades to Mac Roman, the base it is numbered the same as.
    #[test]
    fn upgrading_roman_gives_mac_roman() {
        let mut out = 0_u32;
        // SAFETY: storage for a `TextEncoding`.
        let status =
            unsafe { UpgradeScriptInfoToTextEncoding(0, 0, 0, std::ptr::null(), &mut out) };
        assert_eq!(status, NO_ERR);
        assert_eq!(out, encoding::MAC_ROMAN);
    }

    /// A script code past the Script Manager's own range is refused rather
    /// than guessed at.
    #[test]
    fn upgrading_an_unknown_script_is_refused() {
        let mut out = 0_u32;
        // SAFETY: storage for a `TextEncoding`.
        let status =
            unsafe { UpgradeScriptInfoToTextEncoding(100, 0, 0, std::ptr::null(), &mut out) };
        assert_eq!(status, PARAM_ERR);
    }

    fn pascal(text: &str) -> Vec<u8> {
        let mut out = vec![text.len() as u8];
        out.extend_from_slice(text.as_bytes());
        out
    }

    #[test]
    fn an_internet_name_resolves_to_its_encoding() {
        let name = pascal("UTF-8");
        let mut out = 0_u32;
        // SAFETY: a Pascal string this test owns, and storage for a `TextEncoding`.
        let status = unsafe { TECGetTextEncodingFromInternetName(&mut out, name.as_ptr()) };
        assert_eq!(status, NO_ERR);
        assert_eq!(out, encoding::UTF8);
    }

    #[test]
    fn an_unknown_internet_name_is_refused() {
        let name = pascal("no-such-charset");
        let mut out = 0_u32;
        // SAFETY: as above.
        let status = unsafe { TECGetTextEncodingFromInternetName(&mut out, name.as_ptr()) };
        assert_eq!(status, PARAM_ERR);
    }

    /// ASCII text round-trips unchanged between any two of this tree's
    /// encodings, UTF-8 included.
    #[test]
    fn ascii_round_trips_through_every_encoding() {
        for (input_encoding, output_encoding) in [
            (encoding::UTF8, encoding::MAC_ROMAN),
            (encoding::MAC_ROMAN, encoding::UTF8),
            (encoding::UTF8, encoding::UTF16_BE),
            (encoding::UTF16_LE, encoding::UTF8),
        ] {
            let mut tec = std::ptr::null_mut();
            // SAFETY: storage for a `TECObjectRef`.
            assert_eq!(
                unsafe { TECCreateConverter(&mut tec, input_encoding, output_encoding) },
                NO_ERR
            );
            let message = b"Hello";
            let input: Vec<u8> = match input_encoding {
                encoding::UTF16_LE => message.iter().flat_map(|b| [*b, 0]).collect(),
                _ => message.to_vec(),
            };
            let mut output = [0_u8; 64];
            let (mut used_in, mut used_out) = (0_u64, 0_u64);
            // SAFETY: a live converter, the input above, and storage sized for it.
            let status = unsafe {
                TECConvertText(
                    tec,
                    input.as_ptr(),
                    input.len() as u64,
                    &mut used_in,
                    output.as_mut_ptr(),
                    output.len() as u64,
                    &mut used_out,
                )
            };
            assert_eq!(status, NO_ERR);
            assert_eq!(used_in, input.len() as u64);
            let produced: Vec<u8> = match output_encoding {
                encoding::UTF16_BE => message.iter().flat_map(|b| [0, *b]).collect(),
                _ => message.to_vec(),
            };
            assert_eq!(&output[..used_out as usize], produced.as_slice());
            // SAFETY: the same converter, not yet disposed.
            assert_eq!(unsafe { TECDisposeConverter(tec) }, NO_ERR);
        }
    }

    /// A buffer too small to hold the whole conversion stops rather than
    /// overruns, and reports exactly how far it got.
    #[test]
    fn a_full_output_buffer_stops_cleanly() {
        let mut tec = std::ptr::null_mut();
        // SAFETY: storage for a `TECObjectRef`.
        assert_eq!(
            unsafe { TECCreateConverter(&mut tec, encoding::UTF8, encoding::UTF8) },
            NO_ERR
        );
        let input = b"Hello";
        let mut output = [0_u8; 3];
        let (mut used_in, mut used_out) = (0_u64, 0_u64);
        // SAFETY: a live converter and a buffer shorter than the input.
        let status = unsafe {
            TECConvertText(
                tec,
                input.as_ptr(),
                input.len() as u64,
                &mut used_in,
                output.as_mut_ptr(),
                output.len() as u64,
                &mut used_out,
            )
        };
        assert_eq!(status, OUTPUT_BUFFER_FULL);
        assert_eq!(used_in, 3);
        assert_eq!(used_out, 3);
        assert_eq!(&output, b"Hel");
        // SAFETY: the same converter, not yet disposed.
        assert_eq!(unsafe { TECDisposeConverter(tec) }, NO_ERR);
    }
}
