use microlift::arch::x86_64::context::RegisterContext;

use crate::abi::{Callbacks, GuestMemory, Result};

use super::coregraphics::CgPoint;
use super::host::builtin_or_host;
use crate::shims::bind;

/// The function `symbol` forwards to, as `F`.
///
/// # Safety
///
/// `F` must be the `extern "C"` function pointer type `symbol` is declared with.
unsafe fn entry<F: Copy>(symbol: &str) -> Result<F> {
    const { assert!(size_of::<F>() == size_of::<u64>()) };
    let address = builtin_or_host(symbol)?;
    // SAFETY: a function pointer is one address wide (checked above), and the
    // caller vouches for the signature.
    Ok(unsafe { core::mem::transmute_copy::<u64, F>(&address) })
}

/// `CMTime`, laid out the way both architectures lay it out.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct CmTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// Bytes long, and the reason this module exists: over sixteen, so SysV passes
/// and returns it in memory where AAPCS64 does not.
const CM_TIME_SIZE: u64 = 24;

impl CmTime {
    fn from_words([value, middle, epoch]: [u64; 3]) -> Self {
        CmTime {
            value: value as i64,
            timescale: middle as u32 as i32,
            flags: (middle >> 32) as u32,
            epoch: epoch as i64,
        }
    }

    fn to_words(self) -> [u64; 3] {
        let middle = u64::from(self.timescale as u32) | (u64::from(self.flags) << 32);
        [self.value as u64, middle, self.epoch as u64]
    }

    fn read(mem: &mut dyn GuestMemory, at: u64, symbol: &str) -> Result<Self> {
        let mut words = [0; 3];
        for (i, word) in words.iter_mut().enumerate() {
            *word = mem.read_u64(at + 8 * i as u64, symbol)?;
        }
        Ok(Self::from_words(words))
    }

    fn write(self, mem: &mut dyn GuestMemory, at: u64, symbol: &str) -> Result<()> {
        for (i, word) in self.to_words().into_iter().enumerate() {
            mem.write_u64(at + 8 * i as u64, word, symbol)?;
        }
        Ok(())
    }
}

/// The `n`th `CMTime` in the guest's stack argument area. The call has not
/// pushed a return address yet, so the area starts at `rsp`.
fn time_argument(
    symbol: &str,
    ctx: &RegisterContext,
    mem: &mut dyn GuestMemory,
    n: u64,
) -> Result<CmTime> {
    CmTime::read(mem, ctx.rsp.wrapping_add(n * CM_TIME_SIZE), symbol)
}

/// Return a `CMTime` the SysV MEMORY way: into the hidden buffer in `rdi`,
/// which `rax` hands back. Every real argument is one register along for it.
fn return_time(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    time: CmTime,
) -> Result<()> {
    let buffer = ctx.rdi;
    if buffer == 0 {
        return Err(super::bad_pointer(symbol, 0));
    }
    time.write(mem, buffer, symbol)?;
    ctx.rax = buffer;
    Ok(())
}

/// `CMTime CMTimeMake(int64_t value, int32_t timescale)`
fn time_make(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    // SAFETY: the declaration from CMTime.h.
    let f = unsafe { entry::<extern "C" fn(i64, i32) -> CmTime>(symbol)? };
    let time = f(ctx.rsi as i64, ctx.rdx as i32);
    return_time(symbol, ctx, mem, time)
}

/// `Float64 CMTimeGetSeconds(CMTime time)`
///
/// Forwarded rather than divided: the flags make it more than a division.
fn time_get_seconds(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    // SAFETY: the declaration from CMTime.h.
    let f = unsafe { entry::<extern "C" fn(CmTime) -> f64>(symbol)? };
    let time = time_argument(symbol, ctx, mem, 0)?;
    ctx.xmm[0] = [f(time).to_bits(), 0];
    Ok(())
}

