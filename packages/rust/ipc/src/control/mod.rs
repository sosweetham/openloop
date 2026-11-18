//! The NSXPC control plane: setup calls between the runtime and `openloopd`, never one per frame
//! (docs/ARCHITECTURE.md § Runtime ↔ openloopd). The protocol itself is objc/OpenLoopIPC.m.

mod client;
mod encoding;
mod server;

pub use client::{CallError, Client, ConnectOptions, NewSession};
pub use server::{Listener, Service, SessionGrant, SwapchainGrant};

use objc2::encode::{Encode, Encoding};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyProtocol, NSObjectProtocol, Sel};
use objc2::{ClassType, class, msg_send, sel};
use objc2_foundation::{NSSet, NSXPCConnection, NSXPCInterface};
use objc2_metal::MTLSharedTextureHandle;

use crate::records::HeadsetInfo;
use crate::xpc::{self, xpc_type_t};

unsafe extern "C" {
    // objc/OpenLoopIPC.m.
    fn openloop_ipc_openloopd_service() -> *const AnyProtocol;
}

/// `OpenLoopdService`, with the extended method encodings only clang emits.
pub fn protocol() -> &'static AnyProtocol {
    // SAFETY: the accessor returns a protocol compiled into this binary, alive forever.
    unsafe { &*openloop_ipc_openloopd_service() }
}

/// The first argument of every reply.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    /// The peer speaks another `IPC_VERSION`; the reply's version says which.
    VersionMismatch = 1,
    /// `hello` has not succeeded on this connection.
    NotGreeted = 2,
    NoHeadset = 3,
    UnknownSession = 4,
    LimitReached = 5,
    Unsupported = 6,
    Failed = 7,
}

impl Status {
    /// A status from the peer; anything this build does not know reads as `Failed`.
    pub fn from_wire(value: u32) -> Status {
        [
            Status::Ok,
            Status::VersionMismatch,
            Status::NotGreeted,
            Status::NoHeadset,
            Status::UnknownSession,
            Status::LimitReached,
            Status::Unsupported,
            Status::Failed,
        ]
        .into_iter()
        .find(|status| *status as u32 == value)
        .unwrap_or(Status::Failed)
    }
}

/// What the runtime says about itself in `hello`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub ipc_version: u32,
    /// `XrApplicationInfo::applicationName` and `engineName`.
    pub application: String,
    pub engine: String,
    /// `XrApplicationInfo::apiVersion`.
    pub api_version: u64,
}

/// What `openloopd` answers to a successful `hello`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Welcome {
    /// `MTLDevice.registryID` of `openloopd`'s GPU: shared textures only work on the device that
    /// made them, so the runtime hands the app this device.
    pub registry_id: u64,
    pub boot_id: u64,
    pub headset: Option<HeadsetInfo>,
}

/// XrSwapchainCreateInfo, as `openloopd` needs it to allocate (R2 S3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SwapchainDesc {
    /// An `MTLPixelFormat`.
    pub pixel_format: u64,
    pub width: u32,
    pub height: u32,
    pub array_length: u32,
    pub mipmap_level_count: u32,
    /// `MTLTextureUsage` bits.
    pub usage: u64,
}

/// `audit_token_t` from `<bsm/audit.h>`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditToken(pub [u32; 8]);

// SAFETY: `typedef struct { unsigned int val[8]; } audit_token_t`.
unsafe impl Encode for AuditToken {
    const ENCODING: Encoding = Encoding::Struct("?", &[Encoding::Array(8, &u32::ENCODING)]);
}

impl AuditToken {
    /// The process, and its PID version, which together never repeat (unlike a bare PID).
    pub fn pid(&self) -> (i32, u32) {
        (self.0[5] as i32, self.0[7])
    }
}

/// Who is on the other end of a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    /// May be reused once the peer exits (`<xpc/connection.h>`); prefer `audit_token`.
    pub pid: i32,
    pub euid: u32,
    /// `None` if this macOS lacks NSXPCConnection's private `auditToken` accessor.
    pub audit_token: Option<AuditToken>,
}

