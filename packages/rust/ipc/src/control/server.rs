//! `openloopd`'s side: the listener, and the object it exports on each connection.

use std::cell::Cell;
use std::sync::Arc;

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{
    NSArray, NSData, NSString, NSXPCConnection, NSXPCListener, NSXPCListenerDelegate,
    NSXPCListenerEndpoint,
};
use objc2_metal::{MTLSharedEventHandle, MTLSharedTextureHandle};

use super::encoding::{HeadsetBlock, HelloBlock, SessionBlock, StatusBlock, SwapchainBlock};
use super::{Hello, Peer, Status, SwapchainDesc, Welcome, interface};
use crate::records::HeadsetInfo;
use crate::xpc::XpcObject;
use crate::{IPC_VERSION, Pod};

/// What `openloopd` implements. Calls arrive on each connection's own serial queue, never on a
/// real-time thread, and only after that connection's `hello` succeeded (except `hello`).
pub trait Service: Send + Sync + 'static {
    fn hello(&self, peer: &Peer, hello: &Hello) -> Result<Welcome, Status>;
    fn headset(&self, peer: &Peer) -> Result<HeadsetInfo, Status>;
    fn create_session(&self, peer: &Peer) -> Result<SessionGrant, Status>;
    fn begin_session(&self, peer: &Peer, session: u64) -> Status;
    fn end_session(&self, peer: &Peer, session: u64) -> Status;
    fn destroy_session(&self, peer: &Peer, session: u64) -> Status;

    fn create_swapchain(
        &self,
        _peer: &Peer,
        _session: u64,
        _desc: &SwapchainDesc,
    ) -> Result<SwapchainGrant, Status> {
        Err(Status::Unsupported)
    }

    fn destroy_swapchain(&self, _peer: &Peer, _session: u64, _swapchain: u64) -> Status {
        Status::Unsupported
    }

    /// The connection is gone, whether the app exited, crashed or closed it; called once.
    /// Anything the peer's GPU work might still wait on must be signalled from here.
    fn disconnected(&self, peer: &Peer);
}

pub struct SessionGrant {
    pub session: u64,
    /// From [`crate::SessionRegion::to_xpc`].
    pub region: XpcObject,
    pub app_ready: Option<Retained<MTLSharedEventHandle>>,
    pub compositor_done: Option<Retained<MTLSharedEventHandle>>,
}

pub struct SwapchainGrant {
    pub swapchain: u64,
    pub images: Vec<Retained<MTLSharedTextureHandle>>,
}

/// Accepts connections from processes of this user only, and serves each with `service`.
/// Invalidated on drop.
pub struct Listener {
    listener: Retained<NSXPCListener>,
    _delegate: Retained<Delegate>,
}

impl Listener {
    /// `openloopd` under launchd: `name` must be one of its plist's `MachServices`.
    pub fn mach_service(name: &str, service: Arc<dyn Service>) -> Listener {
        let listener = NSXPCListener::initWithMachServiceName(
            NSXPCListener::alloc(),
            &NSString::from_str(name),
        );
        Listener::start(listener, service)
    }

    /// Reachable only through [`Listener::endpoint`], which only an XPC message can carry to
    /// another process.
    pub fn anonymous(service: Arc<dyn Service>) -> Listener {
        Listener::start(NSXPCListener::anonymousListener(), service)
    }

    fn start(listener: Retained<NSXPCListener>, service: Arc<dyn Service>) -> Listener {
        let delegate = Delegate::new(service);
        listener.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        listener.activate();
        Listener {
            listener,
            _delegate: delegate,
        }
    }

    pub fn endpoint(&self) -> Retained<NSXPCListenerEndpoint> {
        self.listener.endpoint()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.listener.invalidate();
    }
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "OpenLoopIPCListenerDelegate"]
    #[ivars = Arc<dyn Service>]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSXPCListenerDelegate for Delegate {
        #[unsafe(method(listener:shouldAcceptNewConnection:))]
        fn should_accept(&self, _listener: &NSXPCListener, connection: &NSXPCConnection) -> bool {
            accept(self.ivars().clone(), connection)
        }
    }
);

impl Delegate {
    fn new(service: Arc<dyn Service>) -> Retained<Delegate> {
        let this = Delegate::alloc().set_ivars(service);
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }
}

fn accept(service: Arc<dyn Service>, connection: &NSXPCConnection) -> bool {
    let peer = Peer::of(connection);
    // SAFETY: no preconditions.
    if peer.euid != unsafe { libc::geteuid() } {
        return false;
    }
    connection.setExportedInterface(Some(&interface()));
    let exported = Exported::new(service.clone(), peer);
    // SAFETY: the object implements every method of the exported interface's protocol.
    unsafe { connection.setExportedObject(Some(&exported)) };
    connection.setInvalidationHandler(Some(&RcBlock::new(move || service.disconnected(&peer))));
    connection.activate();
    true
}

struct Connection {
    service: Arc<dyn Service>,
    peer: Peer,
    greeted: Cell<bool>,
}

