//! The room from the headset cameras (XR_FB_passthrough), switched on from OpenLoop's menu.
//!
//! openxr-rs 0.22's passthrough wrapper is unusable here: `Passthrough::start` calls
//! xrPassthroughPauseFB, and its composition layer builder and layer handle are private. The
//! extension's functions are therefore called directly. The passthrough and its layer are created
//! running (XR_PASSTHROUGH_IS_RUNNING_AT_CREATION_BIT_FB: "the object is running at creation")
//! and destroyed to switch it off.

use openxr as xr;
use xr::sys::{self, Handle as _};

pub struct Passthrough {
    fns: xr::raw::PassthroughFB,
    feature: sys::PassthroughFB,
    layer: sys::PassthroughLayerFB,
}

fn check(result: sys::Result) -> xr::Result<()> {
    if result.into_raw() < 0 {
        Err(result)
    } else {
        Ok(())
    }
}

impl Passthrough {
    pub fn start(session: &xr::Session<xr::OpenGlEs>) -> xr::Result<Self> {
        let fns = session
            .instance()
            .exts()
            .fb_passthrough
            .ok_or(sys::Result::ERROR_EXTENSION_NOT_PRESENT)?;
        let running = sys::PassthroughFlagsFB::IS_RUNNING_AT_CREATION;
        let mut feature = sys::PassthroughFB::NULL;
        // SAFETY: a live session; the create info outlives the call; the functions belong to the
        // enabled extension.
        check(unsafe {
            (fns.create_passthrough)(
                session.as_raw(),
                &sys::PassthroughCreateInfoFB {
                    ty: sys::PassthroughCreateInfoFB::TYPE,
                    next: std::ptr::null(),
                    flags: running,
                },
                &mut feature,
            )
        })?;
        let mut layer = sys::PassthroughLayerFB::NULL;
        // SAFETY: as above, with the passthrough just created.
        let created = check(unsafe {
            (fns.create_passthrough_layer)(
                session.as_raw(),
                &sys::PassthroughLayerCreateInfoFB {
                    ty: sys::PassthroughLayerCreateInfoFB::TYPE,
                    next: std::ptr::null(),
                    passthrough: feature,
                    flags: running,
                    purpose: sys::PassthroughLayerPurposeFB::RECONSTRUCTION,
                },
                &mut layer,
            )
        });
        if let Err(e) = created {
            // SAFETY: created above and not used since.
            unsafe { (fns.destroy_passthrough)(feature) };
            return Err(e);
        }
        Ok(Self {
            fns,
            feature,
            layer,
        })
    }

    /// The composition layer for this frame, beneath the others.
    pub fn layer(&self) -> sys::CompositionLayerPassthroughFB {
        sys::CompositionLayerPassthroughFB {
            ty: sys::CompositionLayerPassthroughFB::TYPE,
            next: std::ptr::null(),
            flags: xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA,
            space: sys::Space::NULL,
            layer_handle: self.layer,
        }
    }
}

/// `layer` as the layer type `FrameStream::end` takes. openxr-rs builds its own layers the same
/// way: `CompositionLayerBase` is `#[repr(transparent)]` over `XrCompositionLayerBaseHeader`, which
/// every composition layer struct begins with.
pub fn as_base<'a>(
    layer: &'a sys::CompositionLayerPassthroughFB,
) -> &'a xr::CompositionLayerBase<'a, xr::OpenGlEs> {
    // SAFETY: see above; `layer` outlives the reference.
    unsafe { &*(layer as *const sys::CompositionLayerPassthroughFB).cast() }
}

impl Drop for Passthrough {
    fn drop(&mut self) {
        // SAFETY: both handles were created by `start` and are destroyed once, layer first.
        unsafe {
            (self.fns.destroy_passthrough_layer)(self.layer);
            (self.fns.destroy_passthrough)(self.feature);
        }
    }
}