/// `CMTime CMSampleBufferGetPresentationTimeStamp(CMSampleBufferRef sbuf)`
fn presentation_time_stamp(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    // SAFETY: the declaration from CMSampleBuffer.h.
    let f = unsafe { entry::<extern "C" fn(u64) -> CmTime>(symbol)? };
    let time = f(ctx.rsi);
    return_time(symbol, ctx, mem, time)
}

/// `OSStatus VTCompressionSessionCompleteFrames(VTCompressionSessionRef session,
/// CMTime completeUntilPresentationTimeStamp)`
fn complete_frames(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
) -> Result<()> {
    // SAFETY: the declaration from VTCompressionSession.h.
    let f = unsafe { entry::<extern "C" fn(u64, CmTime) -> i32>(symbol)? };
    let until = time_argument(symbol, ctx, mem, 0)?;
    super::set_int_return(ctx, f(ctx.rdi, until));
    Ok(())
}

/// `OSStatus VTCompressionSessionEncodeFrame(VTCompressionSessionRef session,
/// CVImageBufferRef imageBuffer, CMTime presentationTimeStamp, CMTime duration,
/// CFDictionaryRef frameProperties, void *sourceFrameRefcon, VTEncodeInfoFlags
/// *infoFlagsOut)`
///
/// The two times go on the stack and take no registers, so the last three
/// arguments are in `rdx`, `rcx` and `r8`.
fn encode_frame(symbol: &str, ctx: &mut RegisterContext, mem: &mut dyn GuestMemory) -> Result<()> {
    type EncodeFrame = extern "C" fn(u64, u64, CmTime, CmTime, u64, u64, u64) -> i32;
    // SAFETY: the declaration from VTCompressionSession.h. The output callback
    // was substituted when the session was created.
    let f = unsafe { entry::<EncodeFrame>(symbol)? };
    let pts = time_argument(symbol, ctx, mem, 0)?;
    let duration = time_argument(symbol, ctx, mem, 1)?;
    let status = f(ctx.rdi, ctx.rsi, pts, duration, ctx.rdx, ctx.rcx, ctx.r8);
    super::set_int_return(ctx, status);
    Ok(())
}

/// `CGSize CVImageBufferGetEncodedSize(CVImageBufferRef imageBuffer)`
fn encoded_size(symbol: &str, ctx: &mut RegisterContext, _mem: &mut dyn GuestMemory) -> Result<()> {
    // SAFETY: the declaration from CVImageBuffer.h; a `CGSize` is laid out as a
    // `CGPoint` is.
    let f = unsafe { entry::<extern "C" fn(u64) -> CgPoint>(symbol)? };
    let size = f(ctx.rdi);
    ctx.xmm[0] = [size.x.to_bits(), 0];
    ctx.xmm[1] = [size.y.to_bits(), 0];
    Ok(())
}

/// `CAFrameRateRange`: three `float`s, which SysV returns two in `xmm0` and one
/// in `xmm1`, and AAPCS64 in `s0`-`s2`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CaFrameRateRange {
    minimum: f32,
    maximum: f32,
    preferred: f32,
}

/// `CAFrameRateRange CAFrameRateRangeMake(float minimum, float maximum, float
/// preferred)`
fn frame_rate_range_make(
    symbol: &str,
    ctx: &mut RegisterContext,
    _mem: &mut dyn GuestMemory,
) -> Result<()> {
    // SAFETY: the declaration from CAFrameRateRange.h.
    let f = unsafe { entry::<extern "C" fn(f32, f32, f32) -> CaFrameRateRange>(symbol)? };
    let arg = |i: usize| f32::from_bits(ctx.xmm[i][0] as u32);
    let range = f(arg(0), arg(1), arg(2));
    let bits = |v: f32| u64::from(v.to_bits());
    ctx.xmm[0] = [bits(range.minimum) | (bits(range.maximum) << 32), 0];
    ctx.xmm[1] = [bits(range.preferred), 0];
    Ok(())
}

