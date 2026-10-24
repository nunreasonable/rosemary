//! `CMBlockBuffer`: a run of bytes a sample buffer carries.
//!
//! Always one contiguous segment here. The bytes are owned (a block this
//! module allocated or copied), borrowed (the caller's memory, never freed),
//! or a reference into another block buffer, which is retained and whose
//! bytes are shared, as Apple's are.
//!
//! FIXME (ACCURACY): `kCFAllocatorNull` and `kCFAllocatorDefault` are both
//! null in this runtime, so a memory block handed over with an allocator that
//! should free it is borrowed instead, and leaks.

use std::ffi::c_void;
use std::sync::OnceLock;

use corefoundation::base::{CFTypeID, CFTypeRef};
use objc::class::Class;
use objc::object::Id;

/// `OSStatus` values from `CMBlockBuffer.h`.
pub const STRUCTURE_ALLOCATION_FAILED: i32 = -12700;
pub const BLOCK_ALLOCATION_FAILED: i32 = -12701;
pub const BAD_CUSTOM_BLOCK_SOURCE: i32 = -12702;
pub const BAD_OFFSET_PARAMETER: i32 = -12703;
pub const BAD_LENGTH_PARAMETER: i32 = -12704;
pub const BAD_POINTER_PARAMETER: i32 = -12705;
const NO_ERR: i32 = 0;

/// `kCMBlockBufferAlwaysCopyDataFlag`.
pub const ALWAYS_COPY_DATA: u32 = 2;

#[repr(C)]
struct Block {
    isa: *const c_void,
    state: State,
}

enum Storage {
    /// A boxed slice this buffer frees, as a raw pointer so that
    /// `CMBlockBufferReplaceDataBytes` may write through a shared reference.
    Owned {
        ptr: *mut u8,
        len: usize,
    },
    Borrowed {
        ptr: *mut u8,
    },
    /// Another block buffer, retained.
    Reference {
        source: CFTypeRef,
    },
}

struct State {
    storage: Storage,
    /// Where the data starts inside the storage.
    offset: usize,
    length: usize,
}

impl Drop for State {
    fn drop(&mut self) {
        match self.storage {
            Storage::Owned { ptr, len } => {
                // SAFETY: made by `owned` from a boxed slice of `len` bytes.
                drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
            }
            Storage::Borrowed { .. } => {}
            Storage::Reference { source } => crate::release(source),
        }
    }
}

fn class() -> &'static Class {
    static CLASS: OnceLock<usize> = OnceLock::new();
    let p = *CLASS.get_or_init(|| {
        corefoundation::bootstrap();
        let class =
            corefoundation::base::register(c"CMBlockBuffer", None, size_of::<Block>(), true);
        class.replace_method(
            objc::sel::get("dealloc"),
            dealloc as *const c_void,
            c"v16@0:8",
        );
        objc::class::ptr(class) as usize
    });
    // SAFETY: a leaked class registered above.
    unsafe { &*(p as *const Class) }
}

unsafe extern "C" fn dealloc(receiver: Id, _sel: objc::sel::Sel) {
    // SAFETY: `-dealloc` of a `CMBlockBuffer`, whose body is `Block`.
    let block = unsafe { &mut *(receiver as *mut Block) };
    // SAFETY: written once when the buffer was made, dropped only here.
    unsafe { std::ptr::drop_in_place(&mut block.state) };
    objc::arc::finalize(receiver);
}

fn state<'a>(buffer: CFTypeRef) -> Option<&'a State> {
    // SAFETY: null or an object a guest received from this runtime.
    let is_block =
        !buffer.is_null() && unsafe { corefoundation::base::is_kind_of(buffer, class()) };
    // SAFETY: checked to be a `CMBlockBuffer`, whose body is `Block`.
    is_block.then(|| unsafe { &(*(buffer as *const Block)).state })
}

/// Where a buffer's data is, and how long it is.
fn region(state: &State) -> (*mut u8, usize) {
    let base = match state.storage {
        Storage::Owned { ptr, .. } | Storage::Borrowed { ptr } => ptr,
        Storage::Reference { source } => match self::state(source) {
            Some(source) => region(source).0,
            None => std::ptr::null_mut(),
        },
    };
    match base.is_null() {
        true => (base, 0),
        // SAFETY: `offset` was checked against the storage when it was made.
        false => (unsafe { base.add(state.offset) }, state.length),
    }
}

