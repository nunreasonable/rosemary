use std::ffi::{VaList, c_char, c_int};

unsafe extern "C" {
    fn vsnprintf(buffer: *mut c_char, size: usize, format: *const c_char, ap: VaList) -> c_int;
    fn vsscanf(input: *const c_char, format: *const c_char, ap: VaList) -> c_int;
    fn vasprintf(out: *mut *mut c_char, format: *const c_char, ap: VaList) -> c_int;
}

/// `int snprintf_l(char *, size_t, locale_t, const char *, ...)`
///
/// # Safety
///
/// `buffer` holds `size` bytes, `locale` is null or a `locale_t` this process
/// made, and the arguments match `format`.
unsafe extern "C" fn snprintf_l(
    buffer: *mut c_char,
    size: usize,
    locale: libc::locale_t,
    format: *const c_char,
    args: ...
) -> c_int {
    let _switched = crate::xlocale::switch(locale);
    // SAFETY: the caller's buffer, format and arguments.
    unsafe { vsnprintf(buffer, size, format, args) }
}

/// `int vsnprintf_l(char *, size_t, locale_t, const char *, va_list)`
///
/// # Safety
///
/// As [`snprintf_l`], with the arguments already gathered in `args`.
unsafe extern "C" fn vsnprintf_l(
    buffer: *mut c_char,
    size: usize,
    locale: libc::locale_t,
    format: *const c_char,
    args: VaList,
) -> c_int {
    let _switched = crate::xlocale::switch(locale);
    // SAFETY: the caller's buffer, format and argument list.
    unsafe { vsnprintf(buffer, size, format, args) }
}

/// `int sscanf_l(const char *, locale_t, const char *, ...)`
///
/// # Safety
///
/// As [`snprintf_l`].
unsafe extern "C" fn sscanf_l(
    input: *const c_char,
    locale: libc::locale_t,
    format: *const c_char,
    args: ...
) -> c_int {
    let _switched = crate::xlocale::switch(locale);
    unsafe { vsscanf(input, format, args) }
}

/// `int asprintf_l(char **, locale_t, const char *, ...)`
///
/// The result is **moved into the guest's allocator** before it is handed over.
/// `vasprintf` allocates with this host's `malloc` and the caller frees it with
/// whichever `free` its own image binds - which is the crossing
/// [`crate::heap`] describes, and the one that does not fail at the call: the
/// guest's allocator accepts the foreign pointer, decides which of its pages
/// owns it from the pointer's own bits, and pushes a live block onto that
/// page's free list.
///
/// # Safety
///
/// As [`snprintf_l`].
unsafe extern "C" fn asprintf_l(
    out: *mut *mut c_char,
    locale: libc::locale_t,
    format: *const c_char,
    args: ...
) -> c_int {
    if out.is_null() {
        return -1;
    }
    let mut host: *mut c_char = std::ptr::null_mut();
    let written = {
        let _switched = crate::xlocale::switch(locale);
        // SAFETY: a live `char *` slot, and the caller's format and arguments.
        unsafe { vasprintf(&raw mut host, format, args) }
    };
    if written < 0 || host.is_null() {
        return -1;
    }
    // SAFETY: `vasprintf` NUL-terminates, so the block is `written + 1` bytes.
    let moved = unsafe { crate::heap::malloc(written as usize + 1) }.cast::<c_char>();
    if !moved.is_null() {
        // SAFETY: both blocks hold `written + 1` bytes and do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(host, moved, written as usize + 1) };
    }
    // SAFETY: this host's `malloc` allocated it, inside `vasprintf`.
    unsafe { libc::free(host.cast()) };
    if moved.is_null() {
        return -1;
    }
    // SAFETY: the caller states `out` is writable.
    unsafe { *out = moved };
    written
}

exports::exports! {
    functions {
        "snprintf_l" => snprintf_l,
        "vsnprintf_l" => vsnprintf_l,
        "sscanf_l" => sscanf_l,
        "asprintf_l" => asprintf_l,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// Called through the published symbol, as a guest would reach it.
    fn published_snprintf_l()
    -> unsafe extern "C" fn(*mut c_char, usize, libc::locale_t, *const c_char, ...) -> c_int {
        // SAFETY: the export is a variadic function of this shape.
        unsafe { std::mem::transmute(symbol("snprintf_l").expect("published")) }
    }

    /// Six integer arguments and one past them: the seventh is on the caller's
    /// stack, and reading it is the whole reason this is not a register shuffle.
    #[test]
    fn an_argument_past_the_registers_is_still_read() {
        let mut buffer = [0i8; 64];
        // SAFETY: a buffer this test sized, and a format matching the
        // arguments after it.
        let written = unsafe {
            published_snprintf_l()(
                buffer.as_mut_ptr(),
                buffer.len(),
                std::ptr::null_mut(),
                c"%d %d %d %d %d %d %d".as_ptr(),
                1,
                2,
                3,
                4,
                5,
                6,
                7,
            )
        };
        assert_eq!(written, 13);
        // SAFETY: `vsnprintf` terminated it.
        let text = unsafe { CStr::from_ptr(buffer.as_ptr()) };
        assert_eq!(text.to_bytes(), b"1 2 3 4 5 6 7");
    }

    /// And a floating-point argument, which travels in a vector register and is
    /// read out of the other half of the save area.
    #[test]
    fn a_double_is_read_from_the_vector_registers() {
        let mut buffer = [0i8; 64];
        unsafe {
            published_snprintf_l()(
                buffer.as_mut_ptr(),
                buffer.len(),
                std::ptr::null_mut(),
                c"%s=%.2f".as_ptr(),
                c"x".as_ptr(),
                9.25f64,
            );
            assert_eq!(CStr::from_ptr(buffer.as_ptr()).to_bytes(), b"x=9.25");
        }
    }

    /// The locale is switched for the call and put back after it, which is what
    /// the whole `_l` family is for: the same number formats differently.
    #[test]
    fn the_locale_argument_is_the_one_the_number_is_formatted_in() {
        // SAFETY: a locale this test makes and frees.
        let c_locale = unsafe {
            libc::newlocale(
                crate::locale::all_mask(),
                c"C".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        if c_locale.is_null() {
            eprintln!("skipped: this host has no C locale");
            return;
        }
        let mut buffer = [0i8; 32];
        // SAFETY: as above, with a real locale rather than a null one.
        unsafe {
            let before = libc::uselocale(std::ptr::null_mut());
            published_snprintf_l()(
                buffer.as_mut_ptr(),
                buffer.len(),
                c_locale,
                c"%.1f".as_ptr(),
                0.5f64,
            );
            assert_eq!(CStr::from_ptr(buffer.as_ptr()).to_bytes(), b"0.5");
            assert_eq!(
                libc::uselocale(std::ptr::null_mut()),
                before,
                "the thread is left where it was"
            );
            libc::freelocale(c_locale);
        }
    }
}
