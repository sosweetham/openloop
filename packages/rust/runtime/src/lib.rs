//! Tether's OpenXR runtime: the library the Khronos loader loads through the
//! runtime manifest. See `docs/ARCHITECTURE.md`.

mod dispatch;
mod extensions;
mod ffi;
mod generated;
mod instance;
mod negotiate;
mod strings;
mod system;
#[cfg(test)]
mod testing;
mod version;

pub use negotiate::xrNegotiateLoaderRuntimeInterface;
