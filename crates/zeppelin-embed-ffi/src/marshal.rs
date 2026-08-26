//! Pure raw-parts validation for the ABI boundary.

use std::mem::{align_of, size_of};

use crate::abi::ZE_ABI_MAX_STRUCT_SIZE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MarshalError(pub(crate) &'static str);

pub(crate) fn read_struct<T: Copy>(pointer: *const T) -> Result<T, MarshalError> {
    if pointer.is_null() {
        return Err(MarshalError("request pointer is null"));
    }
    if pointer.align_offset(align_of::<T>()) != 0 {
        return Err(MarshalError("request pointer is misaligned"));
    }
    let abi_size = unsafe { std::ptr::read(pointer.cast::<u32>()) };
    validate_abi_size::<T>(abi_size)?;
    let reserved = unsafe { std::ptr::read(pointer.cast::<u32>().add(1)) };
    if reserved != 0 {
        return Err(MarshalError("abi_reserved must be zero"));
    }
    Ok(unsafe { std::ptr::read(pointer) })
}

pub(crate) fn validate_output<T>(pointer: *mut T) -> Result<u32, MarshalError> {
    if pointer.is_null() {
        return Err(MarshalError("output pointer is null"));
    }
    if pointer.align_offset(align_of::<T>()) != 0 {
        return Err(MarshalError("output pointer is misaligned"));
    }
    let abi_size = unsafe { std::ptr::read(pointer.cast::<u32>()) };
    validate_abi_size::<T>(abi_size)?;
    let reserved = unsafe { std::ptr::read(pointer.cast::<u32>().add(1)) };
    if reserved != 0 {
        return Err(MarshalError("abi_reserved must be zero"));
    }
    Ok(abi_size)
}

pub(crate) fn write_output<T>(pointer: *mut T, value: T) {
    unsafe { std::ptr::write(pointer, value) };
}

pub(crate) fn write_scalar<T>(pointer: *mut T, value: T) {
    unsafe { std::ptr::write(pointer, value) };
}

pub(crate) fn read_value<T: Copy>(pointer: *const T) -> T {
    unsafe { std::ptr::read(pointer) }
}

pub(crate) fn read_abi_size<T>(pointer: *const T) -> u32 {
    unsafe { std::ptr::read(pointer.cast::<u32>()) }
}

pub(crate) fn copy_nul_terminated(message: &str, output: *mut std::ffi::c_char) {
    unsafe {
        std::ptr::copy_nonoverlapping(message.as_ptr(), output.cast::<u8>(), message.len());
        std::ptr::write(output.cast::<u8>().add(message.len()), 0);
    }
}

fn validate_abi_size<T>(abi_size: u32) -> Result<(), MarshalError> {
    if abi_size > ZE_ABI_MAX_STRUCT_SIZE {
        return Err(MarshalError("abi_size exceeds the 64 KiB ABI limit"));
    }
    let minimum = u32::try_from(size_of::<T>())
        .map_err(|_| MarshalError("known ABI structure exceeds u32"))?;
    if abi_size < minimum {
        return Err(MarshalError("abi_size is smaller than the ABI v1 prefix"));
    }
    Ok(())
}

pub(crate) fn read_slice<'a, T>(pointer: *const T, count: usize) -> Result<&'a [T], MarshalError> {
    let bytes = count
        .checked_mul(size_of::<T>())
        .ok_or(MarshalError("element count overflows address space"))?;
    if bytes > isize::MAX as usize {
        return Err(MarshalError("input buffer exceeds the maximum Rust slice"));
    }
    if count == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() {
        return Err(MarshalError("nonempty input buffer pointer is null"));
    }
    if pointer.align_offset(align_of::<T>()) != 0 {
        return Err(MarshalError("input buffer pointer is misaligned"));
    }
    Ok(unsafe { std::slice::from_raw_parts(pointer, count) })
}

pub(crate) fn copy_slice<T: Copy>(pointer: *const T, count: usize) -> Result<Vec<T>, MarshalError> {
    let source = read_slice(pointer, count)?;
    let mut copy = Vec::new();
    copy.try_reserve_exact(count)
        .map_err(|_| MarshalError("input copy allocation failed"))?;
    copy.extend_from_slice(source);
    Ok(copy)
}

pub(crate) fn checked_buffer_len(
    count: usize,
    element_size: usize,
    supplied_bytes: usize,
) -> Result<(), MarshalError> {
    let required = count
        .checked_mul(element_size)
        .ok_or(MarshalError("buffer byte length overflows address space"))?;
    if required != supplied_bytes {
        return Err(MarshalError("buffer length does not match element count"));
    }
    Ok(())
}

pub(crate) fn utf8_without_nul<'a>(
    pointer: *const u8,
    length: usize,
) -> Result<&'a str, MarshalError> {
    let bytes = read_slice(pointer, length)?;
    if bytes.contains(&0) {
        return Err(MarshalError("path contains an interior NUL byte"));
    }
    std::str::from_utf8(bytes).map_err(|_| MarshalError("path is not valid UTF-8"))
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug)]
    #[repr(C)]
    struct Request {
        abi_size: u32,
        abi_reserved: u32,
        value: u64,
    }

    #[test]
    fn size_prefix_is_bounded_and_forward_compatible() {
        let request = Request {
            abi_size: size_of::<Request>() as u32,
            abi_reserved: 0,
            value: 7,
        };
        assert_eq!(read_struct(&request).unwrap().value, 7);
        let future = Request {
            abi_size: size_of::<Request>() as u32 + 64,
            ..request
        };
        assert_eq!(read_struct(&future).unwrap().value, 7);
        let short = Request {
            abi_size: 8,
            ..request
        };
        assert_eq!(
            read_struct(&short).unwrap_err(),
            MarshalError("abi_size is smaller than the ABI v1 prefix")
        );
        let absurd = Request {
            abi_size: ZE_ABI_MAX_STRUCT_SIZE + 1,
            ..request
        };
        assert_eq!(
            read_struct(&absurd).unwrap_err(),
            MarshalError("abi_size exceeds the 64 KiB ABI limit")
        );
    }

    #[test]
    fn raw_parts_reject_null_misalignment_and_arithmetic_overflow() {
        assert!(read_slice::<u32>(std::ptr::null(), 0).unwrap().is_empty());
        assert_eq!(
            read_slice::<u32>(std::ptr::null(), 1).unwrap_err(),
            MarshalError("nonempty input buffer pointer is null")
        );
        // Offset from a u32-aligned buffer: a `[u8; 8]` has alignment one, so
        // its address plus one is 4-aligned whenever the base is 3 mod 4,
        // which Miri's randomized placement reaches and native stacks can too.
        let aligned = [0_u32; 2];
        let misaligned = unsafe { aligned.as_ptr().cast::<u8>().add(1).cast::<u32>() };
        assert_eq!(
            read_slice::<u32>(misaligned, 1).unwrap_err(),
            MarshalError("input buffer pointer is misaligned")
        );
        assert_eq!(
            checked_buffer_len(usize::MAX, 2, 0).unwrap_err(),
            MarshalError("buffer byte length overflows address space")
        );
    }

    #[test]
    fn paths_are_explicit_utf8_without_nul() {
        let valid = b"store";
        assert_eq!(
            utf8_without_nul(valid.as_ptr(), valid.len()).unwrap(),
            "store"
        );
        let invalid = [0xff_u8];
        assert_eq!(
            utf8_without_nul(invalid.as_ptr(), invalid.len()).unwrap_err(),
            MarshalError("path is not valid UTF-8")
        );
        let nul = b"a\0b";
        assert_eq!(
            utf8_without_nul(nul.as_ptr(), nul.len()).unwrap_err(),
            MarshalError("path contains an interior NUL byte")
        );
    }
}
