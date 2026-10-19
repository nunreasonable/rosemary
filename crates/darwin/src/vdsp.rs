//! The part of Accelerate's vDSP that Chromium's WebAudio calls: vector
//! arithmetic, the packed real FFT, convolution and the biquad.
//!
//! Strides are counted in elements, as vDSP's are, and may be negative where
//! vDSP allows it. The FFT keeps vDSP's conventions exactly, because callers
//! scale around them: a forward real FFT returns twice the DFT, packed with
//! the DC term in `realp[0]` and the Nyquist term in `imagp[0]`, and the
//! inverse is the unnormalized inverse DFT of such a spectrum.

#![allow(non_snake_case)]

use std::f64::consts::PI;

/// `DSPSplitComplex`.
#[repr(C)]
pub struct SplitComplex {
    realp: *mut f32,
    imagp: *mut f32,
}

/// The element `i` steps of `stride` from `base`.
///
/// # Safety
///
/// That element lies inside the caller's vector.
unsafe fn at<T>(base: *const T, stride: isize, i: usize) -> *const T {
    // SAFETY: per the caller.
    unsafe { base.offset(stride * i as isize) }
}

unsafe fn at_mut<T>(base: *mut T, stride: isize, i: usize) -> *mut T {
    // SAFETY: per the caller.
    unsafe { base.offset(stride * i as isize) }
}

/// `C = A + B`.
///
/// # Safety
///
/// Each vector holds `n` elements at its stride.
pub unsafe extern "C" fn vDSP_vadd(
    a: *const f32,
    ia: isize,
    b: *const f32,
    ib: isize,
    c: *mut f32,
    ic: isize,
    n: usize,
) {
    for i in 0..n {
        // SAFETY: inside each vector, per the caller.
        unsafe { *at_mut(c, ic, i) = *at(a, ia, i) + *at(b, ib, i) };
    }
}

/// `C = A - B`, with `B` first in the argument list as vDSP has it.
///
/// # Safety
///
/// As [`vDSP_vadd`].
pub unsafe extern "C" fn vDSP_vsub(
    b: *const f32,
    ib: isize,
    a: *const f32,
    ia: isize,
    c: *mut f32,
    ic: isize,
    n: usize,
) {
    for i in 0..n {
        // SAFETY: as above.
        unsafe { *at_mut(c, ic, i) = *at(a, ia, i) - *at(b, ib, i) };
    }
}

/// `C = A * B`.
///
/// # Safety
///
/// As [`vDSP_vadd`].
pub unsafe extern "C" fn vDSP_vmul(
    a: *const f32,
    ia: isize,
    b: *const f32,
    ib: isize,
    c: *mut f32,
    ic: isize,
    n: usize,
) {
    for i in 0..n {
        // SAFETY: as above.
        unsafe { *at_mut(c, ic, i) = *at(a, ia, i) * *at(b, ib, i) };
    }
}

/// `C = A * b`.
///
/// # Safety
///
/// As [`vDSP_vadd`]; `b` is readable.
pub unsafe extern "C" fn vDSP_vsmul(
    a: *const f32,
    ia: isize,
    b: *const f32,
    c: *mut f32,
    ic: isize,
    n: usize,
) {
    // SAFETY: readable, per the caller.
    let b = unsafe { *b };
    for i in 0..n {
        // SAFETY: as above.
        unsafe { *at_mut(c, ic, i) = *at(a, ia, i) * b };
    }
}

/// `C = A + b`.
///
/// # Safety
///
/// As [`vDSP_vsmul`].
pub unsafe extern "C" fn vDSP_vsadd(
    a: *const f32,
    ia: isize,
    b: *const f32,
    c: *mut f32,
    ic: isize,
    n: usize,
) {
    // SAFETY: readable, per the caller.
    let b = unsafe { *b };
    for i in 0..n {
        // SAFETY: as above.
        unsafe { *at_mut(c, ic, i) = *at(a, ia, i) + b };
    }
}