impl Peer {
    pub(crate) fn of(connection: &NSXPCConnection) -> Peer {
        let audit_token = connection.respondsToSelector(sel!(auditToken)).then(|| {
            // SAFETY: checked to exist above; it returns audit_token_t by value. There is no
            // public accessor (`NSXPCConnection.h`, `<xpc/connection.h>`).
            let token: AuditToken = unsafe { msg_send![connection, auditToken] };
            token
        });
        Peer {
            pid: connection.processIdentifier(),
            euid: connection.effectiveUserIdentifier(),
            audit_token,
        }
    }
}

pub(crate) fn interface() -> Retained<NSXPCInterface> {
    // SAFETY: the protocol carries clang's extended encodings, which NSXPCInterface requires.
    let interface = unsafe { NSXPCInterface::interfaceWithProtocol(protocol()) };
    set_xpc_type(
        &interface,
        xpc::shmem_type(),
        sel!(createSessionWithReply:),
        2,
        true,
    );
    let images: [&AnyClass; 2] = [class!(NSArray), MTLSharedTextureHandle::class()];
    // SAFETY: argument 2 of that reply is an NSArray of MTLSharedTextureHandle.
    unsafe {
        interface.setClasses_forSelector_argumentIndex_ofReply(
            &NSSet::from_slice(&images),
            sel!(createSwapchainForSession:pixelFormat:width:height:arrayLength:mipmapLevelCount:usage:reply:),
            2,
            true,
        );
    }
    interface
}

/// `-[NSXPCInterface setXPCType:forSelector:argumentIndex:ofReply:]` (macOS 10.15), which
/// objc2-foundation does not bind because it takes an `xpc_type_t`. Without it NSXPC refuses to
/// decode the argument.
fn set_xpc_type(
    interface: &NSXPCInterface,
    ty: xpc_type_t,
    sel: Sel,
    index: usize,
    of_reply: bool,
) {
    // SAFETY: `ty` is a libxpc type singleton and `sel` is in the interface's protocol.
    unsafe {
        let _: () = msg_send![
            interface,
            setXPCType: XpcTypeArg(ty),
            forSelector: sel,
            argumentIndex: index,
            ofReply: of_reply
        ];
    }
}

/// An `xpc_type_t` as Foundation declares it, `const struct _xpc_type_s *`.
#[repr(transparent)]
struct XpcTypeArg(xpc_type_t);

// SAFETY: a pointer, encoded as the method's own declaration.
unsafe impl Encode for XpcTypeArg {
    const ENCODING: Encoding = Encoding::Pointer(&Encoding::Struct("_xpc_type_s", &[]));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use objc2_metal::{
        MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLResource, MTLSharedEvent,
        MTLStorageMode, MTLTexture, MTLTextureType, MTLTextureUsage,
    };

    use super::*;
    use crate::IPC_VERSION;
    use crate::records::Message;
    use crate::testing::{self, TestService};

    fn connect(service: Arc<dyn Service>, timeout: Duration) -> (Listener, Client) {
        let listener = Listener::anonymous(service);
        let options = ConnectOptions {
            timeout,
            ..ConnectOptions::default()
        };
        let client = Client::from_endpoint(&listener.endpoint(), options);
        (listener, client)
    }

    fn connect_to_test_service() -> (Listener, Client) {
        connect(Arc::new(TestService::default()), testing::PATIENCE)
    }

    #[test]
    fn a_session_over_an_anonymous_listener() {
        let (_listener, client) = connect_to_test_service();
        assert!(matches!(
            client.create_session(),
            Err(CallError::Refused(Status::NotGreeted))
        ));
        let welcome = client.hello("openloop-ipc tests", "none", 1).unwrap();
        assert_eq!(welcome.headset, Some(testing::headset_info()));
        assert_eq!(client.headset().unwrap(), Some(testing::headset_info()));
        assert_eq!(client.peer().pid, std::process::id() as i32);

        let session = client.create_session().unwrap();
        assert_eq!(session.region.info().session, session.session);
        assert_eq!(session.region.info().boot_id, welcome.boot_id);
        client.begin_session(session.session).unwrap();
        let region = &session.region;
        assert_eq!(
            region.headset.latest().unwrap().value,
            testing::headset_info()
        );
        assert_eq!(
            region.state.latest().unwrap().value,
            testing::session_state()
        );
        let mut events = region.to_app.consumer();
        let first = region.to_app.pop(&mut events).unwrap().unwrap().decode();
        assert_eq!(
            first,
            Ok(Message::Focus {
                focused: true,
                at_mac_ns: 5
            })
        );

        let unknown = client.begin_session(session.session + 100);
        assert!(
            matches!(unknown, Err(CallError::Refused(Status::UnknownSession))),
            "{unknown:?}"
        );
        client.end_session(session.session).unwrap();
        client.destroy_session(session.session).unwrap();
        let gone = client.destroy_session(session.session);
        assert!(
            matches!(gone, Err(CallError::Refused(Status::UnknownSession))),
            "{gone:?}"
        );
    }

