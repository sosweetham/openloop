//! The runtime's side: one connection to `tetherd`, and calls that wait at most a timeout.

use std::error::Error;
use std::ffi::c_void;
use std::fmt;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, sel};
use objc2_foundation::{
    NSArray, NSData, NSError, NSString, NSXPCConnection, NSXPCConnectionOptions,
    NSXPCListenerEndpoint,
};
use objc2_metal::{MTLSharedEventHandle, MTLSharedTextureHandle};

use super::encoding::{
    HeadsetBlock, HeadsetReply, HelloBlock, HelloReply, SessionBlock, SessionReply, StatusBlock,
    StatusReply, SwapchainBlock, SwapchainReply,
};
use super::{Peer, Status, SwapchainDesc, Welcome, interface};
use crate::records::HeadsetInfo;
use crate::region::{RegionError, SessionRegion};
use crate::xpc::XpcObject;
use crate::{IPC_VERSION, Pod};

/// Sends a protocol message to an NSXPC proxy. The proxy answers by forwarding, so its class
/// implements none of the methods and objc2's `msg_send!` would reject the send in debug
/// builds; the argument types here are the protocol's (objc/TetherIPC.m).
macro_rules! send {
    ($proxy:expr, $sel:expr $(, $argument:expr => $type:ty)* $(,)?) => {{
        let proxy: &AnyObject = $proxy;
        // SAFETY: objc_msgSend called through the exact prototype of the method.
        let untyped = objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn();
        let send: unsafe extern "C-unwind" fn(&AnyObject, Sel $(, $type)*) =
            unsafe { std::mem::transmute(untyped) };
        // SAFETY: the caller passes the selector's own argument types.
        unsafe { send(proxy, $sel $(, $argument)*) }
    }};
}

pub struct ConnectOptions {
    /// tetherd must satisfy this code-signing requirement (csreq(1) syntax) before any message
    /// is exchanged. It must parse: Foundation raises on a malformed one.
    pub code_signing_requirement: Option<String>,
    /// How long one call waits for `tetherd`: NSXPC itself never times out a call to a peer
    /// that is alive but stuck.
    pub timeout: Duration,
    /// `tetherd` exited or crashed; the next call relaunches it through launchd.
    pub on_interrupted: Option<Box<dyn Fn() + Send + Sync>>,
    /// The connection is over for good: the service does not exist, or it was invalidated.
    pub on_invalidated: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            code_signing_requirement: None,
            timeout: Duration::from_secs(2),
            on_interrupted: None,
            on_invalidated: None,
        }
    }
}

#[derive(Debug)]
pub enum CallError {
    /// `tetherd` answered no.
    Refused(Status),
    Version {
        tetherd: u32,
    },
    /// NSXPC's error: 4097 interrupted, 4099 invalid (no such service, or invalidated), 4102
    /// the code-signing requirement failed (`FoundationErrors.h`).
    Connection {
        code: isize,
        description: String,
    },
    TimedOut,
    /// `tetherd` answered with something this build cannot use.
    BadReply(&'static str),
    Region(RegionError),
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(status) => write!(f, "tetherd refused: {status:?}"),
            Self::Version { tetherd } => {
                write!(
                    f,
                    "tetherd speaks IPC version {tetherd}, this runtime {IPC_VERSION}"
                )
            }
            Self::Connection { code, description } => {
                write!(f, "connection to tetherd failed ({code}): {description}")
            }
            Self::TimedOut => write!(f, "tetherd did not answer in time"),
            Self::BadReply(what) => write!(f, "tetherd sent a bad reply: {what}"),
            Self::Region(error) => write!(f, "the session region is unusable: {error}"),
        }
    }
}

impl Error for CallError {}

pub struct NewSession {
    pub session: u64,
    pub region: SessionRegion,
    pub app_ready: Option<Retained<MTLSharedEventHandle>>,
    pub compositor_done: Option<Retained<MTLSharedEventHandle>>,
}

/// Invalidated on drop, as NSXPCConnection requires before it is deallocated.
pub struct Client {
    connection: Retained<NSXPCConnection>,
    timeout: Duration,
}

// SAFETY: moving the connection to another thread is fine: NSXPC dispatches its work onto the
// connection's own serial queue, and the handlers installed here are Send + Sync. Concurrent
// calls are not claimed safe, so the type is not Sync.
unsafe impl Send for Client {}

impl Client {
    /// `service` is a Mach service registered by a launchd plist. Connecting never fails here;
    /// a missing service fails the first call with `Connection { code: 4099 }`.
    pub fn connect(service: &str, options: ConnectOptions) -> Client {
        let connection = NSXPCConnection::initWithMachServiceName_options(
            NSXPCConnection::alloc(),
            &NSString::from_str(service),
            NSXPCConnectionOptions::empty(),
        );
        Client::start(connection, options)
    }

    pub fn from_endpoint(endpoint: &NSXPCListenerEndpoint, options: ConnectOptions) -> Client {
        let connection =
            NSXPCConnection::initWithListenerEndpoint(NSXPCConnection::alloc(), endpoint);
        Client::start(connection, options)
    }