/// `D = A * b + C`.
///
/// # Safety
///
/// As [`vDSP_vsmul`].
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn vDSP_vsma(
    a: *const f32,
    ia: isize,
    b: *const f32,
    c: *const f32,
    ic: isize,
    d: *mut f32,
    id: isize,
    n: usize,
) {
    // SAFETY: readable, per the caller.
    let b = unsafe { *b };
    for i in 0..n {
        // SAFETY: as above.
        unsafe { *at_mut(d, id, i) = *at(a, ia, i) * b + *at(c, ic, i) };
    }
}

/// `D = clamp(A, low, high)`.
///
/// # Safety
///
/// As [`vDSP_vsmul`]; `low` and `high` are readable.
pub unsafe extern "C" fn vDSP_vclip(
    a: *const f32,
    ia: isize,
    low: *const f32,
    high: *const f32,
    d: *mut f32,
    id: isize,
    n: usize,
) {
    // SAFETY: readable, per the caller.
    let (low, high) = unsafe { (*low, *high) };
    for i in 0..n {
        // SAFETY: as above.
        unsafe {
            let x = *at(a, ia, i);
            *at_mut(d, id, i) = if x < low {
                low
            } else if x > high {
                high
            } else {
                x
            };
        }
    }
}

/// `*C = max |A|`.
///
/// # Safety
///
/// `a` holds `n` elements at its stride; `c` is writable.
pub unsafe extern "C" fn vDSP_maxmgv(a: *const f32, ia: isize, c: *mut f32, n: usize) {
    let mut max = 0.0f32;
    for i in 0..n {
        // SAFETY: as above.
        max = max.max(unsafe { *at(a, ia, i) }.abs());
    }
    // SAFETY: writable, per the caller.
    unsafe { *c = max };
}

/// `*C = sum A^2`.
///
/// # Safety
///
/// As [`vDSP_maxmgv`].
pub unsafe extern "C" fn vDSP_svesq(a: *const f32, ia: isize, c: *mut f32, n: usize) {
    let mut sum = 0.0f32;
    for i in 0..n {
        // SAFETY: as above.
        let x = unsafe { *at(a, ia, i) };
        sum += x * x;
    }
    // SAFETY: writable, per the caller.
    unsafe { *c = sum };
}

/// `C = A * B` over split complex vectors, with `A` conjugated when
/// `conjugate` is -1.
///
/// # Safety
///
/// Each split vector's halves hold `n` elements at its stride.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn vDSP_zvmul(
    a: *const SplitComplex,
    ia: isize,
    b: *const SplitComplex,
    ib: isize,
    c: *const SplitComplex,
    ic: isize,
    n: usize,
    conjugate: i32,
) {
    // SAFETY: three split complex headers, per the caller.
    let (a, b, c) = unsafe { (&*a, &*b, &*c) };
    let sign = if conjugate == -1 { -1.0 } else { 1.0 };
    for i in 0..n {
        // SAFETY: inside each vector, per the caller.
        unsafe {
            let (ar, ai) = (*at(a.realp, ia, i), sign * *at(a.imagp, ia, i));
            let (br, bi) = (*at(b.realp, ib, i), *at(b.imagp, ib, i));
            *at_mut(c.realp, ic, i) = ar * br - ai * bi;
            *at_mut(c.imagp, ic, i) = ar * bi + ai * br;
        }
    }
}

/// Interleaved complex to split: `ic` counts floats and is 2 for a packed
/// array.
///
/// # Safety
///
/// `c` holds `n` pairs at its stride and `z`'s halves `n` elements at theirs.
pub unsafe extern "C" fn vDSP_ctoz(
    c: *const f32,
    ic: isize,
    z: *const SplitComplex,
    iz: isize,
    n: usize,
) {
    // SAFETY: a split complex header, per the caller.
    let z = unsafe { &*z };
    for i in 0..n {
        // SAFETY: inside both, per the caller.
        unsafe {
            let pair = at(c, ic, i);
            *at_mut(z.realp, iz, i) = *pair;
            *at_mut(z.imagp, iz, i) = *pair.add(1);
        }
    }
}

