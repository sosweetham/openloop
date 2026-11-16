use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};

use tether_openxr_abi::XrResult;

/// Runs an entry point's body, turning a panic into `XR_ERROR_RUNTIME_FAILURE`
/// instead of unwinding into (or aborting) the host app.
pub fn guard(body: impl FnOnce() -> XrResult) -> XrResult {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(XrResult::XR_ERROR_RUNTIME_FAILURE)
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
