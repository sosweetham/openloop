//! The few libxpc calls the data plane needs, declared by hand from `<xpc/xpc.h>`; libxpc is
//! part of libSystem.

use std::ffi::{CStr, c_char, c_void};
use std::fmt;
use std::ptr::NonNull;

#[allow(non_camel_case_types)]
pub type xpc_object_t = *mut c_void;
#[allow(non_camel_case_types)]
pub type xpc_type_t = *const c_void;

#[repr(C)]
struct XpcType {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    static _xpc_type_shmem: XpcType;

    fn xpc_retain(object: xpc_object_t) -> xpc_object_t;
    fn xpc_release(object: xpc_object_t);
    fn xpc_get_type(object: xpc_object_t) -> xpc_type_t;
    fn xpc_copy_description(object: xpc_object_t) -> *mut c_char;
    pub(crate) fn xpc_shmem_create(region: *mut c_void, length: usize) -> xpc_object_t;
    pub(crate) fn xpc_shmem_map(object: xpc_object_t, region: *mut *mut c_void) -> usize;
}

/// `XPC_TYPE_SHMEM`, for `-[NSXPCInterface setXPCType:…]`.
pub fn shmem_type() -> xpc_type_t {
    (&raw const _xpc_type_shmem).cast()
}

/// One owned reference to an XPC object.
pub struct XpcObject(NonNull<c_void>);

// SAFETY: XPC objects are reference counted atomically, and the only kind this crate holds
// (shmem) is immutable after creation.
unsafe impl Send for XpcObject {}
// SAFETY: as above.
unsafe impl Sync for XpcObject {}

impl XpcObject {
    /// # Safety
    /// `object` is a valid XPC object whose +1 reference the caller hands over.
    pub(crate) unsafe fn from_owned(object: xpc_object_t) -> Option<XpcObject> {
        NonNull::new(object).map(XpcObject)
    }

    /// Retains an object borrowed from a message or a reply.
    ///
    /// # Safety
    /// `object` is null or a valid XPC object.
    pub unsafe fn retain(object: xpc_object_t) -> Option<XpcObject> {
        // SAFETY: the caller guarantees a valid object.
        NonNull::new(object)
            .map(|object| XpcObject(unsafe { NonNull::new_unchecked(xpc_retain(object.as_ptr())) }))
    }

    pub fn as_ptr(&self) -> xpc_object_t {
        self.0.as_ptr()
    }

    pub fn is_shmem(&self) -> bool {
        // SAFETY: a valid object.
        unsafe { xpc_get_type(self.as_ptr()) == shmem_type() }
    }
}

impl Clone for XpcObject {
    fn clone(&self) -> XpcObject {
        // SAFETY: a valid object; xpc_retain returns it.
        unsafe { xpc_retain(self.as_ptr()) };
        XpcObject(self.0)
    }
}

impl Drop for XpcObject {
    fn drop(&mut self) {
        // SAFETY: this reference is ours to give back.
        unsafe { xpc_release(self.as_ptr()) }
    }
}

impl fmt::Debug for XpcObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: a valid object; the description is malloc'd and ours to free.
        unsafe {
            let description = xpc_copy_description(self.as_ptr());
            if description.is_null() {
                return f.write_str("<xpc object>");
            }
            let result = f.write_str(&CStr::from_ptr(description).to_string_lossy());
            libc::free(description.cast());
            result
        }
    }
}
