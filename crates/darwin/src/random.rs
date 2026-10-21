#![cfg(not(target_os = "macos"))]

unsafe extern "C" fn arc4random() -> u32 {
    let mut bytes = [0; 4];
    if getrandom::fill(&mut bytes).is_err() {
        // FIXME (ACCURACY): should we really abort here?
        std::process::abort();
    }
    u32::from_ne_bytes(bytes)
}
unsafe extern "C" fn arc4random_buf(out: *mut u8, len: usize) {
    if len == 0 || out.is_null() {
        return;
    }
    if getrandom::fill(unsafe { std::slice::from_raw_parts_mut(out, len) }).is_err() {
        // FIXME (ACCURACY): should we really abort here?
        std::process::abort();
    }
}

/// `CCRNGStatus CCRandomGenerateBytes(void *bytes, size_t count)`: CommonCrypto's
/// way to the system generator, answering `kCCSuccess` (0).
#[cfg(test)]
unsafe extern "C" fn cc_random_generate_bytes(out: *mut u8, len: usize) -> i32 {
    if len == 0 {
        return 0;
    }
    if out.is_null() {
        // `kCCParamError`.
        return -4300;
    }
    match getrandom::fill(unsafe { std::slice::from_raw_parts_mut(out, len) }) {
        Ok(()) => 0,
        // `kCCRNGFailure`.
        Err(_) => -4309,
    }
}

exports::exports! {
    functions {
        "arc4random" => arc4random,
        "arc4random_buf" => arc4random_buf,
    }
}
