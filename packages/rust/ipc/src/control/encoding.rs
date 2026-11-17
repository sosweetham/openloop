//! Type signatures for the reply blocks Rust creates. NSXPC refuses a block without one ("Block
//! was not compiled using a compiler that inserts type information about arguments"), and it
//! must match objc/TetherIPC.m: each string is clang's `_Block_signature` for that reply type,
//! and a test compares them with the protocol's extended method encodings.

use std::ffi::{CStr, c_void};

use objc2::runtime::AnyObject;
use objc2_foundation::NSData;

macro_rules! reply_encoding {
    ($name:ident, $block:ident, ($($argument:ty),*), $signature:expr) => {
        pub(crate) struct $name;

        /// The reply block as the exported object receives it and the client creates it.
        pub(crate) type $block = block2::Block<dyn Fn($($argument),*)>;

        // SAFETY: the signature is clang's for this argument list on 64-bit Apple platforms,
        // checked against the compiled protocol by `tests::reply_encodings_match_the_protocol`.
        unsafe impl block2::ManualBlockEncoding for $name {
            type Arguments = ($($argument,)*);
            type Return = ();
            const ENCODING_CSTR: &'static CStr = $signature;
        }
    };
}

reply_encoding!(
    HelloReply,
    HelloBlock,
    (u32, u32, u64, u64, *mut NSData),
    c"v40@?0I8I12Q16Q24@\"NSData\"32"
);
reply_encoding!(
    HeadsetReply,
    HeadsetBlock,
    (u32, *mut NSData),
    c"v20@?0I8@\"NSData\"12"
);
reply_encoding!(
    SessionReply,
    SessionBlock,
    (u32, u64, *mut c_void, *mut AnyObject, *mut AnyObject),
    c"v44@?0I8Q12@\"NSObject<OS_xpc_object>\"20@\"MTLSharedEventHandle\"28@\"MTLSharedEventHandle\"36"
);
reply_encoding!(StatusReply, StatusBlock, (u32), c"v12@?0I8");
reply_encoding!(
    SwapchainReply,
    SwapchainBlock,
    (u32, u64, *mut AnyObject),
    c"v28@?0I8Q12@\"NSArray\"20"
);

#[cfg(test)]
mod tests {
    use std::ffi::c_char;

    use block2::ManualBlockEncoding;
    use objc2::runtime::{AnyProtocol, Bool, Sel};
    use objc2::sel;

    use super::*;

    unsafe extern "C" {
        // libobjc's accessor for clang's extended method encodings, which NSXPCInterface reads
        // the same way; not in the public headers.
        fn _protocol_getMethodTypeEncoding(
            protocol: *const AnyProtocol,
            sel: Sel,
            required: Bool,
            instance: Bool,
        ) -> *const c_char;
    }

    /// The reply block's types inside the method's extended encoding: `…@?<v@?I>24` → `v@?I`.
    fn reply_types(sel: Sel) -> String {
        let protocol = crate::control::protocol();
        // SAFETY: a registered protocol; the string is owned by the runtime.
        let encoding = unsafe {
            let raw = _protocol_getMethodTypeEncoding(protocol, sel, Bool::YES, Bool::YES);
            assert!(!raw.is_null(), "{sel} has no extended encoding");
            CStr::from_ptr(raw).to_str().unwrap().to_owned()
        };
        let start = encoding.rfind("@?<").expect("a reply block") + 3;
        let end = encoding.rfind('>').unwrap();
        encoding[start..end].to_owned()
    }

    fn without_offsets(signature: &CStr) -> String {
        let mut types = String::new();
        let mut quoted = false;
        for c in signature.to_str().unwrap().chars() {
            if c == '"' {
                quoted = !quoted;
            }
            if quoted || !c.is_ascii_digit() {
                types.push(c);
            }
        }
        types
    }

    #[test]
    fn reply_encodings_match_the_protocol() {
        let cases: [(Sel, &CStr); 8] = [
            (
                sel!(helloWithIPCVersion:application:engine:apiVersion:reply:),
                HelloReply::ENCODING_CSTR,
            ),
            (sel!(headsetWithReply:), HeadsetReply::ENCODING_CSTR),
            (sel!(createSessionWithReply:), SessionReply::ENCODING_CSTR),
            (sel!(beginSession:reply:), StatusReply::ENCODING_CSTR),
            (sel!(endSession:reply:), StatusReply::ENCODING_CSTR),
            (sel!(destroySession:reply:), StatusReply::ENCODING_CSTR),
            (
                sel!(createSwapchainForSession:pixelFormat:width:height:arrayLength:mipmapLevelCount:usage:reply:),
                SwapchainReply::ENCODING_CSTR,
            ),
            (
                sel!(destroySwapchain:forSession:reply:),
                StatusReply::ENCODING_CSTR,
            ),
        ];
        for (sel, signature) in cases {
            assert_eq!(without_offsets(signature), reply_types(sel), "{sel}");
        }
    }
}