/// `VTDecompressionOutputCallbackRecord` - a function pointer and its refcon.
#[repr(C)]
struct OutputCallbackRecord {
    callback: u64,
    refcon: u64,
}

/// `OSStatus VTDecompressionSessionCreate(CFAllocatorRef allocator,
/// CMVideoFormatDescriptionRef videoFormatDescription, CFDictionaryRef
/// videoDecoderSpecification, CFDictionaryRef destinationImageBufferAttributes, const
/// VTDecompressionOutputCallbackRecord *outputCallback, VTDecompressionSessionRef
/// *decompressionSessionOut)`
///
/// The callback is inside a record, which per-argument substitution cannot see
/// into, so the host gets a local copy of the record with its callback thunked.
fn decompression_session_create(
    symbol: &str,
    ctx: &mut RegisterContext,
    mem: &mut dyn GuestMemory,
    callbacks: &dyn Callbacks,
) -> Result<()> {
    type SessionCreate = extern "C" fn(u64, u64, u64, u64, *const OutputCallbackRecord, u64) -> i32;
    // SAFETY: the declaration from VTDecompressionSession.h.
    let f = unsafe { entry::<SessionCreate>(symbol)? };
    let record = match ctx.r8 {
        0 => None,
        va => Some(OutputCallbackRecord {
            callback: match mem.read_u64(va, symbol)? {
                0 => 0,
                guest => callbacks.substitute(symbol, guest)?,
            },
            refcon: mem.read_u64(va + 8, symbol)?,
        }),
    };
    // VideoToolbox copies the record during the call, so a local one suffices.
    let record_ptr = record.as_ref().map_or(core::ptr::null(), |r| r as *const _);
    let status = f(ctx.rdi, ctx.rsi, ctx.rdx, ctx.rcx, record_ptr, ctx.r9);
    super::set_int_return(ctx, status);
    Ok(())
}

/// `kCMTimeFlags_Valid`, from `CMTime.h`.
const FLAGS_VALID: u32 = 1 << 0;
/// `kCMTimeFlags_PositiveInfinity`.
const FLAGS_POSITIVE_INFINITY: u32 = 1 << 2;

/// `kCMTimeZero`: zero numeric seconds. Any nonzero `timescale` gives the same
/// answer through `CMTimeGetSeconds`/`CMTimeCompare`, the only way the
/// documented API observes this value - the stored `timescale` is not itself
/// part of what "time zero" promises, so this is a real value rather than an
/// approximation of one.
static K_CM_TIME_ZERO: CmTime = CmTime {
    value: 0,
    timescale: 1,
    flags: FLAGS_VALID,
    epoch: 0,
};

/// `kCMTimeInvalid`: "all of its fields set to 0", per `CMTime.h`'s own
/// doc comment on the constant.
static K_CM_TIME_INVALID: CmTime = CmTime {
    value: 0,
    timescale: 0,
    flags: 0,
    epoch: 0,
};

/// `kCMTimePositiveInfinity`. `CMTIME_IS_POSITIVE_INFINITY` only looks at
/// `flags`, so `value`/`timescale`/`epoch` are unobserved and left at 0.
static K_CM_TIME_POSITIVE_INFINITY: CmTime = CmTime {
    value: 0,
    timescale: 0,
    flags: FLAGS_VALID | FLAGS_POSITIVE_INFINITY,
    epoch: 0,
};

/// The three `CMTime` globals a guest's static initializers read directly out
/// of its GOT - not through a shim call, since these are data, not functions.
/// Each symbol's address *is* the struct's bytes, the way `_CGRectInfinite` is
/// in `rosemary::frameworks`, not a pointer to them.
pub(crate) fn data_symbols() -> Vec<(String, u64)> {
    vec![
        (
            "_kCMTimeZero".to_string(),
            &K_CM_TIME_ZERO as *const CmTime as usize as u64,
        ),
        (
            "_kCMTimeInvalid".to_string(),
            &K_CM_TIME_INVALID as *const CmTime as usize as u64,
        ),
        (
            "_kCMTimePositiveInfinity".to_string(),
            &K_CM_TIME_POSITIVE_INFINITY as *const CmTime as usize as u64,
        ),
    ]
}

