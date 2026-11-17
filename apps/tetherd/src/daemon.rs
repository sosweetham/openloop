//! The NSXPC service apps' runtimes call, and the publishing of the headset's state into every
//! session's region (docs/ARCHITECTURE.md § Runtime ↔ tetherd).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use objc2_metal::{MTLCreateSystemDefaultDevice, MTLDevice};
use tether_ipc::control::{Hello, Peer, Service, SessionGrant, Status, Welcome};
use tether_ipc::now_ns;
use tether_ipc::records::HeadsetInfo;
use tether_stream::{Change, HeadsetState, Observer, shared};

use crate::audio::Audio;
use crate::log;
use crate::session::{Owner, Phase, Session, spawn_forwarder};
use crate::stage::Stage;

/// The beat a runtime watches to tell a live `tetherd` from a gone one.
const HEARTBEAT: Duration = Duration::from_millis(100);
const SESSIONS_PER_APP: usize = 4;
const SESSIONS: usize = 16;

/// Every session's region, kept current from the link thread, and which session is in front.
#[derive(Default)]
pub struct Hub {
    sessions: Mutex<Vec<Arc<Session>>>,
    /// The session begun last, 0 for none: its video is what the headset shows.
    foreground: AtomicU64,
}

impl Hub {
    fn sessions(&self) -> MutexGuard<'_, Vec<Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn foreground(&self) -> Option<u64> {
        Some(self.foreground.load(Ordering::Acquire)).filter(|&id| id != 0)
    }

    pub(crate) fn bring_forward(&self, session: u64) {
        self.foreground.store(session, Ordering::Release);
    }

    /// Clears the foreground if it is still `session`.
    fn step_back(&self, session: u64) {
        let _ = self
            .foreground
            .compare_exchange(session, 0, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Beats every session's heartbeat, forever, on its own thread.
    pub fn start_heartbeat(self: &Arc<Self>) {
        let hub = self.clone();
        let spawned = thread::Builder::new()
            .name("tetherd-heartbeat".into())
            .spawn(move || {
                loop {
                    let now = now_ns();
                    for session in hub.sessions().iter() {
                        session.region().beat(now);
                    }
                    thread::sleep(HEARTBEAT);
                }
            });
        if let Err(e) = spawned {
            log!("cannot start the heartbeat: {e}");
        }
    }
}

impl Observer for Hub {
    fn changed(&self, state: &HeadsetState, change: Change) {
        match change {
            Change::Linked => log!("headset connected"),
            Change::Unlinked => log!("headset disconnected"),
            Change::Profile => {
                if let Some(p) = &state.profile {
                    log!(
                        "headset is {:?}: {}x{} per eye at {} Hz, {}",
                        p.name,
                        p.eye_width,
                        p.eye_height,
                        p.refresh_rate,
                        if p.supports_hevc { "HEVC" } else { "H.264" }
                    );
                }
            }
            Change::Refused { version } => log!(
                "refused a headset speaking protocol {version}, not {}: install this checkout's \
                 Quest app (scripts/quest-apk.sh --install)",
                tether_protocol::PROTOCOL_VERSION
            ),
            Change::Recenter => log!("headset recentred"),
            Change::Focus => log!(
                "apps {} input focus",
                if state.app_focused() { "have" } else { "lose" }
            ),
            Change::Tracking | Change::Clock | Change::KeyframeRequest => {}
        }
        let foreground = self.foreground();
        for session in self.sessions().iter() {
            session.reflect(state, change, Some(session.id) == foreground);
        }
    }
}

/// What an app said about itself in `hello`.
struct Client {
    app: String,
    engine: String,
}

pub struct Daemon {
    hub: Arc<Hub>,
    stage: Arc<Stage>,
    audio: Audio,
    clients: Mutex<HashMap<Owner, Client>>,
    boot_id: u64,
    registry_id: u64,
    next_session: AtomicU64,
    reports: PathBuf,
}

impl Daemon {
    pub fn new(hub: Arc<Hub>, stage: Arc<Stage>, audio: Audio, reports: PathBuf) -> Daemon {
        let registry_id = MTLCreateSystemDefaultDevice().map_or(0, |device| device.registryID());
        Daemon {
            hub,
            stage,
            audio,
            clients: Mutex::default(),
            boot_id: now_ns() as u64 ^ (u64::from(std::process::id()) << 40),
            registry_id,
            next_session: AtomicU64::new(1),
            reports,
        }
    }

    fn clients(&self) -> MutexGuard<'_, HashMap<Owner, Client>> {
        self.clients.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The last headset's profile, kept after its link drops so apps size for it.
    fn headset_info(&self) -> Option<HeadsetInfo> {
        let state = self.stage.headset.state();
        let profile = state.profile.as_ref()?;
        Some(shared::headset_info(profile, state.greeted()))
    }

    fn find(&self, peer: &Peer, session: u64) -> Option<Arc<Session>> {
        let owner = owner(peer);
        self.hub
            .sessions()
            .iter()
            .find(|s| s.id == session && s.owner == owner)
            .cloned()
    }

    fn remove(&self, session: &Arc<Session>) {
        self.hub.sessions().retain(|s| !Arc::ptr_eq(s, session));
        self.leave(session, Phase::Destroyed);
    }

    /// The session's forwarder ends its stream once it sees the phase.
    fn leave(&self, session: &Session, phase: Phase) {
        self.hub.step_back(session.id);
        self.audio.release(session.id);
        session.set_phase(phase);
    }
}

fn owner(peer: &Peer) -> Owner {
    Owner {
        pid: peer.pid,
        pid_version: peer.audit_token.map_or(0, |token| token.pid().1),
    }
}

impl Service for Daemon {
    fn hello(&self, peer: &Peer, hello: &Hello) -> Result<Welcome, Status> {
        log!(
            "{} (pid {}, {}) connected, OpenXR {}.{}",
            hello.application,
            peer.pid,
            if hello.engine.is_empty() {
                "no engine"
            } else {
                &hello.engine
            },
            hello.api_version >> 48,
            (hello.api_version >> 32) & 0xffff
        );
        self.clients().insert(
            owner(peer),
            Client {
                app: hello.application.clone(),
                engine: hello.engine.clone(),
            },
        );
        Ok(Welcome {
            registry_id: self.registry_id,
            boot_id: self.boot_id,
            headset: self.headset_info(),
        })
    }

    fn headset(&self, _peer: &Peer) -> Result<HeadsetInfo, Status> {
        self.headset_info().ok_or(Status::NoHeadset)
    }

    fn create_session(&self, peer: &Peer) -> Result<SessionGrant, Status> {
        let owner = owner(peer);
        let (app, engine) = self
            .clients()
            .get(&owner)
            .map(|c| (c.app.clone(), c.engine.clone()))
            .unwrap_or_default();
        {
            let sessions = self.hub.sessions();
            let mine = sessions.iter().filter(|s| s.owner == owner).count();
            if mine >= SESSIONS_PER_APP || sessions.len() >= SESSIONS {
                return Err(Status::LimitReached);
            }
        }
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let audio_pid = peer.audit_token.map(|token| token.pid().0);
        let session =
            Session::new(id, owner, app, engine, audio_pid, self.boot_id).map_err(|e| {
                log!("cannot make a session region: {e}");
                Status::Failed
            })?;
        let region = session.region().to_xpc().map_err(|e| {
            log!("cannot share a session region: {e}");
            Status::Failed
        })?;
        {
            // Under the state lock, so no change on the link thread falls between the first
            // publish and the session joining the list.
            let state = self.stage.headset.state();
            session.reflect_all(&state);
            self.hub.sessions().push(session.clone());
        }
        spawn_forwarder(session.clone(), self.stage.clone(), self.reports.clone());
        log!("session {id} for {} (pid {})", session.app, peer.pid);
        Ok(SessionGrant {
            session: id,
            region,
            app_ready: None,
            compositor_done: None,
        })
    }

    fn begin_session(&self, peer: &Peer, session: u64) -> Status {
        let Some(found) = self.find(peer, session) else {
            return Status::UnknownSession;
        };
        self.hub.bring_forward(session);
        found.set_phase(Phase::Running);
        log!(
            "session {session} ({}) began; its video goes to the headset",
            found.app
        );
        self.audio.follow(found);
        Status::Ok
    }

    fn end_session(&self, peer: &Peer, session: u64) -> Status {
        let Some(found) = self.find(peer, session) else {
            return Status::UnknownSession;
        };
        self.leave(&found, Phase::Ended);
        log!("session {session} ({}) ended", found.app);
        Status::Ok
    }

    fn destroy_session(&self, peer: &Peer, session: u64) -> Status {
        let Some(found) = self.find(peer, session) else {
            return Status::UnknownSession;
        };
        self.remove(&found);
        log!("session {session} ({}) destroyed", found.app);
        Status::Ok
    }

    fn disconnected(&self, peer: &Peer) {
        let owner = owner(peer);
        let client = self.clients().remove(&owner);
        let orphans: Vec<Arc<Session>> = self
            .hub
            .sessions()
            .iter()
            .filter(|s| s.owner == owner)
            .cloned()
            .collect();
        for session in &orphans {
            self.remove(session);
        }
        log!(
            "{} (pid {}) disconnected{}",
            client.map_or("an app".to_owned(), |c| c.app),
            peer.pid,
            match orphans.len() {
                0 => String::new(),
                n => format!("; dropped its {n} session(s)"),
            }
        );
    }
}