/// Split complex to interleaved; the inverse of [`vDSP_ctoz`].
///
/// # Safety
///
/// As [`vDSP_ctoz`].
pub unsafe extern "C" fn vDSP_ztoc(
    z: *const SplitComplex,
    iz: isize,
    c: *mut f32,
    ic: isize,
    n: usize,
) {
    // SAFETY: a split complex header, per the caller.
    let z = unsafe { &*z };
    for i in 0..n {
        // SAFETY: inside both, per the caller.
        unsafe {
            let pair = at_mut(c, ic, i);
            *pair = *at(z.realp, iz, i);
            *pair.add(1) = *at(z.imagp, iz, i);
        }
    }
}

/// `C[n] = sum A[n + p] * F[p * IF]` for `n < N`, `p < P`: correlation, or
/// convolution when `IF` is negative and `F` points at the filter's last
/// element.
///
/// # Safety
///
/// `a` holds `N + P - 1` elements, `f` `P` and `c` `N`, at their strides.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn vDSP_conv(
    a: *const f32,
    ia: isize,
    f: *const f32,
    if_: isize,
    c: *mut f32,
    ic: isize,
    n: usize,
    p: usize,
) {
    for i in 0..n {
        let mut sum = 0.0f32;
        for k in 0..p {
            // SAFETY: inside each vector, per the caller.
            sum += unsafe { *at(a, ia, i + k) * *at(f, if_, k) };
        }
        // SAFETY: as above.
        unsafe { *at_mut(c, ic, i) = sum };
    }
}

/// The two-pole, two-zero filter in double precision: for `n` from 2 to
/// `N + 1`, `C[n] = A[n]B0 + A[n-1]B1 + A[n-2]B2 - C[n-1]B3 - C[n-2]B4`. The
/// first two elements of `A` and `C` are the history the caller carries.
///
/// # Safety
///
/// `a` and `c` hold `N + 2` elements at their strides, `b` five.
pub unsafe extern "C" fn vDSP_deq22D(
    a: *const f64,
    ia: isize,
    b: *const f64,
    c: *mut f64,
    ic: isize,
    n: usize,
) {
    // SAFETY: five coefficients, per the caller.
    let b = unsafe { std::slice::from_raw_parts(b, 5) };
    for i in 2..n + 2 {
        // SAFETY: inside each vector, per the caller.
        unsafe {
            *at_mut(c, ic, i) =
                *at(a, ia, i) * b[0] + *at(a, ia, i - 1) * b[1] + *at(a, ia, i - 2) * b[2]
                    - *at(c, ic, i - 1) * b[3]
                    - *at(c, ic, i - 2) * b[4];
        }
    }
}

/// An `FFTSetup`: the largest size it serves.
struct Setup {
    log2n: u32,
}

/// `FFTSetup vDSP_create_fftsetup(vDSP_Length log2n, FFTRadix)`.
pub extern "C" fn vDSP_create_fftsetup(log2n: usize, _radix: i32) -> *mut std::ffi::c_void {
    if log2n > 24 {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(Setup {
        log2n: log2n as u32,
    }))
    .cast()
}

/// `void vDSP_destroy_fftsetup(FFTSetup)`.
///
/// # Safety
///
/// `setup` came from [`vDSP_create_fftsetup`] and is not used again.
pub unsafe extern "C" fn vDSP_destroy_fftsetup(setup: *mut std::ffi::c_void) {
    if !setup.is_null() {
        // SAFETY: a box this module made.
        drop(unsafe { Box::from_raw(setup.cast::<Setup>()) });
    }
}

