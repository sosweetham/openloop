use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};

use tether_openxr_abi::{XrResult, XrStructureType};

/// Runs an entry point's body, turning a panic into `XR_ERROR_RUNTIME_FAILURE`
/// instead of unwinding into (or aborting) the host app.
pub fn guard(body: impl FnOnce() -> XrResult) -> XrResult {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(XrResult::XR_ERROR_RUNTIME_FAILURE)
}

pub type XrOutcome = Result<XrResult, XrResult>;

/// [`guard`] for bodies written with `?`: `Ok` carries a success code, `Err` a failure.
pub fn call(body: impl FnOnce() -> XrOutcome) -> XrResult {
    guard(|| body().unwrap_or_else(|e| e))
}

pub const OK: XrOutcome = Ok(XrResult::XR_SUCCESS);

/// A caller-provided input struct; null is a validation failure.
///
/// # Safety
/// Non-null pointers must be valid for reads for the duration of the call.
pub unsafe fn read<'a, T>(p: *const T) -> Result<&'a T, XrResult> {
    // SAFETY: forwarded from the caller.
    unsafe { p.as_ref() }.ok_or(XrResult::XR_ERROR_VALIDATION_FAILURE)
}

/// A caller-provided output struct; null is a validation failure.
///
/// # Safety
/// Non-null pointers must be valid for writes for the duration of the call.
pub unsafe fn write<'a, T>(p: *mut T) -> Result<&'a mut T, XrResult> {
    // SAFETY: forwarded from the caller.
    unsafe { p.as_mut() }.ok_or(XrResult::XR_ERROR_VALIDATION_FAILURE)
}

/// A counted array from an input struct; `count == 0` allows a null pointer.
///
/// # Safety
/// When `count > 0`, `p` must point to `count` valid elements.
pub unsafe fn slice<'a, T>(p: *const T, count: u32) -> Result<&'a [T], XrResult> {
    match count {
        0 => Ok(&[]),
        _ if p.is_null() => Err(XrResult::XR_ERROR_VALIDATION_FAILURE),
        // SAFETY: forwarded from the caller.
        n => Ok(unsafe { std::slice::from_raw_parts(p, n as usize) }),
    }
}

pub fn expect_type(actual: XrStructureType, expected: XrStructureType) -> Result<(), XrResult> {
    if actual == expected {
        Ok(())
    } else {
        Err(XrResult::XR_ERROR_VALIDATION_FAILURE)
    }
}

/// A fixed-size C string field; `None` if it has no terminator or is not UTF-8.
pub fn fixed_str(field: &[c_char]) -> Option<String> {
    let bytes: Vec<u8> = field.iter().map(|&c| c as u8).collect();
    CStr::from_bytes_until_nul(&bytes)
        .ok()?
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Walks a `next` chain for the first struct of type `ty`.
///
/// # Safety
/// `next` must be null or a valid OpenXR structure chain.
pub unsafe fn find_in_chain<T>(
    mut next: *const std::ffi::c_void,
    ty: XrStructureType,
) -> Option<*mut T> {
    #[repr(C)]
    struct Header {
        ty: XrStructureType,
        next: *const std::ffi::c_void,
    }
    while !next.is_null() {
        // SAFETY: every OpenXR chain element starts with `type` and `next`.
        let header = unsafe { &*(next as *const Header) };
        if header.ty == ty {
            return Some(next as *mut T);
        }
        next = header.next;
    }
    None
}

/// Copies `src` into a fixed-size C string field, truncating to leave room for NUL.
pub fn write_str(dst: &mut [c_char], src: &str) {
    let len = src.len().min(dst.len().saturating_sub(1));
    for (d, s) in dst.iter_mut().zip(&src.as_bytes()[..len]) {
        *d = *s as c_char;
    }
    dst[len] = 0;
}

/// The OpenXR two-call idiom (spec §2.11): capacity 0 asks for the count only.
///
/// # Safety
/// `count_output` must be valid for writes; `items` must be valid for
/// `capacity` writes when `capacity` is non-zero.
pub unsafe fn two_call<T, S>(
    source: &[S],
    capacity: u32,
    count_output: *mut u32,
    items: *mut T,
    mut fill: impl FnMut(&mut T, &S) -> Result<(), XrResult>,
) -> XrResult {
    if count_output.is_null() {
        return XrResult::XR_ERROR_VALIDATION_FAILURE;
    }
    let count = source.len() as u32;
    // SAFETY: checked non-null; validity is the caller's contract.
    unsafe { *count_output = count };
    if capacity == 0 {
        return XrResult::XR_SUCCESS;
    }
    if capacity < count {
        return XrResult::XR_ERROR_SIZE_INSUFFICIENT;
    }
    if items.is_null() {
        return XrResult::XR_ERROR_VALIDATION_FAILURE;
    }
    // SAFETY: `items` holds at least `capacity >= count` elements per the contract.
    let out = unsafe { std::slice::from_raw_parts_mut(items, source.len()) };
    for (dst, src) in out.iter_mut().zip(source) {
        if let Err(e) = fill(dst, src) {
            return e;
        }
    }
    XrResult::XR_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_str_truncates_and_terminates() {
        let mut buf = [1 as c_char; 4];
        write_str(&mut buf, "tether");
        assert_eq!(buf, [b't' as c_char, b'e' as c_char, b't' as c_char, 0]);
    }

    #[test]
    fn two_call_reports_count_then_fills() {
        let src = [10u32, 20, 30];
        let copy = |d: &mut u32, s: &u32| {
            *d = *s;
            Ok(())
        };
        let mut count = 0;
        let r = unsafe {
            two_call(&src, 0, &mut count, std::ptr::null_mut::<u32>(), |_, _| {
                Ok(())
            })
        };
        assert_eq!((r, count), (XrResult::XR_SUCCESS, 3));

        let mut small = [0u32; 2];
        let r = unsafe { two_call(&src, 2, &mut count, small.as_mut_ptr(), copy) };
        assert_eq!(r, XrResult::XR_ERROR_SIZE_INSUFFICIENT);

        let mut out = [0u32; 3];
        let r = unsafe { two_call(&src, 3, &mut count, out.as_mut_ptr(), copy) };
        assert_eq!((r, out), (XrResult::XR_SUCCESS, src));
    }
}