fn make(storage: Storage, offset: usize, length: usize) -> CFTypeRef {
    // SAFETY: `Block` is the registered layout.
    let block = unsafe { corefoundation::base::allocate::<Block>(class()) };
    // SAFETY: a fresh, zeroed allocation whose state is written once here.
    unsafe {
        std::ptr::write(
            &mut (*block).state,
            State {
                storage,
                offset,
                length,
            },
        )
    };
    block as CFTypeRef
}

fn owned(bytes: Vec<u8>) -> Storage {
    let len = bytes.len();
    let ptr = Box::into_raw(bytes.into_boxed_slice()) as *mut u8;
    Storage::Owned { ptr, len }
}

/// The bytes of a block buffer, or `None` for anything else.
///
/// Valid for as long as the caller holds a reference to `buffer`.
pub fn bytes<'a>(buffer: CFTypeRef) -> Option<&'a [u8]> {
    let (ptr, len) = region(state(buffer)?);
    match ptr.is_null() {
        true => Some(&[]),
        // SAFETY: the buffer's own region, alive while it is.
        false => Some(unsafe { std::slice::from_raw_parts(ptr, len) }),
    }
}

/// `OSStatus CMBlockBufferCreateWithMemoryBlock(CFAllocatorRef structureAllocator,
/// void *memoryBlock, size_t blockLength, CFAllocatorRef blockAllocator,
/// const CMBlockBufferCustomBlockSource *customBlockSource, size_t offsetToData,
/// size_t dataLength, CMBlockBufferFlags flags, CMBlockBufferRef *blockBufferOut)`.
///
/// A null memory block is allocated, zeroed, now rather than on first use.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn CMBlockBufferCreateWithMemoryBlock(
    _structure_allocator: CFTypeRef,
    memory_block: *mut c_void,
    block_length: usize,
    _block_allocator: CFTypeRef,
    custom_block_source: *const c_void,
    offset_to_data: usize,
    data_length: usize,
    _flags: u32,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() {
        return BAD_POINTER_PARAMETER;
    }
    if !custom_block_source.is_null() {
        eprintln!(
            "rosemary: CMBlockBufferCreateWithMemoryBlock: custom block sources are not supported"
        );
        return BAD_CUSTOM_BLOCK_SOURCE;
    }
    let fits = offset_to_data
        .checked_add(data_length)
        .is_some_and(|end| end <= block_length);
    if block_length == 0 || !fits {
        return BAD_LENGTH_PARAMETER;
    }
    let storage = match memory_block.is_null() {
        true => {
            let mut bytes = Vec::new();
            if bytes.try_reserve_exact(block_length).is_err() {
                return BLOCK_ALLOCATION_FAILED;
            }
            bytes.resize(block_length, 0);
            owned(bytes)
        }
        false => Storage::Borrowed {
            ptr: memory_block.cast(),
        },
    };
    let made = make(storage, offset_to_data, data_length);
    // SAFETY: the caller's out-parameter, checked above.
    unsafe { out.write(made) };
    NO_ERR
}