/// An in-place radix-2 complex FFT of `re`/`im`, forward for `sign` -1 and an
/// unnormalized inverse for +1.
fn fft(re: &mut [f64], im: &mut [f64], sign: f64) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let angle = sign * 2.0 * PI / len as f64;
        let (step_re, step_im) = (angle.cos(), angle.sin());
        for start in (0..n).step_by(len) {
            let (mut w_re, mut w_im) = (1.0, 0.0);
            for k in 0..len / 2 {
                let (u, v) = (start + k, start + k + len / 2);
                let (t_re, t_im) = (re[v] * w_re - im[v] * w_im, re[v] * w_im + im[v] * w_re);
                re[v] = re[u] - t_re;
                im[v] = im[u] - t_im;
                re[u] += t_re;
                im[u] += t_im;
                (w_re, w_im) = (
                    w_re * step_re - w_im * step_im,
                    w_re * step_im + w_im * step_re,
                );
            }
        }
        len <<= 1;
    }
}

/// `void vDSP_fft_zrip(FFTSetup, const DSPSplitComplex *, vDSP_Stride,
/// vDSP_Length log2n, FFTDirection)`: the real FFT of `2^log2n` samples
/// stored as `realp[k] = x[2k]`, `imagp[k] = x[2k + 1]`, in place.
///
/// FIXME (ACCURACY): computed in double precision through a full complex
/// transform, so results differ from Apple's in the last bits.
///
/// # Safety
///
/// `c`'s halves hold `2^(log2n - 1)` elements at stride `ic`.
pub unsafe extern "C" fn vDSP_fft_zrip(
    setup: *mut std::ffi::c_void,
    c: *const SplitComplex,
    ic: isize,
    log2n: usize,
    direction: i32,
) {
    // SAFETY: a setup this module made, or null.
    if setup.is_null() || log2n == 0 || log2n as u32 > unsafe { (*setup.cast::<Setup>()).log2n } {
        return;
    }
    // SAFETY: a split complex header, per the caller.
    let c = unsafe { &*c };
    let n = 1usize << log2n;
    let half = n / 2;
    let (mut re, mut im) = (vec![0.0f64; n], vec![0.0f64; n]);
    match direction {
        // Forward: unpack the samples, transform, pack twice the spectrum.
        1 => {
            for k in 0..half {
                // SAFETY: inside the vector, per the caller.
                unsafe {
                    re[2 * k] = f64::from(*at(c.realp, ic, k));
                    re[2 * k + 1] = f64::from(*at(c.imagp, ic, k));
                }
            }
            fft(&mut re, &mut im, -1.0);
            // SAFETY: as above.
            unsafe {
                *at_mut(c.realp, ic, 0) = (2.0 * re[0]) as f32;
                *at_mut(c.imagp, ic, 0) = (2.0 * re[half]) as f32;
                for k in 1..half {
                    *at_mut(c.realp, ic, k) = (2.0 * re[k]) as f32;
                    *at_mut(c.imagp, ic, k) = (2.0 * im[k]) as f32;
                }
            }
        }
        // Inverse: rebuild the Hermitian spectrum, transform, pack samples.
        _ => {
            // SAFETY: as above.
            unsafe {
                re[0] = f64::from(*at(c.realp, ic, 0));
                re[half] = f64::from(*at(c.imagp, ic, 0));
                for k in 1..half {
                    let (r, i) = (
                        f64::from(*at(c.realp, ic, k)),
                        f64::from(*at(c.imagp, ic, k)),
                    );
                    re[k] = r;
                    im[k] = i;
                    re[n - k] = r;
                    im[n - k] = -i;
                }
            }
            fft(&mut re, &mut im, 1.0);
            for k in 0..half {
                // SAFETY: as above.
                unsafe {
                    *at_mut(c.realp, ic, k) = re[2 * k] as f32;
                    *at_mut(c.imagp, ic, k) = re[2 * k + 1] as f32;
                }
            }
        }
    }
}

