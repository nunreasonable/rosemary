//! `CMTime`: a rational number of seconds with flags for the values that are
//! not numbers.
//!
//! On x86-64 a `CMTime` is twenty-four bytes, which SysV classes as MEMORY:
//! returned through a hidden pointer in `rdi` and passed by value on the
//! stack. A `#[repr(C)]` struct in an `extern "C"` signature lowers the same
//! way, so these take and return [`Time`] directly.

use crate::Time;

/// `CMTimeFlags`.
pub const FLAG_VALID: u32 = 1;
pub const FLAG_HAS_BEEN_ROUNDED: u32 = 2;
pub const FLAG_POSITIVE_INFINITY: u32 = 4;
pub const FLAG_NEGATIVE_INFINITY: u32 = 8;
pub const FLAG_INDEFINITE: u32 = 16;

/// `kCMTimeInvalid`, as a value.
pub const INVALID: Time = Time {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

/// `CMTime CMTimeMake(int64_t value, int32_t timescale)`.
///
/// FIXME (ACCURACY): a timescale that is not positive gives `kCMTimeInvalid`;
/// Apple's answer for one is not verified.
pub extern "C" fn CMTimeMake(value: i64, timescale: i32) -> Time {
    if timescale <= 0 {
        return INVALID;
    }
    Time {
        value,
        timescale,
        flags: FLAG_VALID,
        epoch: 0,
    }
}

/// `Float64 CMTimeGetSeconds(CMTime)`: NaN for an invalid or indefinite time,
/// and the matching infinity for the two infinite ones.
pub extern "C" fn CMTimeGetSeconds(time: Time) -> f64 {
    if time.flags & FLAG_VALID == 0 || time.flags & FLAG_INDEFINITE != 0 {
        return f64::NAN;
    }
    if time.flags & FLAG_POSITIVE_INFINITY != 0 {
        return f64::INFINITY;
    }
    if time.flags & FLAG_NEGATIVE_INFINITY != 0 {
        return f64::NEG_INFINITY;
    }
    if time.timescale == 0 {
        return f64::NAN;
    }
    time.value as f64 / f64::from(time.timescale)
}

exports::exports! {
    functions {
        "CMTimeMake" => CMTimeMake,
        "CMTimeGetSeconds" => CMTimeGetSeconds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::ffi::c_void;
    use std::sync::Mutex;

    #[test]
    fn cm_time_make_is_valid_with_epoch_zero() {
        let time = CMTimeMake(1_500_000_000, 1_000_000_000);
        assert_eq!(
            time,
            Time {
                value: 1_500_000_000,
                timescale: 1_000_000_000,
                flags: FLAG_VALID,
                epoch: 0,
            }
        );
        assert_eq!(CMTimeGetSeconds(time), 1.5);
        assert_eq!(CMTimeMake(5, 0), INVALID);
    }

    #[test]
    fn seconds_of_invalid_and_indefinite_are_nan() {
        assert!(CMTimeGetSeconds(INVALID).is_nan());
        let indefinite = Time {
            flags: FLAG_VALID | FLAG_INDEFINITE,
            ..CMTimeMake(1, 1)
        };
        assert!(CMTimeGetSeconds(indefinite).is_nan());
    }

    #[test]
    fn seconds_of_infinities_are_infinite() {
        let positive = Time {
            flags: FLAG_VALID | FLAG_POSITIVE_INFINITY,
            ..INVALID
        };
        let negative = Time {
            flags: FLAG_VALID | FLAG_NEGATIVE_INFINITY,
            ..INVALID
        };
        assert_eq!(CMTimeGetSeconds(positive), f64::INFINITY);
        assert_eq!(CMTimeGetSeconds(negative), f64::NEG_INFINITY);
    }

    /// What the output callback saw, for the test below.
    type Seen = (usize, usize, i32, u32, usize, Time, Time);
    static SEEN: Mutex<Option<Seen>> = Mutex::new(None);

    unsafe extern "C" fn output(
        refcon: *mut c_void,
        source: *mut c_void,
        status: i32,
        flags: u32,
        image: *const c_void,
        pts: Time,
        duration: Time,
    ) {
        *SEEN.lock().unwrap() = Some((
            refcon as usize,
            source as usize,
            status,
            flags,
            image as usize,
            pts,
            duration,
        ));
    }

    /// A guest calls these through a pointer, so the by-value layout has to
    /// survive an indirect call, and VideoToolbox's output callback takes two
    /// of them on the stack after five scalars.
    #[test]
    fn cm_time_is_24_bytes_and_crosses_an_extern_c_call_by_value() {
        assert_eq!(size_of::<Time>(), 24);
        assert_eq!(std::mem::offset_of!(Time, timescale), 8);
        assert_eq!(std::mem::offset_of!(Time, flags), 12);
        assert_eq!(std::mem::offset_of!(Time, epoch), 16);

        let make: extern "C" fn(i64, i32) -> Time = std::hint::black_box(CMTimeMake);
        let seconds: extern "C" fn(Time) -> f64 = std::hint::black_box(CMTimeGetSeconds);
        let time = make(-7, 2);
        assert_eq!(
            (time.value, time.timescale, time.flags),
            (-7, 2, FLAG_VALID)
        );
        assert_eq!(seconds(time), -3.5);

        type Callback =
            unsafe extern "C" fn(*mut c_void, *mut c_void, i32, u32, *const c_void, Time, Time);
        let callback: Callback = std::hint::black_box(output);
        let pts = make(33_333_333, 1_000_000_000);
        let duration = Time {
            epoch: 9,
            ..make(1, 30)
        };
        // SAFETY: `output` has exactly this prototype and touches no pointer.
        unsafe {
            callback(
                0x11 as *mut c_void,
                0x22 as *mut c_void,
                -12909,
                2,
                0x33 as *const c_void,
                pts,
                duration,
            )
        };
        assert_eq!(
            *SEEN.lock().unwrap(),
            Some((0x11, 0x22, -12909, 2, 0x33, pts, duration))
        );
    }
}