/// `OSStatus CMBlockBufferCreateContiguous(CFAllocatorRef structureAllocator,
/// CMBlockBufferRef sourceBuffer, CFAllocatorRef blockAllocator,
/// const CMBlockBufferCustomBlockSource *customBlockSource, size_t offsetToData,
/// size_t dataLength, CMBlockBufferFlags flags, CMBlockBufferRef *blockBufferOut)`.
///
/// Every buffer here is contiguous already, so without
/// `kCMBlockBufferAlwaysCopyDataFlag` the result refers to the source's bytes.
/// A length of zero means the rest of the source.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn CMBlockBufferCreateContiguous(
    _structure_allocator: CFTypeRef,
    source: CFTypeRef,
    _block_allocator: CFTypeRef,
    custom_block_source: *const c_void,
    offset_to_data: usize,
    data_length: usize,
    flags: u32,
    out: *mut CFTypeRef,
) -> i32 {
    if out.is_null() {
        return BAD_POINTER_PARAMETER;
    }
    let Some(source_state) = state(source) else {
        return BAD_POINTER_PARAMETER;
    };
    if !custom_block_source.is_null() {
        eprintln!(
            "rosemary: CMBlockBufferCreateContiguous: custom block sources are not supported"
        );
        return BAD_CUSTOM_BLOCK_SOURCE;
    }
    let total = source_state.length;
    if offset_to_data > total || (offset_to_data == total && total != 0) {
        return BAD_OFFSET_PARAMETER;
    }
    let length = match data_length {
        0 => total - offset_to_data,
        n => n,
    };
    if offset_to_data
        .checked_add(length)
        .is_none_or(|end| end > total)
    {
        return BAD_LENGTH_PARAMETER;
    }
    let made = match flags & ALWAYS_COPY_DATA != 0 {
        true => {
            let Some(bytes) = bytes(source) else {
                return BAD_POINTER_PARAMETER;
            };
            make(
                owned(bytes[offset_to_data..offset_to_data + length].to_vec()),
                0,
                length,
            )
        }
        false => make(
            Storage::Reference {
                // SAFETY: a live block buffer, checked above.
                source: unsafe { corefoundation::api::CFRetain(source) },
            },
            offset_to_data,
            length,
        ),
    };
    // SAFETY: the caller's out-parameter, checked above.
    unsafe { out.write(made) };
    NO_ERR
}

/// `OSStatus CMBlockBufferGetDataPointer(CMBlockBufferRef, size_t offset,
/// size_t *lengthAtOffsetOut, size_t *totalLengthOut, char **dataPointerOut)`:
/// every out-parameter may be null.
pub unsafe extern "C" fn CMBlockBufferGetDataPointer(
    buffer: CFTypeRef,
    offset: usize,
    length_at_offset_out: *mut usize,
    total_length_out: *mut usize,
    data_pointer_out: *mut *mut u8,
) -> i32 {
    let found = state(buffer).map(region);
    let answer = match found {
        None => Err(BAD_POINTER_PARAMETER),
        Some((_, len)) if offset >= len => Err(BAD_OFFSET_PARAMETER),
        // SAFETY: inside the buffer's region.
        Some((ptr, len)) => Ok((unsafe { ptr.add(offset) }, len - offset, len)),
    };
    let (pointer, at_offset, total) = answer.unwrap_or((std::ptr::null_mut(), 0, 0));
    // SAFETY: each out-parameter is the caller's, or null.
    unsafe {
        if !length_at_offset_out.is_null() {
            length_at_offset_out.write(at_offset);
        }
        if !total_length_out.is_null() {
            total_length_out.write(total);
        }
        if !data_pointer_out.is_null() {
            data_pointer_out.write(pointer);
        }
    }
    answer.map_or_else(|status| status, |_| NO_ERR)
}

/// `OSStatus CMBlockBufferReplaceDataBytes(const void *sourceBytes,
/// CMBlockBufferRef destinationBuffer, size_t offsetIntoDestination,
/// size_t dataLength)`: written in place, so through a reference into the
/// buffer it refers to.
pub unsafe extern "C" fn CMBlockBufferReplaceDataBytes(
    source_bytes: *const c_void,
    destination: CFTypeRef,
    offset: usize,
    length: usize,
) -> i32 {
    let Some(state) = state(destination) else {
        return BAD_POINTER_PARAMETER;
    };
    if source_bytes.is_null() && length != 0 {
        return BAD_POINTER_PARAMETER;
    }
    let (ptr, len) = region(state);
    if offset.checked_add(length).is_none_or(|end| end > len) {
        return BAD_LENGTH_PARAMETER;
    }
    if length != 0 {
        // SAFETY: the caller's `length` bytes, into the buffer's region whose
        // bounds were checked above. `copy` because the two may overlap.
        unsafe { std::ptr::copy(source_bytes.cast::<u8>(), ptr.add(offset), length) };
    }
    NO_ERR
}

/// `size_t CMBlockBufferGetDataLength(CMBlockBufferRef)`.
pub unsafe extern "C" fn CMBlockBufferGetDataLength(buffer: CFTypeRef) -> usize {
    state(buffer).map_or(0, |s| s.length)
}

/// `CFTypeID CMBlockBufferGetTypeID(void)`.
pub unsafe extern "C" fn CMBlockBufferGetTypeID() -> CFTypeID {
    corefoundation::base::type_id(class())
}

