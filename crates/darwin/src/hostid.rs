//! `gethostuuid`: the machine's identity, as a UUID.
//!
//! Derived from `/etc/machine-id` through a hash rather than handed over as it
//! is, so that a program learns a stable identity for this machine without
//! learning the host's own identifier. The same machine always answers the
//! same UUID.

use sha2::{Digest, Sha256};

fn host_uuid() -> [u8; 16] {
    let id = std::fs::read("/etc/machine-id")
        .or_else(|_| std::fs::read("/var/lib/dbus/machine-id"))
        .unwrap_or_default();
    let digest = Sha256::new()
        .chain_update(b"rosemary gethostuuid\0")
        .chain_update(id.trim_ascii())
        .finalize();
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&digest[..16]);
    // A version 5-shaped, RFC 4122 UUID: name-based, from a hash.
    uuid[6] = (uuid[6] & 0x0f) | 0x50;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    uuid
}

/// `int gethostuuid(uuid_t id, const struct timespec *wait)`.
///
/// # Safety
///
/// `out` is 16 writable bytes.
unsafe extern "C" fn gethostuuid(out: *mut u8, _wait: *const libc::timespec) -> libc::c_int {
    if out.is_null() {
        crate::errno::set_guest(14); // EFAULT
        return -1;
    }
    let uuid = host_uuid();
    // SAFETY: the caller's 16 bytes.
    unsafe { std::ptr::copy_nonoverlapping(uuid.as_ptr(), out, 16) };
    0
}

/// `int fsctl(const char *path, unsigned long request, void *data, unsigned
/// int options)`: the filesystem controls are HFS and APFS ones, which no
/// filesystem here answers. `ENOTSUP`, as Darwin says for a volume without them.
unsafe extern "C" fn fsctl(
    _path: *const libc::c_char,
    _request: u64,
    _data: *mut libc::c_void,
    _options: u32,
) -> libc::c_int {
    crate::errno::set_guest(45); // Darwin's ENOTSUP
    -1
}

exports::exports! {
    functions {
        "gethostuuid" => gethostuuid,
        "fsctl" => fsctl,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uuid_is_stable_and_well_formed() {
        let a = host_uuid();
        assert_eq!(a, host_uuid());
        assert_eq!(a[6] >> 4, 5);
        assert_eq!(a[8] >> 6, 0b10);
    }
}
