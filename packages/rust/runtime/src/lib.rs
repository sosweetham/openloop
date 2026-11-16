//! Tether's OpenXR runtime: the library the Khronos loader loads through the
//! runtime manifest. See `docs/ARCHITECTURE.md`.

mod actions;
mod convert;
mod dispatch;
mod extensions;
mod ffi;
mod frame;
mod generated;
mod handles;
mod instance;
mod negotiate;
mod session;
mod space;
mod state;
mod strings;
mod swapchain;
mod system;
#[cfg(test)]
mod testing;
mod version;

pub use negotiate::xrNegotiateLoaderRuntimeInterface;
