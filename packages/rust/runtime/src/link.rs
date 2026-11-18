//! Where the runtime's headset comes from (docs/ARCHITECTURE.md § Runtime ↔ tetherd): `tetherd`,
//! over NSXPC and one shared-memory region per session; or, with `TETHER_INPROCESS=1` until
//! R2 S5, a link the runtime runs inside the app's own process as it did before R2.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use tether_ipc::control::{CallError, Client, ConnectOptions, Status as Refusal};
use tether_ipc::records::{AccessUnit, HeadsetInfo, Message as Event, SessionState};
use tether_ipc::{AuProducer, Producer, PushError, SessionRegion};
use tether_openxr_abi::XrResult;
use tether_protocol::{DeviceProfile, Haptics, Message};
use tether_stream::time::now_ns;
use tether_stream::{Headset, PIPELINE_LATENCY_NS, Tracked, UsbTransport, shared};

/// Runs the headset link inside the app's process instead of reaching `tetherd`.
pub const INPROCESS_VAR: &str = "TETHER_INPROCESS";

/// `tetherd` answers a setup call in microseconds once running and launches on demand in well
/// under a second; past this it is stuck.
const CALL_TIMEOUT: Duration = Duration::from_secs(2);
/// `tetherd` beats every session's region every 100 ms; older than this, it is gone.
const STALE_NS: i64 = 1_000_000_000;

pub enum Backend {
    InProcess(Arc<Headset>),
    Tetherd(Tetherd),
}

static BACKEND: OnceLock<Backend> = OnceLock::new();
static CONNECTING: Mutex<()> = Mutex::new(());

/// The backend the first instance chose. Every other entry point runs after an instance
/// exists, so the detached fallback is never reached in practice.
pub fn backend() -> &'static Backend {
    BACKEND.get_or_init(|| Backend::InProcess(Headset::detached()))
}

/// Picks and connects the backend on the first `xrCreateInstance`. A `tetherd` that cannot be
/// reached fails this instance only; the next one tries again.
pub fn connect(application: &str, engine: &str, api_version: u64) -> Result<(), XrResult> {
    if BACKEND.get().is_some() {
        return Ok(());
    }
    let _connecting = CONNECTING.lock().unwrap_or_else(PoisonError::into_inner);
    if BACKEND.get().is_some() {
        return Ok(());
    }
    let backend = if cfg!(test) {
        // A listening unit test would take the port from a running app or accept the user's
        // headset, and a connecting one would depend on the user's tetherd.
        Backend::InProcess(Headset::detached())
    } else if std::env::var_os(INPROCESS_VAR).is_some_and(|v| v == "1") {
        Backend::InProcess(in_process())
    } else {
        Backend::Tetherd(Tetherd::connect(application, engine, api_version)?)
    };
    let _ = BACKEND.set(backend);
    Ok(())
}

fn in_process() -> Arc<Headset> {
    match UsbTransport::bind() {
        Ok(transport) => {
            // tether-e2e reads the port from this line when it asks for any free one.
            crate::log!("headset listener on 127.0.0.1:{}", transport.port());
            Headset::start(Box::new(transport))
        }
        Err(e) => {
            crate::log!("USB listener unavailable ({e}); running without a headset");
            Headset::detached()
        }
    }
}

impl Backend {
    /// The headset's profile, or the last one's after it disconnected; `None` before any.
    pub fn profile(&self) -> Option<DeviceProfile> {
        match self {
            Backend::InProcess(headset) => headset.state().profile.clone(),
            Backend::Tetherd(tetherd) => tetherd
                .call(|client| client.headset())
                .map_err(|e| crate::log!("tetherd: {e}"))
                .ok()
                .flatten()
                .map(|info| shared::device_profile(&info)),
        }
    }

    pub fn create_session(&self) -> Result<SessionLink, XrResult> {
        match self {
            Backend::InProcess(headset) => Ok(SessionLink::InProcess(headset.clone())),
            Backend::Tetherd(tetherd) => {
                let (new, lifeline) = tetherd
                    .call_with(|client, lifeline| {
                        client.create_session().map(|new| (new, lifeline.clone()))
                    })
                    .map_err(|e| {
                        crate::log!("tetherd cannot make a session: {e}");
                        match e {
                            CallError::Refused(Refusal::LimitReached) => {
                                XrResult::XR_ERROR_LIMIT_REACHED
                            }
                            _ => XrResult::XR_ERROR_RUNTIME_FAILURE,
                        }
                    })?;
                Ok(SessionLink::Tetherd(Arc::new(TetherdSession {
                    id: new.session,
                    events: Mutex::new(new.region.to_tetherd.producer()),
                    units: Mutex::new(new.region.access_units.producer()),
                    region: new.region,
                    lifeline,
                    destroyed: AtomicBool::new(false),
                })))
            }
        }
    }
}