    #[test]
    fn another_ipc_version_is_refused() {
        let (_listener, client) = connect_to_test_service();
        let refused = client.hello_as(IPC_VERSION + 1, "old runtime", "none", 1);
        assert!(
            matches!(
                refused,
                Err(CallError::Version {
                    openloopd: IPC_VERSION
                })
            ),
            "{refused:?}"
        );
        let ungreeted = client.headset();
        assert!(
            matches!(ungreeted, Err(CallError::Refused(Status::NotGreeted))),
            "{ungreeted:?}"
        );
    }

    #[test]
    fn a_missing_service_fails_the_call_quickly() {
        let name = format!("dev.openloop.test.missing.{}", std::process::id());
        let client = Client::connect(&name, ConnectOptions::default());
        let started = std::time::Instant::now();
        let failed = client.hello("openloop-ipc tests", "none", 1);
        eprintln!("missing service: {failed:?} after {:?}", started.elapsed());
        assert!(
            matches!(failed, Err(CallError::Connection { code: 4099, .. })),
            "{failed:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    struct Stuck;

    impl Service for Stuck {
        fn hello(&self, _: &Peer, _: &Hello) -> Result<Welcome, Status> {
            std::thread::sleep(Duration::from_millis(500));
            Err(Status::Failed)
        }
        fn headset(&self, _: &Peer) -> Result<HeadsetInfo, Status> {
            Err(Status::NoHeadset)
        }
        fn create_session(&self, _: &Peer) -> Result<SessionGrant, Status> {
            Err(Status::Failed)
        }
        fn begin_session(&self, _: &Peer, _: u64) -> Status {
            Status::Failed
        }
        fn end_session(&self, _: &Peer, _: u64) -> Status {
            Status::Failed
        }
        fn destroy_session(&self, _: &Peer, _: u64) -> Status {
            Status::Failed
        }
        fn disconnected(&self, _: &Peer) {}
    }

    #[test]
    fn a_stuck_service_times_out_instead_of_hanging() {
        let (_listener, client) = connect(Arc::new(Stuck), Duration::from_millis(100));
        let started = std::time::Instant::now();
        let result = client.hello("openloop-ipc tests", "none", 1);
        assert!(matches!(result, Err(CallError::TimedOut)), "{result:?}");
        assert!(
            started.elapsed() < Duration::from_millis(400),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn metal_handles_cross_the_connection() {
        let Some(device) = MTLCreateSystemDefaultDevice() else {
            eprintln!("no Metal device; skipped");
            return;
        };
        let (_listener, client) = connect_to_test_service();
        let welcome = client.hello("openloop-ipc tests", "none", 1).unwrap();
        assert_eq!(welcome.registry_id, device.registryID());
        let session = client.create_session().unwrap();
        let app_ready = device.newSharedEventWithHandle(session.app_ready.as_ref().unwrap());
        app_ready.unwrap().setSignaledValue(7);
        assert!(session.compositor_done.is_some());

        let desc = SwapchainDesc {
            pixel_format: MTLPixelFormat::RGBA8Unorm_sRGB.0 as u64,
            width: 256,
            height: 128,
            array_length: 2,
            mipmap_level_count: 1,
            usage: (MTLTextureUsage::ShaderRead
                | MTLTextureUsage::RenderTarget
                | MTLTextureUsage::PixelFormatView)
                .0 as u64,
        };
        let (_, images) = client.create_swapchain(session.session, &desc).unwrap();
        assert_eq!(images.len(), 3);
        for handle in &images {
            let texture = device.newSharedTextureWithHandle(handle).unwrap();
            assert_eq!(texture.textureType(), MTLTextureType::Type2DArray);
            assert_eq!(
                (texture.width(), texture.height(), texture.arrayLength()),
                (256, 128, 2)
            );
            assert_eq!(texture.storageMode(), MTLStorageMode::Private);
        }
    }
}