exports::exports! {
    functions {
        "CMBlockBufferCreateWithMemoryBlock" => CMBlockBufferCreateWithMemoryBlock,
        "CMBlockBufferCreateContiguous" => CMBlockBufferCreateContiguous,
        "CMBlockBufferGetDataPointer" => CMBlockBufferGetDataPointer,
        "CMBlockBufferReplaceDataBytes" => CMBlockBufferReplaceDataBytes,
        "CMBlockBufferGetDataLength" => CMBlockBufferGetDataLength,
        "CMBlockBufferGetTypeID" => CMBlockBufferGetTypeID,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A buffer over `memory`, as Roblox makes one: `kCFAllocatorNull`.
    pub(crate) fn borrowed(memory: &mut [u8]) -> CFTypeRef {
        let mut out = std::ptr::null();
        // SAFETY: the caller's memory and a live out-parameter.
        let status = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                std::ptr::null(),
                memory.as_mut_ptr().cast(),
                memory.len(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                memory.len(),
                0,
                &mut out,
            )
        };
        assert_eq!(status, NO_ERR);
        out
    }

    fn contiguous(source: CFTypeRef, offset: usize, length: usize, flags: u32) -> (i32, CFTypeRef) {
        let mut out = std::ptr::null();
        // SAFETY: a live out-parameter.
        let status = unsafe {
            CMBlockBufferCreateContiguous(
                std::ptr::null(),
                source,
                std::ptr::null(),
                std::ptr::null(),
                offset,
                length,
                flags,
                &mut out,
            )
        };
        (status, out)
    }

    fn release(object: CFTypeRef) {
        // SAFETY: an object the test owns a reference to.
        unsafe { corefoundation::api::CFRelease(object) };
    }

    #[test]
    fn a_borrowed_block_reads_through_and_is_never_freed() {
        let mut memory = vec![1u8, 2, 3, 4, 5];
        let block = borrowed(&mut memory);
        assert_eq!(bytes(block), Some(&[1u8, 2, 3, 4, 5][..]));
        assert_eq!(bytes(block).unwrap().as_ptr(), memory.as_ptr());
        memory[2] = 9;
        assert_eq!(
            bytes(block).unwrap()[2],
            9,
            "the caller's memory, not a copy"
        );
        // SAFETY: a block this test made.
        unsafe {
            assert_eq!(CMBlockBufferGetDataLength(block), 5);
            assert_eq!(
                corefoundation::api::CFGetTypeID(block),
                CMBlockBufferGetTypeID()
            );
        }
        release(block);
        // Still the caller's to read and free.
        assert_eq!(memory, [1, 2, 9, 4, 5]);
    }

    #[test]
    fn a_null_memory_block_is_allocated_zeroed() {
        let mut out = std::ptr::null();
        // SAFETY: a live out-parameter.
        let status = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                std::ptr::null(),
                std::ptr::null_mut(),
                8,
                std::ptr::null(),
                std::ptr::null(),
                2,
                4,
                0,
                &mut out,
            )
        };
        assert_eq!(status, NO_ERR);
        assert_eq!(bytes(out), Some(&[0u8; 4][..]));
        release(out);
    }

    #[test]
    fn contiguous_with_always_copy_owns_a_copy() {
        let mut memory = vec![10u8, 20, 30, 40];
        let source = borrowed(&mut memory);
        let (status, copy) = contiguous(source, 1, 2, ALWAYS_COPY_DATA);
        assert_eq!(status, NO_ERR);
        assert_eq!(bytes(copy), Some(&[20u8, 30][..]));
        memory[1] = 99;
        assert_eq!(
            bytes(copy),
            Some(&[20u8, 30][..]),
            "unchanged by the source"
        );
        release(source);
        assert_eq!(bytes(copy), Some(&[20u8, 30][..]), "outlives the source");
        release(copy);
    }

    #[test]
    fn contiguous_without_copy_aliases_the_source() {
        let mut memory = vec![10u8, 20, 30, 40];
        let source = borrowed(&mut memory);
        // SAFETY: a block this test made.
        let before = unsafe { corefoundation::api::CFGetRetainCount(source) };
        let (status, alias) = contiguous(source, 2, 0, 0);
        assert_eq!(status, NO_ERR);
        assert_eq!(bytes(alias), Some(&[30u8, 40][..]));
        assert_eq!(bytes(alias).unwrap().as_ptr(), memory[2..].as_ptr());
        // SAFETY: as above.
        assert_eq!(
            unsafe { corefoundation::api::CFGetRetainCount(source) },
            before + 1
        );
        // A write through the alias lands in the source.
        // SAFETY: two bytes from a live array into the alias.
        let status =
            unsafe { CMBlockBufferReplaceDataBytes([7u8, 8].as_ptr().cast(), alias, 0, 2) };
        assert_eq!(status, NO_ERR);
        assert_eq!(bytes(source), Some(&[10u8, 20, 7, 8][..]));
        release(alias);
        // SAFETY: as above.
        assert_eq!(
            unsafe { corefoundation::api::CFGetRetainCount(source) },
            before
        );
        release(source);
    }

    #[test]
    fn data_pointer_reports_length_at_offset_and_total() {
        let mut memory = vec![0u8; 16];
        let block = borrowed(&mut memory);
        let (mut at_offset, mut total, mut pointer) = (0usize, 0usize, std::ptr::null_mut());
        // SAFETY: a block this test made and live out-parameters, then null
        // ones.
        unsafe {
            assert_eq!(
                CMBlockBufferGetDataPointer(block, 5, &mut at_offset, &mut total, &mut pointer),
                NO_ERR
            );
            assert_eq!((at_offset, total), (11, 16));
            assert_eq!(pointer, memory.as_mut_ptr().add(5));
            assert_eq!(
                CMBlockBufferGetDataPointer(
                    block,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                NO_ERR
            );
        }
        release(block);
    }

    #[test]
    fn bad_offset_and_length_are_rejected() {
        let mut memory = vec![0u8; 8];
        let block = borrowed(&mut memory);
        let mut pointer = std::ptr::dangling_mut::<u8>();
        // SAFETY: a block this test made and live out-parameters.
        unsafe {
            assert_eq!(
                CMBlockBufferGetDataPointer(
                    block,
                    8,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut pointer
                ),
                BAD_OFFSET_PARAMETER
            );
            assert!(pointer.is_null());
            let mut out = std::ptr::null();
            assert_eq!(
                CMBlockBufferCreateWithMemoryBlock(
                    std::ptr::null(),
                    memory.as_mut_ptr().cast(),
                    8,
                    std::ptr::null(),
                    std::ptr::null(),
                    4,
                    5,
                    0,
                    &mut out,
                ),
                BAD_LENGTH_PARAMETER
            );
            assert!(out.is_null());
        }
        assert_eq!(contiguous(block, 9, 0, 0).0, BAD_OFFSET_PARAMETER);
        assert_eq!(contiguous(block, 8, 0, 0).0, BAD_OFFSET_PARAMETER);
        assert_eq!(contiguous(block, 4, 5, 0).0, BAD_LENGTH_PARAMETER);
        assert_eq!(
            contiguous(std::ptr::null(), 0, 0, 0).0,
            BAD_POINTER_PARAMETER
        );
        release(block);
    }

    #[test]
    fn replace_data_bytes_writes_in_place_and_checks_bounds() {
        let mut memory = vec![0u8; 6];
        let block = borrowed(&mut memory);
        // SAFETY: a block this test made and live source bytes.
        unsafe {
            assert_eq!(
                CMBlockBufferReplaceDataBytes([1u8, 2, 3].as_ptr().cast(), block, 3, 3),
                NO_ERR
            );
            assert_eq!(
                CMBlockBufferReplaceDataBytes([1u8, 2, 3].as_ptr().cast(), block, 4, 3),
                BAD_LENGTH_PARAMETER
            );
        }
        release(block);
        assert_eq!(memory, [0, 0, 0, 1, 2, 3]);
    }

    #[test]
    fn custom_block_source_is_refused() {
        let source = [0usize; 4];
        let mut out = std::ptr::null();
        // SAFETY: a live out-parameter; the source is never read.
        let status = unsafe {
            CMBlockBufferCreateWithMemoryBlock(
                std::ptr::null(),
                std::ptr::null_mut(),
                8,
                std::ptr::null(),
                source.as_ptr().cast(),
                0,
                8,
                0,
                &mut out,
            )
        };
        assert_eq!(status, BAD_CUSTOM_BLOCK_SOURCE);
        assert!(out.is_null());
    }
}