pub struct Tetherd {
    connection: Mutex<Connection>,
    service: String,
    application: String,
    engine: String,
    api_version: u64,
}

/// What a connection's handlers report, from the connection's own queue.
#[derive(Default)]
struct Health {
    /// Counts the times `tetherd` went away. A relaunched `tetherd` knows nothing of this
    /// process, and the one that went away dropped every session it made for it.
    losses: AtomicU64,
    /// The connection is over for good; the next call opens a new one.
    invalidated: AtomicBool,
}

struct Connection {
    client: Client,
    health: Arc<Health>,
    /// `health.losses` when `tetherd` was last greeted on this connection.
    greeted: Option<u64>,
}

/// Ties a session to the `tetherd` that made it.
#[derive(Clone)]
pub struct Lifeline {
    health: Arc<Health>,
    losses: u64,
}

impl Lifeline {
    fn lost(&self) -> bool {
        self.health.losses.load(Ordering::Acquire) != self.losses
    }

    fn is(&self, other: &Lifeline) -> bool {
        Arc::ptr_eq(&self.health, &other.health) && self.losses == other.losses
    }
}

impl Tetherd {
    fn connect(application: &str, engine: &str, api_version: u64) -> Result<Tetherd, XrResult> {
        let service = tether_ipc::service_name();
        let mut connection = Tetherd::open(&service);
        let losses = connection.health.losses.load(Ordering::Acquire);
        let welcome = connection
            .client
            .hello(application, engine, api_version)
            .map_err(|e| {
                crate::log!(
                    "tetherd ({service}) is unreachable: {e}. Install it with \
                     scripts/tetherd.sh install, or set {INPROCESS_VAR}=1 to run without it."
                );
                XrResult::XR_ERROR_RUNTIME_UNAVAILABLE
            })?;
        connection.greeted = Some(losses);
        crate::log!(
            "connected to tetherd ({service}, pid {}); {}",
            connection.client.peer().pid,
            welcome
                .headset
                .as_ref()
                .and_then(HeadsetInfo::name)
                .map_or("no headset yet".to_owned(), |name| format!(
                    "headset {name:?}"
                ))
        );
        Ok(Tetherd {
            connection: Mutex::new(connection),
            service,
            application: application.to_owned(),
            engine: engine.to_owned(),
            api_version,
        })
    }

    fn open(service: &str) -> Connection {
        let health = Arc::new(Health::default());
        let (interrupted, invalidated) = (health.clone(), health.clone());
        let options = ConnectOptions {
            code_signing_requirement: Some(tether_ipc::tetherd_requirement()),
            timeout: CALL_TIMEOUT,
            on_interrupted: Some(Box::new(move || {
                interrupted.losses.fetch_add(1, Ordering::AcqRel);
            })),
            on_invalidated: Some(Box::new(move || {
                invalidated.invalidated.store(true, Ordering::Release);
                invalidated.losses.fetch_add(1, Ordering::AcqRel);
            })),
        };
        Connection {
            client: Client::connect(service, options),
            health,
            greeted: None,
        }
    }

    fn call<T>(&self, call: impl Fn(&Client) -> Result<T, CallError>) -> Result<T, CallError> {
        self.call_with(|client, _| call(client))
    }