/// One of this module's implementations that needs no [`Callbacks`].
pub type Shim = super::coregraphics::Shim;

/// Whether `name` (underscore already stripped) is one of this module's plain
/// shims.
pub fn shim_for(name: &str) -> Option<Shim> {
    Some(match name {
        "CMTimeMake" => bind!("CMTimeMake" => time_make),
        "CMTimeGetSeconds" => bind!("CMTimeGetSeconds" => time_get_seconds),
        "CMSampleBufferGetPresentationTimeStamp" => {
            bind!("CMSampleBufferGetPresentationTimeStamp" => presentation_time_stamp)
        }
        "VTCompressionSessionCompleteFrames" => {
            bind!("VTCompressionSessionCompleteFrames" => complete_frames)
        }
        "VTCompressionSessionEncodeFrame" => {
            bind!("VTCompressionSessionEncodeFrame" => encode_frame)
        }
        "CVImageBufferGetEncodedSize" => bind!("CVImageBufferGetEncodedSize" => encoded_size),
        "CAFrameRateRangeMake" => bind!("CAFrameRateRangeMake" => frame_rate_range_make),
        _ => return None,
    })
}

/// Whether `name` is this module's one callback-carrying shim.
pub fn callback_shim_for(name: &str) -> Option<crate::signal::Shim> {
    match name {
        "VTDecompressionSessionCreate" => {
            Some(bind!(callback "VTDecompressionSessionCreate" => decompression_session_create))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout claim every function here rests on, which a wrong answer would
    /// violate silently.
    #[test]
    fn cm_time_is_the_layout_both_architectures_use() {
        assert_eq!(size_of::<CmTime>() as u64, CM_TIME_SIZE);
        let time = CmTime {
            value: 0x0102_0304_0506_0708,
            timescale: 600,
            flags: 1,
            epoch: -1,
        };
        let words = time.to_words();
        assert_eq!(words, [0x0102_0304_0506_0708, (1 << 32) | 600, u64::MAX]);
        assert_eq!(CmTime::from_words(words), time);
    }

    /// Twelve bytes and three members, which is what makes the SysV return
    /// packing - two members in `xmm0`, one in `xmm1` - the right arrangement.
    #[test]
    fn a_frame_rate_range_is_three_floats() {
        assert_eq!(size_of::<CaFrameRateRange>(), 12);
    }

    /// Each of `kCMTimeZero`/`kCMTimeInvalid`/`kCMTimePositiveInfinity` reads
    /// back as the `CMTime` its doc comment describes, at the address a guest's
    /// GOT slot for it resolves to.
    #[test]
    fn the_cm_time_globals_match_their_documented_fields() {
        let symbols = data_symbols();
        let read = |name: &str| {
            let (_, address) = symbols.iter().find(|(n, _)| n == name).expect(name);
            // SAFETY: the address of a `static CmTime` this module owns.
            unsafe { *(*address as *const CmTime) }
        };

        let zero = read("_kCMTimeZero");
        assert_eq!(zero.value, 0);
        assert_ne!(zero.timescale, 0, "a numeric time needs a nonzero scale");
        assert_eq!(zero.flags & 1, 1, "kCMTimeFlags_Valid must be set");

        let invalid = read("_kCMTimeInvalid");
        assert_eq!(invalid, CmTime::default(), "an invalid time is all zero");

        let positive_infinity = read("_kCMTimePositiveInfinity");
        assert_eq!(positive_infinity.flags & 1, 1, "still Valid");
        assert_eq!(
            positive_infinity.flags & (1 << 2),
            1 << 2,
            "and PositiveInfinity"
        );
    }
}