    fn start(connection: Retained<NSXPCConnection>, options: ConnectOptions) -> Client {
        connection.setRemoteObjectInterface(Some(&interface()));
        if let Some(requirement) = &options.code_signing_requirement {
            connection.setCodeSigningRequirement(&NSString::from_str(requirement));
        }
        // The handlers are Send + Sync because they run on the connection's queue.
        if let Some(handler) = options.on_interrupted {
            connection.setInterruptionHandler(Some(&RcBlock::new(handler)));
        }
        if let Some(handler) = options.on_invalidated {
            connection.setInvalidationHandler(Some(&RcBlock::new(handler)));
        }
        connection.activate();
        Client {
            connection,
            timeout: options.timeout,
        }
    }

    /// `tetherd`'s identity as the kernel reports it; zero before the first reply.
    pub fn peer(&self) -> Peer {
        Peer::of(&self.connection)
    }

    pub fn hello(
        &self,
        application: &str,
        engine: &str,
        api_version: u64,
    ) -> Result<Welcome, CallError> {
        self.hello_as(IPC_VERSION, application, engine, api_version)
    }

    pub(crate) fn hello_as(
        &self,
        ipc_version: u32,
        application: &str,
        engine: &str,
        api_version: u64,
    ) -> Result<Welcome, CallError> {
        let application = NSString::from_str(application);
        let engine = NSString::from_str(engine);
        self.call(|proxy, pending| {
            let reply = RcBlock::with_encoding::<_, _, _, HelloReply>(
                move |status: u32,
                      version: u32,
                      registry_id: u64,
                      boot_id: u64,
                      headset: *mut NSData| {
                    pending.fill(match Status::from_wire(status) {
                        // SAFETY: NSXPC passes a valid NSData or nil for the call's duration.
                        Status::Ok => unsafe { headset_info(headset) }.map(|headset| Welcome {
                            registry_id,
                            boot_id,
                            headset,
                        }),
                        Status::VersionMismatch => Err(CallError::Version { tetherd: version }),
                        status => Err(CallError::Refused(status)),
                    })
                },
            );
            send!(
                proxy,
                sel!(helloWithIPCVersion:application:engine:apiVersion:reply:),
                ipc_version => u32,
                &application => &NSString,
                &engine => &NSString,
                api_version => u64,
                &reply => &HelloBlock,
            );
        })
    }

    /// `None` while no headset is connected.
    pub fn headset(&self) -> Result<Option<HeadsetInfo>, CallError> {
        self.call(|proxy, pending| {
            let reply = RcBlock::with_encoding::<_, _, _, HeadsetReply>(
                move |status: u32, headset: *mut NSData| {
                    pending.fill(match Status::from_wire(status) {
                        // SAFETY: as in `hello`.
                        Status::Ok => unsafe { headset_info(headset) }
                            .and_then(|info| info.ok_or(CallError::BadReply("no headset data"))),
                        status => Err(CallError::Refused(status)),
                    })
                },
            );
            send!(
                proxy,
                sel!(headsetWithReply:),
                &reply => &HeadsetBlock,
            );
        })
        .map(Some)
        .or_else(|error| match error {
            CallError::Refused(Status::NoHeadset) => Ok(None),
            error => Err(error),
        })
    }

    /// Maps the session's region and checks it is a layout this build reads.
    pub fn create_session(&self) -> Result<NewSession, CallError> {
        self.call(|proxy, pending| {
            let reply = RcBlock::with_encoding::<_, _, _, SessionReply>(
                move |status: u32,
                      session: u64,
                      region: *mut c_void,
                      app_ready: *mut AnyObject,
                      compositor_done: *mut AnyObject| {
                    pending.fill(match Status::from_wire(status) {
                        // SAFETY: NSXPC passes valid objects (or nil) of the declared types,
                        // borrowed for the call's duration.
                        Status::Ok => unsafe {
                            new_session(session, region, app_ready, compositor_done)
                        },
                        status => Err(CallError::Refused(status)),
                    })
                },
            );
            send!(
                proxy,
                sel!(createSessionWithReply:),
                &reply => &SessionBlock,
            );
        })
    }

    pub fn begin_session(&self, session: u64) -> Result<(), CallError> {
        self.status_call(|proxy, reply| {
            send!(proxy, sel!(beginSession:reply:), session => u64, reply => &StatusBlock)
        })
    }

    pub fn end_session(&self, session: u64) -> Result<(), CallError> {
        self.status_call(|proxy, reply| {
            send!(proxy, sel!(endSession:reply:), session => u64, reply => &StatusBlock)
        })
    }

    pub fn destroy_session(&self, session: u64) -> Result<(), CallError> {
        self.status_call(|proxy, reply| {
            send!(proxy, sel!(destroySession:reply:), session => u64, reply => &StatusBlock)
        })
    }