    /// Runs `call` against the `tetherd` serving now, greeting it first if it has not been
    /// greeted since it last went away. `call` also gets that `tetherd`'s [`Lifeline`].
    fn call_with<T>(
        &self,
        call: impl Fn(&Client, &Lifeline) -> Result<T, CallError>,
    ) -> Result<T, CallError> {
        let mut connection = self.connection();
        if connection.health.invalidated.load(Ordering::Acquire) {
            *connection = Tetherd::open(&self.service);
        }
        let losses = connection.health.losses.load(Ordering::Acquire);
        if connection.greeted != Some(losses) {
            self.greet(&mut connection)?;
        }
        match call(&connection.client, &Self::lifeline(&connection)) {
            Err(CallError::Refused(Refusal::NotGreeted)) => {
                self.greet(&mut connection)?;
                call(&connection.client, &Self::lifeline(&connection))
            }
            result => result,
        }
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn greet(&self, connection: &mut Connection) -> Result<(), CallError> {
        let losses = connection.health.losses.load(Ordering::Acquire);
        connection
            .client
            .hello(&self.application, &self.engine, self.api_version)?;
        connection.greeted = Some(losses);
        Ok(())
    }

    fn lifeline(connection: &Connection) -> Lifeline {
        Lifeline {
            health: connection.health.clone(),
            losses: connection.greeted.unwrap_or(u64::MAX),
        }
    }
}

/// One session's tie to the headset.
#[derive(Clone)]
pub enum SessionLink {
    InProcess(Arc<Headset>),
    Tetherd(Arc<TetherdSession>),
}

/// The levels a session follows; `xrPollEvent` and `xrWaitFrame` turn their changes into events.
#[derive(Clone, Copy, Debug, Default)]
pub struct Status {
    /// Video can go to the headset.
    pub linked: bool,
    pub focused: bool,
    pub recenters: u64,
    /// Grows with every keyframe the headset or `tetherd` asked of this session's encoder.
    pub keyframe_requests: u64,
}

impl SessionLink {
    pub fn tracked(&self) -> Tracked {
        match self {
            SessionLink::InProcess(headset) => headset.state().tracked.clone(),
            SessionLink::Tetherd(session) => session.tracked(),
        }
    }

    pub fn status(&self) -> Status {
        match self {
            SessionLink::InProcess(headset) => {
                let state = headset.state();
                Status {
                    linked: state.ready_for_video(),
                    focused: state.app_focused(),
                    recenters: state.recenters,
                    keyframe_requests: 0,
                }
            }
            SessionLink::Tetherd(session) => session.status(),
        }
    }

    /// The headset's frame period, which `xrWaitFrame` paces to; `None` before any headset.
    pub fn period_ns(&self) -> Option<i64> {
        match self {
            SessionLink::InProcess(headset) => headset
                .state()
                .profile
                .as_ref()
                .map(|p| (1e9 / f64::from(p.refresh_rate.max(1.0))) as i64),
            SessionLink::Tetherd(session) => session
                .region
                .timing
                .latest()
                .ok()
                .map(|timing| timing.value.period_ns),
        }
        // A rate under 1 Hz is no headset's; the default applies.
        .filter(|&period| (1..=1_000_000_000).contains(&period))
    }

    /// How long after `xrWaitFrame`'s deadline a frame is displayed: `tetherd`'s figure, which
    /// it also delays the app's sound by.
    pub fn latency_ns(&self) -> i64 {
        match self {
            SessionLink::InProcess(_) => None,
            SessionLink::Tetherd(session) => session
                .region
                .timing
                .latest()
                .ok()
                .map(|timing| timing.value.latency_ns),
        }
        .filter(|&latency| (1..=1_000_000_000).contains(&latency))
        .unwrap_or(PIPELINE_LATENCY_NS)
    }

    /// The headset's refresh rate and whether it decodes HEVC; `None` before any headset.
    pub fn display(&self) -> Option<(f32, bool)> {
        match self {
            SessionLink::InProcess(headset) => headset
                .state()
                .profile
                .as_ref()
                .map(|p| (p.refresh_rate, p.supports_hevc)),
            SessionLink::Tetherd(session) => session.region.headset.latest().ok().map(|info| {
                let info = info.value;
                (info.refresh_rate, info.flags & HeadsetInfo::HEVC != 0)
            }),
        }
    }

    /// Fire and forget: a full ring drops the command rather than block the app.
    pub fn haptic(&self, haptics: Haptics) {
        match self {
            SessionLink::InProcess(headset) => {
                headset.send(&Message::Haptics(haptics));
            }
            SessionLink::Tetherd(session) => session.push_event(Event::Haptic {
                hand: u32::from(haptics.hand),
                amplitude: haptics.amplitude,
                duration_ns: haptics.duration_ns,
                frequency_hz: haptics.frequency.max(0.0),
            }),
        }
    }

    pub fn stop_haptic(&self, hand: u8) {
        match self {
            SessionLink::InProcess(headset) => {
                headset.send(&Message::Haptics(Haptics {
                    hand,
                    amplitude: 0.0,
                    duration_ns: 0,
                    frequency: 0.0,
                }));
            }
            SessionLink::Tetherd(session) => session.push_event(Event::StopHaptic {
                hand: u32::from(hand),
            }),
        }
    }