exports::exports! {
    functions {
        "vDSP_vadd" => vDSP_vadd,
        "vDSP_vsub" => vDSP_vsub,
        "vDSP_vmul" => vDSP_vmul,
        "vDSP_vsmul" => vDSP_vsmul,
        "vDSP_vsadd" => vDSP_vsadd,
        "vDSP_vsma" => vDSP_vsma,
        "vDSP_vclip" => vDSP_vclip,
        "vDSP_maxmgv" => vDSP_maxmgv,
        "vDSP_svesq" => vDSP_svesq,
        "vDSP_zvmul" => vDSP_zvmul,
        "vDSP_ctoz" => vDSP_ctoz,
        "vDSP_ztoc" => vDSP_ztoc,
        "vDSP_conv" => vDSP_conv,
        "vDSP_deq22D" => vDSP_deq22D,
        "vDSP_create_fftsetup" => vDSP_create_fftsetup,
        "vDSP_destroy_fftsetup" => vDSP_destroy_fftsetup,
        "vDSP_fft_zrip" => vDSP_fft_zrip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A forward real FFT followed by the inverse gives the input back times
    /// `2N`, the factor Chromium's FFTFrame divides out.
    #[test]
    fn a_round_trip_scales_by_twice_the_length() {
        let log2n = 5;
        let n = 1 << log2n;
        let samples: Vec<f32> = (0..n).map(|i| ((i * 7) % 11) as f32 - 5.0).collect();
        let (mut realp, mut imagp) = (vec![0.0f32; n / 2], vec![0.0f32; n / 2]);
        let split = SplitComplex {
            realp: realp.as_mut_ptr(),
            imagp: imagp.as_mut_ptr(),
        };
        let setup = vDSP_create_fftsetup(log2n, 0);
        // SAFETY: buffers of the sizes the calls take.
        unsafe {
            vDSP_ctoz(samples.as_ptr(), 2, &split, 1, n / 2);
            vDSP_fft_zrip(setup, &split, 1, log2n, 1);
            vDSP_fft_zrip(setup, &split, 1, log2n, -1);
            let mut back = vec![0.0f32; n];
            vDSP_ztoc(&split, 1, back.as_mut_ptr(), 2, n / 2);
            for (x, y) in samples.iter().zip(&back) {
                assert!((x * 2.0 * n as f32 - y).abs() < 1e-3, "{x} {y}");
            }
            vDSP_destroy_fftsetup(setup);
        }
    }

    /// The packed spectrum of a cosine at bin 3 has its energy there, twice
    /// the DFT's `N / 2`.
    #[test]
    fn a_cosine_lands_in_its_bin() {
        let log2n = 4;
        let n = 1 << log2n;
        let samples: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 3.0 * i as f32 / n as f32).cos())
            .collect();
        let (mut realp, mut imagp) = (vec![0.0f32; n / 2], vec![0.0f32; n / 2]);
        let split = SplitComplex {
            realp: realp.as_mut_ptr(),
            imagp: imagp.as_mut_ptr(),
        };
        let setup = vDSP_create_fftsetup(log2n, 0);
        // SAFETY: as above.
        unsafe {
            vDSP_ctoz(samples.as_ptr(), 2, &split, 1, n / 2);
            vDSP_fft_zrip(setup, &split, 1, log2n, 1);
            vDSP_destroy_fftsetup(setup);
        }
        assert!((realp[3] - n as f32).abs() < 1e-3);
        assert!(realp[1].abs() < 1e-3 && realp[0].abs() < 1e-3);
    }

    #[test]
    fn the_biquad_passes_a_unit_filter_through() {
        let a = [0.0f64, 0.0, 1.0, 2.0, 3.0];
        let mut c = [0.0f64; 5];
        let b = [1.0f64, 0.0, 0.0, 0.0, 0.0];
        // SAFETY: arrays of N + 2 and five elements.
        unsafe { vDSP_deq22D(a.as_ptr(), 1, b.as_ptr(), c.as_mut_ptr(), 1, 3) };
        assert_eq!(&c[2..], &[1.0, 2.0, 3.0]);
    }
}
