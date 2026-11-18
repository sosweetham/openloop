//! The audio control thread: the only place a HAL call is made. Each call can stall for tens of
//! seconds inside coreaudiod, so none may run on a real-time, frame or IPC thread. Calls are
//! timed; one that runs into the HAL's own RPC timeout, or IO that never delivers a callback,
//! blocks capture for good (see `latch`), and nothing is retried or torn down after that.

use std::mem;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::events::{BlockReason, EndReason, Event, HalCall, RestartReason};
use crate::ioproc::IoContext;
use crate::latch::Latch;
use crate::ring::CaptureRing;
use crate::time::Timebase;

pub(crate) type ObjectId = u32;
pub(crate) const SYSTEM_OBJECT: ObjectId = 1;

const TICK: Duration = Duration::from_millis(50);
const WAITING_POLL_NS: i64 = 1_000_000_000;
const LOSS_REPORT_NS: i64 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Status(pub i32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ListenerKey(pub u32);

/// A property the control thread listens to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Watch {
    ProcessList,
    ServiceRestarted,
    TapFormat,
    SampleRate,
    DeviceAlive,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TapFormat {
    pub sample_rate: f64,
    pub channels: u32,
    pub float32: bool,
}

impl TapFormat {
    /// The rate, when this is a format the IOProc reads: Float32 linear PCM at a whole rate.
    pub fn readable_rate(&self) -> Option<u32> {
        let rate = self.sample_rate;
        (self.float32
            && self.channels >= 1
            && rate.fract() == 0.0
            && (8_000.0..=384_000.0).contains(&rate))
        .then_some(rate as u32)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TapObject {
    pub id: ObjectId,
    pub uid: String,
}

/// Core Audio as the control thread uses it; tests substitute a fake.
pub(crate) trait Hal {
    type Proc: Copy;
    /// `XrTime` now. Times every call, so a fake can stall without waiting.
    fn now_ns(&self) -> i64;
    fn process_exists(&self, pid: i32) -> bool;
    /// 0 when the process has no process object.
    fn translate_pid(&mut self, pid: i32) -> Result<ObjectId, Status>;
    fn create_tap(&mut self, process: ObjectId) -> Result<TapObject, Status>;
    fn tap_format(&mut self, tap: ObjectId) -> Result<TapFormat, Status>;
    fn destroy_tap(&mut self, tap: ObjectId) -> Result<(), Status>;
    fn create_aggregate(&mut self, tap_uid: &str, pid: i32) -> Result<ObjectId, Status>;
    fn device_alive(&mut self, device: ObjectId) -> Result<bool, Status>;
    fn destroy_aggregate(&mut self, device: ObjectId) -> Result<(), Status>;
    fn create_io_proc(
        &mut self,
        device: ObjectId,
        context: NonNull<IoContext>,
    ) -> Result<Self::Proc, Status>;
    fn start(&mut self, device: ObjectId, proc: Self::Proc) -> Result<(), Status>;
    fn stop(&mut self, device: ObjectId, proc: Self::Proc) -> Result<(), Status>;
    fn destroy_io_proc(&mut self, device: ObjectId, proc: Self::Proc) -> Result<(), Status>;
    fn add_listener(&mut self, object: ObjectId, watch: Watch) -> Result<ListenerKey, Status>;
    fn remove_listener(&mut self, key: ListenerKey) -> Result<(), Status>;
    /// Drops the registrations a restarted audio server has lost, without a HAL call.
    fn forget_listeners(&mut self);
}

pub(crate) enum Command {
    Capture(i32),
    Stop,
    Changed(Watch),
    Shutdown,
}

pub(crate) struct Settings {
    pub first_callback_timeout: Duration,
    pub stall_threshold: Duration,
    pub slow_call: Duration,
    pub marker: Option<PathBuf>,
}

/// Why a run of HAL calls stopped early. `Failed` has already been reported; after `Halted`
/// capture is blocked and no further call may be made.
enum Abort {
    Failed,
    Halted,
}

struct Halted;

/// Times every HAL call and turns a stall into a block.
struct Calls<H> {
    hal: H,
    events: Sender<Event>,
    latch: &'static Latch,
    stall_ns: i64,
    slow_ns: i64,
    marker: Option<PathBuf>,
}

impl<H: Hal> Calls<H> {
    fn call<T>(
        &mut self,
        pid: i32,
        call: HalCall,
        f: impl FnOnce(&mut H) -> Result<T, Status>,
    ) -> Result<T, Abort> {
        let start = self.hal.now_ns();
        let result = f(&mut self.hal);
        let elapsed = self.hal.now_ns().saturating_sub(start);
        if elapsed >= self.stall_ns {
            self.block(BlockReason::Stalled {
                call,
                elapsed_ms: millis(elapsed),
            });
            return Err(Abort::Halted);
        }
        if elapsed >= self.slow_ns {
            self.emit(Event::SlowCall {
                call,
                elapsed_ms: millis(elapsed),
            });
        }
        result.map_err(|status| {
            self.emit(Event::Failed {
                pid,
                call,
                status: status.0,
            });
            Abort::Failed
        })
    }

    /// A teardown step: a failure is reported and the teardown goes on.
    fn teardown(
        &mut self,
        pid: i32,
        call: HalCall,
        f: impl FnOnce(&mut H) -> Result<(), Status>,
    ) -> Result<bool, Halted> {
        match self.call(pid, call, f) {
            Ok(()) => Ok(true),
            Err(Abort::Failed) => Ok(false),
            Err(Abort::Halted) => Err(Halted),
        }
    }

    fn block(&mut self, reason: BlockReason) {
        if let Err(error) = self.latch.block(&reason, self.marker.as_deref()) {
            eprintln!("audiotap: could not record the capture block: {error}");
        }
        self.emit(Event::Blocked(reason));
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

struct Session<P> {
    pid: i32,
    process: ObjectId,
    tap: TapObject,
    format: TapFormat,
    sample_rate: u32,
    tap_listener: Option<ListenerKey>,
    io: Option<Io<P>>,
}

struct Io<P> {
    aggregate: ObjectId,
    listeners: Vec<ListenerKey>,
    proc: Option<P>,
    started: bool,
    started_ns: i64,
    receiving: bool,
    reported_unreadable: u64,
    next_unreadable_report_ns: i64,
    context: NonNull<IoContext>,
}

enum State<P> {
    Idle,
    Waiting { pid: i32 },
    Running(Session<P>),
    Blocked,
}

pub(crate) struct Controller<H: Hal> {
    calls: Calls<H>,
    ring: Arc<CaptureRing>,
    timebase: Timebase,
    first_callback_ns: i64,
    state: State<H::Proc>,
    system_listeners: Vec<ListenerKey>,
    next_epoch: u32,
    next_poll_ns: i64,
    reported_drops: u64,
    next_overrun_report_ns: i64,
}

impl<H: Hal> Controller<H> {
    pub fn new(
        hal: H,
        ring: Arc<CaptureRing>,
        events: Sender<Event>,
        latch: &'static Latch,
        settings: Settings,
        first_epoch: u32,
    ) -> Controller<H> {
        if let Some(marker) = &settings.marker {
            latch.load_marker(marker);
        }
        Controller {
            calls: Calls {
                hal,
                events,
                latch,
                stall_ns: nanos(settings.stall_threshold),
                slow_ns: nanos(settings.slow_call),
                marker: settings.marker,
            },
            ring,
            timebase: Timebase::host(),
            first_callback_ns: nanos(settings.first_callback_timeout),
            state: State::Idle,
            system_listeners: Vec::new(),
            next_epoch: first_epoch.max(1),
            next_poll_ns: 0,
            reported_drops: 0,
            next_overrun_report_ns: 0,
        }
    }

    pub fn run(mut self, commands: Receiver<Command>) {
        loop {
            match commands.recv_timeout(TICK) {
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.tick();
        }
        self.shutdown();
    }

    pub fn handle(&mut self, command: Command) {
        match command {
            Command::Capture(pid) => self.capture(pid),
            Command::Stop => self.end(EndReason::Stopped),
            Command::Changed(watch) => self.changed(watch),
            Command::Shutdown => self.shutdown(),
        }
    }

    pub fn shutdown(&mut self) {
        self.end(EndReason::Stopped);
        if matches!(self.state, State::Blocked) {
            return;
        }
        for key in mem::take(&mut self.system_listeners) {
            if self
                .calls
                .teardown(0, HalCall::RemoveListener, |h| h.remove_listener(key))
                .is_err()
            {
                self.state = State::Blocked;
                return;
            }
        }
    }

    fn capture(&mut self, pid: i32) {
        if let Some(reason) = self.calls.latch.blocked() {
            self.calls.emit(Event::Refused { pid, reason });
            return;
        }
        self.end(EndReason::Replaced);
        if let Some(reason) = self.calls.latch.blocked() {
            self.calls.emit(Event::Refused { pid, reason });
            return;
        }
        if self.watch_system().is_err() {
            self.state = State::Blocked;
            return;
        }
        self.find(pid);
    }

    fn watch_system(&mut self) -> Result<(), Halted> {
        if !self.system_listeners.is_empty() {
            return Ok(());
        }
        for watch in [Watch::ProcessList, Watch::ServiceRestarted] {
            match self.calls.call(0, HalCall::AddListener, |h| {
                h.add_listener(SYSTEM_OBJECT, watch)
            }) {
                Ok(key) => self.system_listeners.push(key),
                Err(Abort::Failed) => {}
                Err(Abort::Halted) => return Err(Halted),
            }
        }
        Ok(())
    }

    /// Looks the process up; one that has not used audio yet is waited for.
    fn find(&mut self, pid: i32) {
        let waiting = matches!(self.state, State::Waiting { pid: p } if p == pid);
        self.state = State::Idle;
        match self
            .calls
            .call(pid, HalCall::TranslatePid, |h| h.translate_pid(pid))
        {
            Ok(0) if !self.calls.hal.process_exists(pid) => {
                self.calls.emit(Event::Ended {
                    pid,
                    reason: EndReason::ProcessExited,
                });
            }
            Ok(0) => {
                if !waiting {
                    self.calls.emit(Event::WaitingForProcess { pid });
                }
                self.state = State::Waiting { pid };
            }
            Ok(process) => self.open(pid, process, None),
            Err(Abort::Failed) => self.calls.emit(Event::Ended {
                pid,
                reason: EndReason::Failed,
            }),
            Err(Abort::Halted) => self.state = State::Blocked,
        }
    }

    fn open(&mut self, pid: i32, process: ObjectId, restart: Option<RestartReason>) {
        self.state = State::Idle;
        let tap = match self
            .calls
            .call(pid, HalCall::CreateTap, |h| h.create_tap(process))
        {
            Ok(tap) => tap,
            Err(Abort::Failed) => {
                return self.calls.emit(Event::Ended {
                    pid,
                    reason: EndReason::Failed,
                });
            }
            Err(Abort::Halted) => return self.state = State::Blocked,
        };
        let mut session = Session {
            pid,
            process,
            tap,
            format: TapFormat::default(),
            sample_rate: 0,
            tap_listener: None,
            io: None,
        };
        match self.configure(&mut session) {
            Ok(()) => {
                let sample_rate = session.sample_rate;
                self.calls.emit(match restart {
                    Some(reason) => Event::Restarted {
                        pid,
                        reason,
                        sample_rate,
                    },
                    None => Event::Started { pid, sample_rate },
                });
                self.state = State::Running(session);
            }
            Err(Abort::Failed) => self.close_after_failure(session),
            Err(Abort::Halted) => self.abandon(session),
        }
    }

    fn configure(&mut self, session: &mut Session<H::Proc>) -> Result<(), Abort> {
        let (pid, tap) = (session.pid, session.tap.id);
        let format = self
            .calls
            .call(pid, HalCall::ReadTapFormat, |h| h.tap_format(tap))?;
        session.sample_rate = self.readable(pid, &format)?;
        session.format = format;
        session.tap_listener = self.listen(pid, tap, Watch::TapFormat)?;
        self.start_io(session)
    }

    fn readable(&mut self, pid: i32, format: &TapFormat) -> Result<u32, Abort> {
        format.readable_rate().ok_or_else(|| {
            self.calls.emit(Event::UnsupportedFormat {
                pid,
                sample_rate: format.sample_rate,
                channels: format.channels,
                float32: format.float32,
            });
            Abort::Failed
        })
    }

    fn listen(
        &mut self,
        pid: i32,
        object: ObjectId,
        watch: Watch,
    ) -> Result<Option<ListenerKey>, Abort> {
        match self
            .calls
            .call(pid, HalCall::AddListener, |h| h.add_listener(object, watch))
        {
            Ok(key) => Ok(Some(key)),
            Err(Abort::Failed) => Ok(None),
            Err(Abort::Halted) => Err(Abort::Halted),
        }
    }

    /// The tap-only aggregate device and the IOProc, started.
    fn start_io(&mut self, session: &mut Session<H::Proc>) -> Result<(), Abort> {
        let pid = session.pid;
        let uid = session.tap.uid.clone();
        let aggregate = self.calls.call(pid, HalCall::CreateAggregate, |h| {
            h.create_aggregate(&uid, pid)
        })?;
        let epoch = self.next_epoch;
        self.next_epoch = self.next_epoch.checked_add(1).unwrap_or(1);
        let context = IoContext::new(self.ring.clone(), self.timebase, epoch, session.sample_rate);
        let io = session.io.insert(Io {
            aggregate,
            listeners: Vec::new(),
            proc: None,
            started: false,
            started_ns: 0,
            receiving: false,
            reported_unreadable: 0,
            next_unreadable_report_ns: 0,
            context: NonNull::from(Box::leak(Box::new(context))),
        });
        for watch in [Watch::DeviceAlive, Watch::SampleRate] {
            if let Some(key) = self.listen(pid, aggregate, watch)? {
                io.listeners.push(key);
            }
        }
        let context = io.context;
        let proc = self.calls.call(pid, HalCall::CreateIoProc, |h| {
            h.create_io_proc(aggregate, context)
        })?;
        io.proc = Some(proc);
        self.calls
            .call(pid, HalCall::StartIo, |h| h.start(aggregate, proc))?;
        io.started = true;
        io.started_ns = self.calls.hal.now_ns();
        Ok(())
    }

    /// Ends whatever is current: IOProc, then aggregate device, then tap.
    fn end(&mut self, reason: EndReason) {
        match mem::replace(&mut self.state, State::Idle) {
            State::Idle => {}
            State::Blocked => self.state = State::Blocked,
            State::Waiting { pid } => self.calls.emit(Event::Ended { pid, reason }),
            State::Running(session) => {
                let pid = session.pid;
                match self.close(session) {
                    Ok(()) => self.calls.emit(Event::Ended { pid, reason }),
                    Err(Halted) => {
                        self.state = State::Blocked;
                        self.calls.emit(Event::Ended {
                            pid,
                            reason: EndReason::Blocked,
                        });
                    }
                }
            }
        }
    }

    fn close_after_failure(&mut self, session: Session<H::Proc>) {
        let pid = session.pid;
        let reason = match self.close(session) {
            Ok(()) => EndReason::Failed,
            Err(Halted) => {
                self.state = State::Blocked;
                EndReason::Blocked
            }
        };
        self.calls.emit(Event::Ended { pid, reason });
    }

    fn close(&mut self, mut session: Session<H::Proc>) -> Result<(), Halted> {
        let pid = session.pid;
        if let Some(io) = session.io.take() {
            self.close_io(pid, io)?;
        }
        if let Some(key) = session.tap_listener.take() {
            self.calls
                .teardown(pid, HalCall::RemoveListener, |h| h.remove_listener(key))?;
        }
        let tap = session.tap.id;
        self.calls
            .teardown(pid, HalCall::DestroyTap, |h| h.destroy_tap(tap))?;
        Ok(())
    }

    fn close_io(&mut self, pid: i32, io: Io<H::Proc>) -> Result<(), Halted> {
        self.retire(&io);
        for key in io.listeners {
            self.calls
                .teardown(pid, HalCall::RemoveListener, |h| h.remove_listener(key))?;
        }
        let aggregate = io.aggregate;
        let freeable = match io.proc {
            Some(proc) => {
                if io.started {
                    self.calls
                        .teardown(pid, HalCall::StopIo, |h| h.stop(aggregate, proc))?;
                }
                self.calls.teardown(pid, HalCall::DestroyIoProc, |h| {
                    h.destroy_io_proc(aggregate, proc)
                })?
            }
            None => true,
        };
        if freeable {
            // SAFETY: the IOProc was destroyed, or never registered, so nothing holds the context.
            drop(unsafe { Box::from_raw(io.context.as_ptr()) });
        }
        self.calls.teardown(pid, HalCall::DestroyAggregate, |h| {
            h.destroy_aggregate(aggregate)
        })?;
        Ok(())
    }

    /// Stops the IOProc writing and ends its stream in the ring; no HAL call.
    fn retire(&self, io: &Io<H::Proc>) {
        // SAFETY: a context is freed only by close_io, after this.
        let context = unsafe { io.context.as_ref() };
        context.retire();
        self.ring.close(context.epoch);
    }

    /// Leaves every HAL object of the session in place and its context allocated. Used when a
    /// prompt may be pending (any call could queue behind it; coreaudiod reclaims a dead
    /// client's private objects) and after the audio server restarted (the IDs are stale).
    fn forget(&mut self, session: Session<H::Proc>) {
        if let Some(io) = &session.io {
            self.retire(io);
        }
    }

    fn abandon(&mut self, session: Session<H::Proc>) {
        self.forget(session);
        self.state = State::Blocked;
    }

    fn halt(&mut self) {
        match mem::replace(&mut self.state, State::Blocked) {
            State::Running(session) => self.abandon(session),
            _ => self.state = State::Blocked,
        }
    }

    fn changed(&mut self, watch: Watch) {
        match watch {
            Watch::ServiceRestarted => self.service_restarted(),
            Watch::ProcessList => match &self.state {
                State::Waiting { pid } => {
                    let pid = *pid;
                    self.find(pid);
                }
                State::Running(session) => {
                    let (pid, process) = (session.pid, session.process);
                    self.check_process(pid, process);
                }
                _ => {}
            },
            Watch::TapFormat | Watch::SampleRate | Watch::DeviceAlive => self.check_io(watch),
        }
    }

    fn check_process(&mut self, pid: i32, process: ObjectId) {
        match self
            .calls
            .call(pid, HalCall::TranslatePid, |h| h.translate_pid(pid))
        {
            Ok(current) if current == process => {}
            Ok(_) => self.end(EndReason::ProcessExited),
            Err(Abort::Failed) => {}
            Err(Abort::Halted) => self.halt(),
        }
    }

    fn check_io(&mut self, watch: Watch) {
        let State::Running(session) = &self.state else {
            return;
        };
        let (pid, tap, current) = (session.pid, session.tap.id, session.format.clone());
        let aggregate = session.io.as_ref().map(|io| io.aggregate);
        let (reason, format) = if watch == Watch::DeviceAlive {
            let Some(aggregate) = aggregate else {
                return;
            };
            match self
                .calls
                .call(pid, HalCall::ReadDeviceAlive, |h| h.device_alive(aggregate))
            {
                Ok(true) | Err(Abort::Failed) => return,
                Ok(false) => (RestartReason::DeviceDied, None),
                Err(Abort::Halted) => return self.halt(),
            }
        } else {
            match self
                .calls
                .call(pid, HalCall::ReadTapFormat, |h| h.tap_format(tap))
            {
                Ok(format) if format == current => return,
                Ok(format) => (RestartReason::FormatChanged, Some(format)),
                Err(Abort::Failed) => return,
                Err(Abort::Halted) => return self.halt(),
            }
        };
        self.rebuild(reason, format);
    }

    /// A new aggregate device and IOProc over the same tap. Only once the permission is proven:
    /// until then a second IOProc registration might queue behind a pending prompt.
    fn rebuild(&mut self, reason: RestartReason, format: Option<TapFormat>) {
        if !self.calls.latch.granted() {
            return self.end(match reason {
                RestartReason::FormatChanged => EndReason::FormatChanged,
                RestartReason::DeviceDied => EndReason::DeviceDied,
                RestartReason::ServiceRestarted => EndReason::ServiceRestarted,
            });
        }
        let State::Running(mut session) = mem::replace(&mut self.state, State::Idle) else {
            return;
        };
        let pid = session.pid;
        if let Some(io) = session.io.take()
            && self.close_io(pid, io).is_err()
        {
            return self.abandon(session);
        }
        if let Some(format) = format {
            match self.readable(pid, &format) {
                Ok(rate) => {
                    session.sample_rate = rate;
                    session.format = format;
                }
                Err(_) => return self.close_after_failure(session),
            }
        }
        match self.start_io(&mut session) {
            Ok(()) => {
                let sample_rate = session.sample_rate;
                self.calls.emit(Event::Restarted {
                    pid,
                    reason,
                    sample_rate,
                });
                self.state = State::Running(session);
            }
            Err(Abort::Failed) => self.close_after_failure(session),
            Err(Abort::Halted) => self.abandon(session),
        }
    }

    fn service_restarted(&mut self) {
        // AudioHardware.h, kAudioHardwarePropertyServiceRestarted: "any state the client has,
        // such as cached data or added listeners, must be re-established by the client". The old
        // objects died with the old server and their IDs may name others now, so none is
        // destroyed.
        if matches!(self.state, State::Blocked) {
            return;
        }
        self.calls.hal.forget_listeners();
        self.system_listeners.clear();
        match mem::replace(&mut self.state, State::Idle) {
            State::Running(session) => {
                let pid = session.pid;
                self.forget(session);
                if !self.calls.latch.granted() {
                    return self.calls.emit(Event::Ended {
                        pid,
                        reason: EndReason::ServiceRestarted,
                    });
                }
                if self.watch_system().is_err() {
                    return self.state = State::Blocked;
                }
                match self
                    .calls
                    .call(pid, HalCall::TranslatePid, |h| h.translate_pid(pid))
                {
                    Ok(0) => self.calls.emit(Event::Ended {
                        pid,
                        reason: EndReason::ProcessExited,
                    }),
                    Ok(process) => self.open(pid, process, Some(RestartReason::ServiceRestarted)),
                    Err(Abort::Failed) => self.calls.emit(Event::Ended {
                        pid,
                        reason: EndReason::Failed,
                    }),
                    Err(Abort::Halted) => self.state = State::Blocked,
                }
            }
            State::Waiting { pid } => {
                self.state = State::Waiting { pid };
                if self.watch_system().is_err() {
                    return self.state = State::Blocked;
                }
                self.find(pid);
            }
            other => self.state = other,
        }
    }

    pub fn tick(&mut self) {
        let now = self.calls.hal.now_ns();
        match &mut self.state {
            State::Waiting { pid } => {
                let pid = *pid;
                if !self.calls.hal.process_exists(pid) {
                    self.end(EndReason::ProcessExited);
                } else if now >= self.next_poll_ns {
                    // In case a process-list notification was missed.
                    self.next_poll_ns = now + WAITING_POLL_NS;
                    self.find(pid);
                }
            }
            State::Running(session) => {
                let pid = session.pid;
                if !self.calls.hal.process_exists(pid) {
                    return self.end(EndReason::ProcessExited);
                }
                let mut silent_for = None;
                if let Some(io) = session.io.as_mut().filter(|io| io.started) {
                    // SAFETY: a context is freed only by close_io, which takes the session.
                    let context = unsafe { io.context.as_ref() };
                    let waited = now - io.started_ns;
                    if !io.receiving && context.callbacks() > 0 {
                        io.receiving = true;
                        self.calls.latch.grant();
                        self.calls.emit(Event::Receiving {
                            pid,
                            after_ms: millis(waited),
                        });
                    } else if !io.receiving && waited >= self.first_callback_ns {
                        silent_for = Some(waited);
                    }
                    let unreadable = context.unreadable();
                    if unreadable > io.reported_unreadable && now >= io.next_unreadable_report_ns {
                        self.calls.emit(Event::Unreadable {
                            pid,
                            callbacks: unreadable - io.reported_unreadable,
                        });
                        io.reported_unreadable = unreadable;
                        io.next_unreadable_report_ns = now + LOSS_REPORT_NS;
                    }
                }
                if let Some(waited) = silent_for {
                    self.calls.block(BlockReason::NoCallbacks {
                        waited_ms: millis(waited),
                    });
                    self.halt();
                }
            }
            State::Idle | State::Blocked => {}
        }
        self.report_overruns(now);
    }

    fn report_overruns(&mut self, now: i64) {
        let dropped = self.ring.stats().dropped_frames;
        if dropped > self.reported_drops && now >= self.next_overrun_report_ns {
            self.calls.emit(Event::Overrun {
                dropped_frames: dropped - self.reported_drops,
            });
            self.reported_drops = dropped;
            self.next_overrun_report_ns = now + LOSS_REPORT_NS;
        }
    }

    #[cfg(test)]
    fn hal(&mut self) -> &mut H {
        &mut self.calls.hal
    }
}

fn nanos(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

fn millis(ns: i64) -> u64 {
    u64::try_from(ns / 1_000_000).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ioproc::tests::{buffer, call, list, time};
    use crate::packetizer::Packetizer;
    use crate::packetizer::tests::{Owned, drain};
    use objc2_core_audio_types::AudioTimeStampFlags;
    use std::collections::{HashMap, HashSet};
    use std::sync::mpsc;

    const PID: i32 = 4242;
    const PROCESS: ObjectId = 77;

    #[derive(Clone, Debug, PartialEq)]
    enum Logged {
        Translate(i32),
        CreateTap(ObjectId),
        ReadFormat(ObjectId),
        DestroyTap(ObjectId),
        CreateAggregate(String),
        DeviceAlive(ObjectId),
        DestroyAggregate(ObjectId),
        CreateProc(ObjectId),
        Start(u32),
        Stop(u32),
        DestroyProc(u32),
        Listen(ObjectId, Watch),
        Unlisten(u32),
        Forget,
    }

    struct Fake {
        now: i64,
        log: Vec<Logged>,
        objects: HashMap<i32, ObjectId>,
        alive: HashSet<i32>,
        format: TapFormat,
        device_alive: bool,
        fail: HashMap<HalCall, i32>,
        delay_ns: HashMap<HalCall, i64>,
        next_id: u32,
        contexts: HashMap<u32, NonNull<IoContext>>,
    }

    impl Fake {
        fn new() -> Fake {
            Fake {
                now: 1_000_000_000,
                log: Vec::new(),
                objects: HashMap::from([(PID, PROCESS)]),
                alive: HashSet::from([PID]),
                format: TapFormat {
                    sample_rate: 48_000.0,
                    channels: 2,
                    float32: true,
                },
                device_alive: true,
                fail: HashMap::new(),
                delay_ns: HashMap::new(),
                next_id: 100,
                contexts: HashMap::new(),
            }
        }

        fn step(&mut self, call: HalCall, logged: Logged) -> Result<(), Status> {
            self.log.push(logged);
            self.now += self.delay_ns.get(&call).copied().unwrap_or(1_000);
            match self.fail.get(&call) {
                Some(&status) => Err(Status(status)),
                None => Ok(()),
            }
        }

        fn id(&mut self) -> u32 {
            self.next_id += 1;
            self.next_id
        }

        /// One IO cycle of interleaved stereo on the newest IOProc.
        fn fire(&mut self, frames: usize, sample_time: f64) {
            let proc = *self.contexts.keys().max().expect("an IOProc");
            // SAFETY: the controller keeps a context alive while its IOProc is registered.
            let context = unsafe { self.contexts[&proc].as_ref() };
            let mut data = vec![0.25f32; frames * 2];
            let list = list(&[buffer(&mut data, 2)]);
            let stamp = time(
                sample_time as u64 * 500,
                sample_time,
                AudioTimeStampFlags::SampleHostTimeValid,
            );
            call(context, Some(&list), Some(&stamp));
        }

        /// One IO cycle with no buffer list on the newest IOProc.
        fn fire_unreadable(&mut self) {
            let proc = *self.contexts.keys().max().expect("an IOProc");
            // SAFETY: as in fire.
            call(unsafe { self.contexts[&proc].as_ref() }, None, None);
        }
    }

    impl Hal for Fake {
        type Proc = u32;

        fn now_ns(&self) -> i64 {
            self.now
        }

        fn process_exists(&self, pid: i32) -> bool {
            self.alive.contains(&pid)
        }

        fn translate_pid(&mut self, pid: i32) -> Result<ObjectId, Status> {
            self.step(HalCall::TranslatePid, Logged::Translate(pid))?;
            Ok(self.objects.get(&pid).copied().unwrap_or(0))
        }

        fn create_tap(&mut self, process: ObjectId) -> Result<TapObject, Status> {
            self.step(HalCall::CreateTap, Logged::CreateTap(process))?;
            let id = self.id();
            Ok(TapObject {
                id,
                uid: format!("tap-{id}"),
            })
        }

        fn tap_format(&mut self, tap: ObjectId) -> Result<TapFormat, Status> {
            self.step(HalCall::ReadTapFormat, Logged::ReadFormat(tap))?;
            Ok(self.format.clone())
        }

        fn destroy_tap(&mut self, tap: ObjectId) -> Result<(), Status> {
            self.step(HalCall::DestroyTap, Logged::DestroyTap(tap))
        }

        fn create_aggregate(&mut self, tap_uid: &str, _pid: i32) -> Result<ObjectId, Status> {
            self.step(
                HalCall::CreateAggregate,
                Logged::CreateAggregate(tap_uid.into()),
            )?;
            Ok(self.id())
        }

        fn device_alive(&mut self, device: ObjectId) -> Result<bool, Status> {
            self.step(HalCall::ReadDeviceAlive, Logged::DeviceAlive(device))?;
            Ok(self.device_alive)
        }

        fn destroy_aggregate(&mut self, device: ObjectId) -> Result<(), Status> {
            self.step(HalCall::DestroyAggregate, Logged::DestroyAggregate(device))
        }

        fn create_io_proc(
            &mut self,
            device: ObjectId,
            context: NonNull<IoContext>,
        ) -> Result<u32, Status> {
            self.step(HalCall::CreateIoProc, Logged::CreateProc(device))?;
            let id = self.id();
            self.contexts.insert(id, context);
            Ok(id)
        }

        fn start(&mut self, _device: ObjectId, proc: u32) -> Result<(), Status> {
            self.step(HalCall::StartIo, Logged::Start(proc))
        }

        fn stop(&mut self, _device: ObjectId, proc: u32) -> Result<(), Status> {
            self.step(HalCall::StopIo, Logged::Stop(proc))
        }

        fn destroy_io_proc(&mut self, _device: ObjectId, proc: u32) -> Result<(), Status> {
            self.step(HalCall::DestroyIoProc, Logged::DestroyProc(proc))?;
            self.contexts.remove(&proc);
            Ok(())
        }

        fn add_listener(&mut self, object: ObjectId, watch: Watch) -> Result<ListenerKey, Status> {
            self.step(HalCall::AddListener, Logged::Listen(object, watch))?;
            Ok(ListenerKey(self.id()))
        }

        fn remove_listener(&mut self, key: ListenerKey) -> Result<(), Status> {
            self.step(HalCall::RemoveListener, Logged::Unlisten(key.0))
        }

        fn forget_listeners(&mut self) {
            self.log.push(Logged::Forget);
        }
    }

    // The fake numbers objects from 101 in creation order: the two system listeners, then the
    // tap, its listener, the aggregate, its two listeners and the IOProc.
    const PROCESS_LIST_KEY: u32 = 101;
    const RESTART_KEY: u32 = 102;
    const TAP: ObjectId = 103;
    const TAP_LISTENER: u32 = 104;
    const AGGREGATE: ObjectId = 105;
    const ALIVE_LISTENER: u32 = 106;
    const RATE_LISTENER: u32 = 107;
    const PROC: u32 = 108;

    struct Rig {
        controller: Controller<Fake>,
        events: mpsc::Receiver<Event>,
        packetizer: Packetizer,
        latch: &'static Latch,
    }

    fn rig_with(settings: Settings) -> Rig {
        let ring = Arc::new(CaptureRing::new(1 << 14));
        let (tx, events) = mpsc::channel();
        let latch: &'static Latch = Box::leak(Box::new(Latch::new()));
        let controller = Controller::new(Fake::new(), ring.clone(), tx, latch, settings, 1);
        Rig {
            controller,
            events,
            packetizer: Packetizer::new(ring, 240, 0),
            latch,
        }
    }

    fn settings() -> Settings {
        Settings {
            first_callback_timeout: Duration::from_secs(5),
            stall_threshold: Duration::from_secs(20),
            slow_call: Duration::from_millis(500),
            marker: None,
        }
    }

    fn rig() -> Rig {
        rig_with(settings())
    }

    impl Rig {
        fn events(&self) -> Vec<Event> {
            self.events.try_iter().collect()
        }

        fn log(&mut self) -> Vec<Logged> {
            mem::take(&mut self.controller.hal().log)
        }

        fn advance(&mut self, ns: i64) {
            self.controller.hal().now += ns;
        }

        /// Captures PID and proves the permission with one callback.
        fn running(&mut self) {
            self.controller.handle(Command::Capture(PID));
            self.controller.hal().fire(512, 0.0);
            self.controller.tick();
            assert!(
                self.events()
                    .iter()
                    .any(|e| matches!(e, Event::Receiving { .. }))
            );
            self.log();
        }
    }

    fn open_calls() -> Vec<Logged> {
        vec![
            Logged::Listen(SYSTEM_OBJECT, Watch::ProcessList),
            Logged::Listen(SYSTEM_OBJECT, Watch::ServiceRestarted),
            Logged::Translate(PID),
            Logged::CreateTap(PROCESS),
            Logged::ReadFormat(TAP),
            Logged::Listen(TAP, Watch::TapFormat),
            Logged::CreateAggregate(format!("tap-{TAP}")),
            Logged::Listen(AGGREGATE, Watch::DeviceAlive),
            Logged::Listen(AGGREGATE, Watch::SampleRate),
            Logged::CreateProc(AGGREGATE),
            Logged::Start(PROC),
        ]
    }

    fn close_calls() -> Vec<Logged> {
        vec![
            Logged::Unlisten(ALIVE_LISTENER),
            Logged::Unlisten(RATE_LISTENER),
            Logged::Stop(PROC),
            Logged::DestroyProc(PROC),
            Logged::DestroyAggregate(AGGREGATE),
            Logged::Unlisten(TAP_LISTENER),
            Logged::DestroyTap(TAP),
        ]
    }

    #[test]
    fn opens_tap_then_aggregate_then_ioproc_and_tears_down_in_reverse() {
        let mut rig = rig();
        rig.controller.handle(Command::Capture(PID));
        assert_eq!(rig.log(), open_calls());
        assert_eq!(
            rig.events(),
            vec![Event::Started {
                pid: PID,
                sample_rate: 48_000
            }]
        );

        rig.controller.hal().fire(512, 0.0);
        rig.controller.tick();
        assert_eq!(
            rig.events(),
            vec![Event::Receiving {
                pid: PID,
                after_ms: 0
            }]
        );
        assert!(rig.latch.granted());
        let items = drain(&mut rig.packetizer);
        assert!(matches!(items[0], Owned::Start(_)));
        assert_eq!(items.len(), 3, "start and two full packets");

        rig.controller.handle(Command::Stop);
        assert_eq!(rig.log(), close_calls());
        assert_eq!(
            rig.events(),
            vec![Event::Ended {
                pid: PID,
                reason: EndReason::Stopped
            }]
        );
        let items = drain(&mut rig.packetizer);
        assert_eq!(
            items.last(),
            Some(&Owned::End(0)),
            "the retired epoch ends the stream"
        );

        rig.controller.shutdown();
        assert_eq!(
            rig.log(),
            vec![
                Logged::Unlisten(PROCESS_LIST_KEY),
                Logged::Unlisten(RESTART_KEY)
            ]
        );
    }

    #[test]
    fn a_process_without_audio_yet_is_waited_for() {
        let mut rig = rig();
        rig.controller.hal().objects.clear();
        rig.controller.handle(Command::Capture(PID));
        assert_eq!(rig.events(), vec![Event::WaitingForProcess { pid: PID }]);
        rig.log();

        rig.controller.handle(Command::Changed(Watch::ProcessList));
        assert_eq!(rig.log(), vec![Logged::Translate(PID)]);
        assert!(rig.events().is_empty(), "still waiting, said once");

        rig.controller.hal().objects.insert(PID, PROCESS);
        rig.controller.handle(Command::Changed(Watch::ProcessList));
        assert_eq!(
            rig.log()[..2],
            [Logged::Translate(PID), Logged::CreateTap(PROCESS)]
        );
        assert_eq!(
            rig.events(),
            vec![Event::Started {
                pid: PID,
                sample_rate: 48_000
            }]
        );
    }

    #[test]
    fn waiting_ends_when_the_process_exits() {
        let mut rig = rig();
        rig.controller.hal().objects.clear();
        rig.controller.handle(Command::Capture(PID));
        rig.events();
        rig.log();
        rig.controller.hal().alive.clear();
        rig.controller.tick();
        assert_eq!(
            rig.events(),
            vec![Event::Ended {
                pid: PID,
                reason: EndReason::ProcessExited
            }]
        );
        assert!(rig.log().is_empty());
    }

    #[test]
    fn a_pid_that_is_already_gone_ends_at_once() {
        let mut rig = rig();
        rig.controller.hal().objects.clear();
        rig.controller.hal().alive.clear();
        rig.controller.handle(Command::Capture(PID));
        assert_eq!(
            rig.events(),
            vec![Event::Ended {
                pid: PID,
                reason: EndReason::ProcessExited
            }]
        );
    }

    #[test]
    fn waiting_polls_in_case_a_notification_is_missed() {
        let mut rig = rig();
        rig.controller.hal().objects.clear();
        rig.controller.handle(Command::Capture(PID));
        rig.controller.tick();
        rig.log();
        rig.controller.tick();
        assert!(rig.log().is_empty(), "at most once a second");
        rig.advance(WAITING_POLL_NS);
        rig.controller.tick();
        assert_eq!(rig.log(), vec![Logged::Translate(PID)]);
    }

    #[test]
    fn the_app_exiting_tears_the_capture_down() {
        let mut rig = rig();
        rig.running();
        rig.controller.hal().objects.clear();
        rig.controller.handle(Command::Changed(Watch::ProcessList));
        let mut expected = vec![Logged::Translate(PID)];
        expected.extend(close_calls());
        assert_eq!(rig.log(), expected);
        assert_eq!(
            rig.events(),
            vec![Event::Ended {
                pid: PID,
                reason: EndReason::ProcessExited
            }]
        );
    }

    #[test]
    fn a_dead_pid_is_noticed_without_a_notification() {
        let mut rig = rig();
        rig.running();
        rig.controller.hal().alive.clear();
        rig.controller.tick();
        assert_eq!(rig.log(), close_calls());
        assert_eq!(
            rig.events(),
            vec![Event::Ended {
                pid: PID,
                reason: EndReason::ProcessExited
            }]
        );
    }

    #[test]
    fn a_stalled_ioproc_registration_blocks_and_nothing_else_is_called() {
        let dir = std::env::temp_dir().join(format!("audiotap-control-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("blocked");
        let _ = std::fs::remove_file(&marker);
        let mut rig = rig_with(Settings {
            marker: Some(marker.clone()),
            ..settings()
        });
        rig.controller
            .hal()
            .delay_ns
            .insert(HalCall::CreateIoProc, 60_000_000_000);
        rig.controller.handle(Command::Capture(PID));
        assert_eq!(rig.log().last(), Some(&Logged::CreateProc(AGGREGATE)));
        let reason = BlockReason::Stalled {
            call: HalCall::CreateIoProc,
            elapsed_ms: 60_000,
        };
        assert_eq!(rig.events(), vec![Event::Blocked(reason.clone())]);
        assert!(marker.exists());

        rig.controller.hal().fire(512, 0.0);
        assert_eq!(
            rig.controller.ring.stats().chunks,
            0,
            "the context is retired"
        );
        rig.controller.handle(Command::Capture(PID + 1));
        rig.controller
            .handle(Command::Changed(Watch::ServiceRestarted));
        rig.controller.handle(Command::Changed(Watch::TapFormat));
        rig.controller.handle(Command::Stop);
        rig.advance(10_000_000_000);
        rig.controller.tick();
        rig.controller.shutdown();
        assert!(
            rig.log().is_empty(),
            "no HAL call after the stall, not even teardown"
        );
        assert_eq!(
            rig.events(),
            vec![Event::Refused {
                pid: PID + 1,
                reason
            }]
        );

        let mut later = rig_with(Settings {
            marker: Some(marker.clone()),
            ..settings()
        });
        later.controller.handle(Command::Capture(PID));
        assert!(later.log().is_empty());
        assert!(matches!(
            later.events()[..],
            [Event::Refused {
                reason: BlockReason::Recorded(_),
                ..
            }]
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_callbacks_after_start_blocks_without_teardown() {
        let mut rig = rig();
        rig.controller.handle(Command::Capture(PID));
        rig.events();
        rig.log();
        rig.advance(4_000_000_000);
        rig.controller.tick();
        assert!(rig.events().is_empty());
        rig.advance(1_000_000_000);
        rig.controller.tick();
        assert_eq!(
            rig.events(),
            vec![Event::Blocked(BlockReason::NoCallbacks {
                waited_ms: 5_000
            })]
        );
        rig.controller.handle(Command::Stop);
        rig.controller.shutdown();
        assert!(rig.log().is_empty());
        assert!(!rig.latch.granted());
    }

    #[test]
    fn a_stall_during_teardown_leaves_the_rest_alone() {
        let mut rig = rig();
        rig.running();
        rig.controller
            .hal()
            .delay_ns
            .insert(HalCall::StopIo, 30_000_000_000);
        rig.controller.handle(Command::Stop);
        assert_eq!(rig.log(), close_calls()[..3]);
        assert_eq!(
            rig.events(),
            vec![
                Event::Blocked(BlockReason::Stalled {
                    call: HalCall::StopIo,
                    elapsed_ms: 30_000
                }),
                Event::Ended {
                    pid: PID,
                    reason: EndReason::Blocked
                },
            ]
        );
        rig.controller.shutdown();
        assert!(rig.log().is_empty());
    }

    #[test]
    fn a_failed_call_ends_the_capture_and_is_not_retried() {
        let mut rig = rig();
        rig.controller
            .hal()
            .fail
            .insert(HalCall::CreateTap, 560_947_818);
        rig.controller.handle(Command::Capture(PID));
        assert_eq!(
            rig.events(),
            vec![
                Event::Failed {
                    pid: PID,
                    call: HalCall::CreateTap,
                    status: 560_947_818
                },
                Event::Ended {
                    pid: PID,
                    reason: EndReason::Failed
                },
            ]
        );
        rig.log();
        for _ in 0..50 {
            rig.advance(1_000_000_000);
            rig.controller.tick();
        }
        assert!(rig.log().is_empty());
    }

    #[test]
    fn a_failed_start_unwinds_what_was_built() {
        let mut rig = rig();
        rig.controller.hal().fail.insert(HalCall::StartIo, -50);
        rig.controller.handle(Command::Capture(PID));
        let log = rig.log();
        let unwound: Vec<_> = close_calls()
            .into_iter()
            .filter(|l| *l != Logged::Stop(PROC))
            .collect();
        assert_eq!(log[open_calls().len()..], unwound);
        assert_eq!(
            rig.events().last(),
            Some(&Event::Ended {
                pid: PID,
                reason: EndReason::Failed
            })
        );
    }

    #[test]
    fn an_unreadable_format_is_reported_and_the_tap_destroyed() {
        let mut rig = rig();
        rig.controller.hal().format = TapFormat {
            sample_rate: 48_000.0,
            channels: 2,
            float32: false,
        };
        rig.controller.handle(Command::Capture(PID));
        assert_eq!(
            rig.log()[3..],
            [
                Logged::CreateTap(PROCESS),
                Logged::ReadFormat(TAP),
                Logged::DestroyTap(TAP)
            ]
        );
        assert_eq!(
            rig.events(),
            vec![
                Event::UnsupportedFormat {
                    pid: PID,
                    sample_rate: 48_000.0,
                    channels: 2,
                    float32: false
                },
                Event::Ended {
                    pid: PID,
                    reason: EndReason::Failed
                },
            ]
        );
    }

    #[test]
    fn a_format_change_rebuilds_the_aggregate_and_ioproc_once_proven() {
        let mut rig = rig();
        rig.running();
        drain(&mut rig.packetizer);
        rig.controller.hal().format.sample_rate = 44_100.0;
        rig.controller.handle(Command::Changed(Watch::TapFormat));
        let mut expected = vec![Logged::ReadFormat(TAP)];
        expected.extend(close_calls()[..5].iter().cloned());
        expected.extend([
            Logged::CreateAggregate(format!("tap-{TAP}")),
            Logged::Listen(109, Watch::DeviceAlive),
            Logged::Listen(109, Watch::SampleRate),
            Logged::CreateProc(109),
            Logged::Start(112),
        ]);
        assert_eq!(rig.log(), expected);
        assert_eq!(
            rig.events(),
            vec![Event::Restarted {
                pid: PID,
                reason: RestartReason::FormatChanged,
                sample_rate: 44_100
            }]
        );
        rig.controller.hal().fire(512, 0.0);
        let items = drain(&mut rig.packetizer);
        assert!(items.contains(&Owned::End(0)));
        assert!(
            items
                .iter()
                .any(|i| matches!(i, Owned::Start(f) if f.stream == 1 && f.sample_rate == 44_100))
        );
    }

    #[test]
    fn an_unchanged_format_notification_does_nothing() {
        let mut rig = rig();
        rig.running();
        rig.controller.handle(Command::Changed(Watch::SampleRate));
        assert_eq!(rig.log(), vec![Logged::ReadFormat(TAP)]);
        assert!(rig.events().is_empty());
    }

    #[test]
    fn before_the_permission_is_proven_a_format_change_ends_instead_of_rebuilding() {
        let mut rig = rig();
        rig.controller.handle(Command::Capture(PID));
        rig.events();
        rig.log();
        rig.controller.hal().format.sample_rate = 44_100.0;
        rig.controller.handle(Command::Changed(Watch::TapFormat));
        let mut expected = vec![Logged::ReadFormat(TAP)];
        expected.extend(close_calls());
        assert_eq!(rig.log(), expected);
        assert_eq!(
            rig.events(),
            vec![Event::Ended {
                pid: PID,
                reason: EndReason::FormatChanged
            }]
        );
    }

    #[test]
    fn a_dead_aggregate_is_rebuilt() {
        let mut rig = rig();
        rig.running();
        rig.controller.hal().device_alive = false;
        rig.controller.handle(Command::Changed(Watch::DeviceAlive));
        assert_eq!(rig.log()[0], Logged::DeviceAlive(AGGREGATE));
        assert_eq!(
            rig.events(),
            vec![Event::Restarted {
                pid: PID,
                reason: RestartReason::DeviceDied,
                sample_rate: 48_000
            }]
        );
    }

    #[test]
    fn a_service_restart_rebuilds_from_scratch_without_touching_stale_ids() {
        let mut rig = rig();
        rig.running();
        rig.controller
            .handle(Command::Changed(Watch::ServiceRestarted));
        assert_eq!(
            rig.log(),
            vec![
                Logged::Forget,
                Logged::Listen(SYSTEM_OBJECT, Watch::ProcessList),
                Logged::Listen(SYSTEM_OBJECT, Watch::ServiceRestarted),
                Logged::Translate(PID),
                Logged::CreateTap(PROCESS),
                Logged::ReadFormat(111),
                Logged::Listen(111, Watch::TapFormat),
                Logged::CreateAggregate("tap-111".into()),
                Logged::Listen(113, Watch::DeviceAlive),
                Logged::Listen(113, Watch::SampleRate),
                Logged::CreateProc(113),
                Logged::Start(116),
            ]
        );
        assert_eq!(
            rig.events(),
            vec![Event::Restarted {
                pid: PID,
                reason: RestartReason::ServiceRestarted,
                sample_rate: 48_000
            }]
        );
    }

    #[test]
    fn a_service_restart_before_the_permission_is_proven_ends() {
        let mut rig = rig();
        rig.controller.handle(Command::Capture(PID));
        rig.events();
        rig.log();
        rig.controller
            .handle(Command::Changed(Watch::ServiceRestarted));
        assert_eq!(rig.log(), vec![Logged::Forget]);
        assert_eq!(
            rig.events(),
            vec![Event::Ended {
                pid: PID,
                reason: EndReason::ServiceRestarted
            }]
        );
    }

    #[test]
    fn slow_calls_are_reported_and_tolerated() {
        let mut rig = rig();
        rig.controller
            .hal()
            .delay_ns
            .insert(HalCall::CreateIoProc, 2_000_000_000);
        rig.controller.handle(Command::Capture(PID));
        assert_eq!(
            rig.events(),
            vec![
                Event::SlowCall {
                    call: HalCall::CreateIoProc,
                    elapsed_ms: 2_000
                },
                Event::Started {
                    pid: PID,
                    sample_rate: 48_000
                },
            ]
        );
    }

    #[test]
    fn a_new_capture_replaces_the_old_one_after_its_teardown() {
        let mut rig = rig();
        rig.running();
        rig.controller.hal().objects.insert(PID + 1, PROCESS + 1);
        rig.controller.hal().alive.insert(PID + 1);
        rig.controller.handle(Command::Capture(PID + 1));
        let log = rig.log();
        assert_eq!(log[..close_calls().len()], close_calls());
        assert_eq!(
            log[close_calls().len()..][..2],
            [Logged::Translate(PID + 1), Logged::CreateTap(PROCESS + 1)]
        );
        assert_eq!(
            rig.events(),
            vec![
                Event::Ended {
                    pid: PID,
                    reason: EndReason::Replaced
                },
                Event::Started {
                    pid: PID + 1,
                    sample_rate: 48_000
                },
            ]
        );
    }

    #[test]
    fn unreadable_callbacks_still_prove_the_permission_and_are_reported() {
        let mut rig = rig();
        rig.controller.handle(Command::Capture(PID));
        rig.events();
        for _ in 0..3 {
            rig.controller.hal().fire_unreadable();
        }
        rig.controller.tick();
        assert_eq!(
            rig.events(),
            vec![
                Event::Receiving {
                    pid: PID,
                    after_ms: 0
                },
                Event::Unreadable {
                    pid: PID,
                    callbacks: 3
                }
            ]
        );
        rig.controller.hal().fire_unreadable();
        rig.controller.tick();
        assert!(rig.events().is_empty(), "at most once a second");
        rig.advance(LOSS_REPORT_NS);
        rig.controller.tick();
        assert_eq!(
            rig.events(),
            vec![Event::Unreadable {
                pid: PID,
                callbacks: 1
            }]
        );
    }

    #[test]
    fn overruns_are_reported_at_most_once_a_second() {
        let mut rig = rig();
        rig.running();
        for i in 1..=40 {
            rig.controller.hal().fire(512, i as f64 * 512.0);
        }
        rig.controller.tick();
        assert_eq!(
            rig.events(),
            vec![Event::Overrun {
                dropped_frames: 9 * 512
            }]
        );
        rig.controller.hal().fire(512, 50_000.0);
        rig.controller.tick();
        assert!(rig.events().is_empty());
        rig.advance(LOSS_REPORT_NS);
        rig.controller.tick();
        assert_eq!(
            rig.events(),
            vec![Event::Overrun {
                dropped_frames: 512
            }]
        );
    }
}