    pub fn begin(&self) {
        if let SessionLink::Tetherd(session) = self {
            session.control("begin", |client, id| client.begin_session(id));
        }
    }

    pub fn end(&self) {
        if let SessionLink::Tetherd(session) = self {
            session.control("end", |client, id| client.end_session(id));
        }
    }

    pub fn destroy(&self) {
        if let SessionLink::Tetherd(session) = self {
            session.destroy();
        }
    }

    /// The `tetherd` that made the session went away, and its state with it.
    pub fn lost(&self) -> bool {
        match self {
            SessionLink::InProcess(_) => false,
            SessionLink::Tetherd(session) => session.lifeline.lost(),
        }
    }
}

pub struct TetherdSession {
    id: u64,
    region: SessionRegion,
    events: Mutex<Producer>,
    units: Mutex<AuProducer>,
    lifeline: Lifeline,
    destroyed: AtomicBool,
}

impl TetherdSession {
    /// `tetherd` has beaten the session's heartbeat recently.
    fn alive(&self) -> bool {
        now_ns().saturating_sub(self.region.heartbeat_mac_ns()) < STALE_NS
    }

    fn tracked(&self) -> Tracked {
        match self.region.tracking.latest() {
            Ok(sample) => shared::tracked(&sample.value, self.alive()),
            Err(_) => Tracked::default(),
        }
    }

    fn status(&self) -> Status {
        let state: SessionState = self
            .region
            .state
            .latest()
            .map(|s| s.value)
            .unwrap_or_default();
        let alive = self.alive();
        Status {
            linked: alive && state.link_up(),
            // Without `tetherd` nothing holds the menu open, so the app keeps its focus.
            focused: !alive || state.focused(),
            recenters: state.recenters,
            keyframe_requests: state.keyframe_requests,
        }
    }

    /// Copies one encoded access unit into the region and wakes `tetherd`; `false` when the
    /// ring has no room, so the frame is lost and the stream needs a keyframe.
    pub fn push_access_unit(&self, unit: &AccessUnit, data: &[u8]) -> bool {
        let mut producer = self.units.lock().unwrap_or_else(PoisonError::into_inner);
        let pushed = match self.region.access_units.reserve(&mut producer, data.len()) {
            Ok(mut writer) => writer.write(data).map(|()| writer.commit(unit)),
            Err(e) => Err(e),
        };
        match pushed {
            Ok(()) => {
                self.region.tetherd_bell.ring();
                true
            }
            Err(PushError::Full) => false,
            Err(e) => {
                crate::log!("dropping a {}-byte access unit: {e:?}", data.len());
                false
            }
        }
    }

    fn push_event(&self, event: Event) {
        let mut producer = self.events.lock().unwrap_or_else(PoisonError::into_inner);
        if self
            .region
            .to_tetherd
            .push(&mut producer, &event.into())
            .is_ok()
        {
            self.region.tetherd_bell.ring();
        }
    }

    /// A session whose `tetherd` went away is never named to the next one, which numbers its
    /// own sessions afresh.
    fn control(&self, what: &str, call: impl Fn(&Client, u64) -> Result<(), CallError>) {
        if self.lifeline.lost() {
            return;
        }
        if let Backend::Tetherd(tetherd) = backend()
            && let Err(e) = tetherd.call_with(|client, serving| {
                if serving.is(&self.lifeline) {
                    call(client, self.id)
                } else {
                    Ok(())
                }
            })
        {
            crate::log!("tetherd did not {what} session {}: {e}", self.id);
        }
    }

    fn destroy(&self) {
        if !self.destroyed.swap(true, Ordering::AcqRel) {
            self.control("destroy", |client, id| client.destroy_session(id));
        }
    }
}

impl Drop for TetherdSession {
    fn drop(&mut self) {
        self.destroy();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_is_lost_with_the_tetherd_that_made_it() {
        let health = Arc::new(Health::default());
        let made = Lifeline {
            health: health.clone(),
            losses: 0,
        };
        assert!(!made.lost());
        assert!(made.is(&made.clone()));

        health.losses.fetch_add(1, Ordering::AcqRel);
        assert!(made.lost(), "tetherd went away");
        let serving = Lifeline {
            health: health.clone(),
            losses: 1,
        };
        assert!(!serving.lost());
        assert!(
            !made.is(&serving),
            "the relaunched tetherd is never asked about the lost session"
        );
        let reopened = Lifeline {
            health: Arc::new(Health::default()),
            losses: 0,
        };
        assert!(!made.is(&reopened), "nor is one on a new connection");
    }
}