    /// R2 S3.
    pub fn create_swapchain(
        &self,
        session: u64,
        desc: &SwapchainDesc,
    ) -> Result<(u64, Vec<Retained<MTLSharedTextureHandle>>), CallError> {
        self.call(|proxy, pending| {
            let reply = RcBlock::with_encoding::<_, _, _, SwapchainReply>(
                move |status: u32, swapchain: u64, images: *mut AnyObject| {
                    pending.fill(match Status::from_wire(status) {
                        Status::Ok => {
                            // SAFETY: NSXPC decoded an NSArray holding only the classes the
                            // interface allows for this argument.
                            let images = unsafe {
                                Retained::retain(images.cast::<NSArray<MTLSharedTextureHandle>>())
                            };
                            images
                                .map(|images| (swapchain, images.to_vec()))
                                .ok_or(CallError::BadReply("no images"))
                        }
                        status => Err(CallError::Refused(status)),
                    })
                },
            );
            send!(
                proxy,
                sel!(createSwapchainForSession:pixelFormat:width:height:arrayLength:mipmapLevelCount:usage:reply:),
                session => u64,
                desc.pixel_format => u64,
                desc.width => u32,
                desc.height => u32,
                desc.array_length => u32,
                desc.mipmap_level_count => u32,
                desc.usage => u64,
                &reply => &SwapchainBlock,
            );
        })
    }

    pub fn destroy_swapchain(&self, session: u64, swapchain: u64) -> Result<(), CallError> {
        self.status_call(|proxy, reply| {
            send!(
                proxy,
                sel!(destroySwapchain:forSession:reply:),
                swapchain => u64,
                session => u64,
                reply => &StatusBlock,
            )
        })
    }

    fn status_call(&self, send: impl FnOnce(&AnyObject, &StatusBlock)) -> Result<(), CallError> {
        self.call(|proxy, pending| {
            let reply = RcBlock::with_encoding::<_, _, _, StatusReply>(move |status: u32| {
                pending.fill(match Status::from_wire(status) {
                    Status::Ok => Ok(()),
                    status => Err(CallError::Refused(status)),
                })
            });
            send(proxy, &reply);
        })
    }

    /// Sends one message through an asynchronous proxy and waits for its reply or its error,
    /// exactly one of which NSXPC delivers, on the connection's queue.
    fn call<T: 'static>(
        &self,
        send: impl FnOnce(&AnyObject, Arc<Pending<T>>),
    ) -> Result<T, CallError> {
        let pending = Arc::new(Pending {
            result: Mutex::new(None),
            ready: Condvar::new(),
        });
        let failed = pending.clone();
        let on_error = RcBlock::new(move |error: NonNull<NSError>| {
            // SAFETY: NSXPC passes a valid error.
            let error = unsafe { error.as_ref() };
            failed.fill(Err(CallError::Connection {
                code: error.code(),
                description: error.localizedDescription().to_string(),
            }));
        });
        let proxy = self.connection.remoteObjectProxyWithErrorHandler(&on_error);
        send(&proxy, pending.clone());
        pending.wait(self.timeout)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.connection.invalidate();
    }
}

/// Handed from the connection's queue to the waiting caller once.
struct Pending<T> {
    result: Mutex<Option<Result<T, CallError>>>,
    ready: Condvar,
}

// SAFETY: a value is moved exactly once, from the reply block to the caller; the objects in
// replies (NSData copies, Metal shared handles) are immutable and atomically reference counted.
unsafe impl<T> Send for Pending<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for Pending<T> {}

impl<T> Pending<T> {
    fn fill(&self, result: Result<T, CallError>) {
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if slot.is_none() {
            *slot = Some(result);
            self.ready.notify_all();
        }
    }

    fn wait(&self, timeout: Duration) -> Result<T, CallError> {
        let deadline = Instant::now() + timeout;
        let mut slot = self
            .result
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(result) = slot.take() {
                return result;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(CallError::TimedOut);
            }
            slot = self
                .ready
                .wait_timeout(slot, deadline - now)
                .unwrap_or_else(|poison| poison.into_inner())
                .0;
        }
    }
}

/// # Safety
/// `data` is null or a valid NSData.
unsafe fn headset_info(data: *mut NSData) -> Result<Option<HeadsetInfo>, CallError> {
    // SAFETY: the caller's guarantee.
    let Some(data) = (unsafe { data.as_ref() }) else {
        return Ok(None);
    };
    HeadsetInfo::from_bytes(&data.to_vec())
        .map(Some)
        .ok_or(CallError::BadReply("headset data of the wrong size"))
}

/// # Safety
/// `region` is null or a valid XPC object, the others null or valid MTLSharedEventHandles.
unsafe fn new_session(
    session: u64,
    region: *mut c_void,
    app_ready: *mut AnyObject,
    compositor_done: *mut AnyObject,
) -> Result<NewSession, CallError> {
    // SAFETY: the caller's guarantee.
    let region = unsafe { XpcObject::retain(region) }.ok_or(CallError::BadReply("no region"))?;
    let region = SessionRegion::map(&region).map_err(CallError::Region)?;
    // SAFETY: the caller's guarantee.
    let event = |object: *mut AnyObject| unsafe { Retained::retain(object.cast()) };
    Ok(NewSession {
        session,
        region,
        app_ready: event(app_ready),
        compositor_done: event(compositor_done),
    })
}