define_class!(
    // The methods are objc/OpenLoopIPC.m's `OpenLoopdService`, with the same argument types.
    #[unsafe(super(NSObject))]
    #[name = "OpenLoopIPCExportedService"]
    #[ivars = Connection]
    struct Exported;

    unsafe impl NSObjectProtocol for Exported {}

    impl Exported {
        #[unsafe(method(helloWithIPCVersion:application:engine:apiVersion:reply:))]
        fn hello(
            &self,
            ipc_version: u32,
            application: Option<&NSString>,
            engine: Option<&NSString>,
            api_version: u64,
            reply: &HelloBlock,
        ) {
            let refuse = |status: Status| reply.call((status as u32, IPC_VERSION, 0, 0, null()));
            if ipc_version != IPC_VERSION {
                return refuse(Status::VersionMismatch);
            }
            let hello = Hello {
                ipc_version,
                application: bounded(application),
                engine: bounded(engine),
                api_version,
            };
            let connection = self.ivars();
            match connection.service.hello(&connection.peer, &hello) {
                Ok(welcome) => {
                    connection.greeted.set(true);
                    let headset = welcome.headset.map(|info| NSData::with_bytes(info.as_bytes()));
                    reply.call((
                        Status::Ok as u32,
                        IPC_VERSION,
                        welcome.registry_id,
                        welcome.boot_id,
                        headset.as_deref().map_or(null(), object),
                    ));
                }
                Err(status) => refuse(status),
            }
        }

        #[unsafe(method(headsetWithReply:))]
        fn headset(&self, reply: &HeadsetBlock) {
            let result = self.greeted().and_then(|c| c.service.headset(&c.peer));
            match result {
                Ok(info) => {
                    let data = NSData::with_bytes(info.as_bytes());
                    reply.call((Status::Ok as u32, object(&*data)));
                }
                Err(status) => reply.call((status as u32, null())),
            }
        }

        #[unsafe(method(createSessionWithReply:))]
        fn create_session(
            &self,
            reply: &SessionBlock,
        ) {
            let result = self.greeted().and_then(|c| c.service.create_session(&c.peer));
            match result {
                Ok(grant) => reply.call((
                    Status::Ok as u32,
                    grant.session,
                    grant.region.as_ptr(),
                    grant.app_ready.as_deref().map_or(null(), object),
                    grant.compositor_done.as_deref().map_or(null(), object),
                )),
                Err(status) => {
                    reply.call((status as u32, 0, std::ptr::null_mut(), null(), null()))
                }
            }
        }

        #[unsafe(method(beginSession:reply:))]
        fn begin_session(&self, session: u64, reply: &StatusBlock) {
            self.status(reply, |c| c.service.begin_session(&c.peer, session));
        }

        #[unsafe(method(endSession:reply:))]
        fn end_session(&self, session: u64, reply: &StatusBlock) {
            self.status(reply, |c| c.service.end_session(&c.peer, session));
        }

        #[unsafe(method(destroySession:reply:))]
        fn destroy_session(&self, session: u64, reply: &StatusBlock) {
            self.status(reply, |c| c.service.destroy_session(&c.peer, session));
        }

        #[unsafe(method(createSwapchainForSession:pixelFormat:width:height:arrayLength:mipmapLevelCount:usage:reply:))]
        #[allow(clippy::too_many_arguments)]
        fn create_swapchain(
            &self,
            session: u64,
            pixel_format: u64,
            width: u32,
            height: u32,
            array_length: u32,
            mipmap_level_count: u32,
            usage: u64,
            reply: &SwapchainBlock,
        ) {
            let desc =
                SwapchainDesc { pixel_format, width, height, array_length, mipmap_level_count, usage };
            let result =
                self.greeted().and_then(|c| c.service.create_swapchain(&c.peer, session, &desc));
            match result {
                Ok(grant) => {
                    let images = NSArray::from_retained_slice(&grant.images);
                    reply.call((Status::Ok as u32, grant.swapchain, object(&*images)));
                }
                Err(status) => reply.call((status as u32, 0, null())),
            }
        }

        #[unsafe(method(destroySwapchain:forSession:reply:))]
        fn destroy_swapchain(&self, swapchain: u64, session: u64, reply: &StatusBlock) {
            self.status(reply, |c| c.service.destroy_swapchain(&c.peer, session, swapchain));
        }
    }
);

impl Exported {
    fn new(service: Arc<dyn Service>, peer: Peer) -> Retained<Exported> {
        let this = Exported::alloc().set_ivars(Connection {
            service,
            peer,
            greeted: Cell::new(false),
        });
        // SAFETY: NSObject's designated initializer.
        unsafe { msg_send![super(this), init] }
    }

    fn greeted(&self) -> Result<&Connection, Status> {
        let connection = self.ivars();
        if connection.greeted.get() {
            Ok(connection)
        } else {
            Err(Status::NotGreeted)
        }
    }

    fn status(&self, reply: &StatusBlock, call: impl FnOnce(&Connection) -> Status) {
        let status = self.greeted().map_or_else(|status| status, call);
        reply.call((status as u32,));
    }
}

/// The peer is untrusted; a name only ever reaches logs and reports.
fn bounded(text: Option<&NSString>) -> String {
    text.map(|text| text.to_string().chars().take(256).collect())
        .unwrap_or_default()
}

/// A borrowed object as a reply block takes it; NSXPC encodes it before the call returns.
fn object<T: objc2::Message, U>(value: &T) -> *mut U {
    (value as *const T).cast_mut().cast()
}

fn null<T>() -> *mut T {
    std::ptr::null_mut()
}
