#![allow(dead_code)]

use std;
use std::io;
use std::error::Error;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};

use serde_json as json;
use futures::Future;
use byteorder::{ByteOrder, BigEndian};
use bytes::{BytesMut, BufMut};
use tokio_core::reactor::Timeout;
use tokio_io::codec::{Encoder, Decoder};
use nix::sys::signal::{kill, Signal};
use nix::unistd::{close, pipe, fork, ForkResult, Pid};

use actix::prelude::*;

use config::ServiceConfig;
use io::PipeFile;
use worker::{WorkerMessage, WorkerCommand};
use event::Reason;
use exec::exec_worker;
use service::{self, FeService};

const HEARTBEAT: u64 = 2;
const WORKER_TIMEOUT: i8 = 98;
pub const WORKER_INIT_FAILED: i8 = 99;
pub const WORKER_BOOT_FAILED: i8 = 100;

pub struct Process {
    idx: usize,
    pid: Pid,
    state: ProcessState,
    hb: Instant,
    addr: Address<FeService>,
    timeout: Duration,
    startup_timeout: u64,
    shutdown_timeout: u64,
}

impl Actor for Process {
    type Context = FramedContext<Self>;
}

impl FramedActor for Process {
    type Io = PipeFile;
    type Codec = TransportCodec;
}

#[derive(Debug)]
enum ProcessState {
    Starting,
    Failed,
    Running,
    Stopping,
}

#[derive(PartialEq, Debug)]
pub enum ProcessMessage {
    Message(WorkerMessage),
    StartupTimeout,
    StopTimeout,
    Heartbeat,
    Kill,
}

#[derive(Debug, Clone)]
pub enum ProcessError {
    /// Heartbeat failed
    Heartbeat,
    /// Worker startup process failed, possibly application initialization failed
    FailedToStart(Option<String>),
    /// Timeout during startup
    StartupTimeout,
    /// Timeout during graceful stop
    StopTimeout,
    /// Worker configuratin error
    ConfigError(String),
    /// Worker init failed
    InitFailed,
    /// Worker boot failed
    BootFailed,
    /// Worker received signal
    Signal(usize),
    /// Worker exited with code
    ExitCode(i8),
}

impl ProcessError {
    pub fn from(code: i8) -> ProcessError {
        match code {
            WORKER_TIMEOUT => ProcessError::StartupTimeout,
            WORKER_INIT_FAILED => ProcessError::InitFailed,
            WORKER_BOOT_FAILED => ProcessError::BootFailed,
            code => ProcessError::ExitCode(code),
        }
    }
}

impl<'a> std::convert::From<&'a ProcessError> for Reason
{
    fn from(ob: &'a ProcessError) -> Self {
        match ob {
            &ProcessError::Heartbeat => Reason::HeartbeatFailed,
            &ProcessError::FailedToStart(ref err) =>
                Reason::FailedToStart(
                    if let &Some(ref e) = err { Some(format!("{}", e))} else {None}),
            &ProcessError::StartupTimeout => Reason::StartupTimeout,
            &ProcessError::StopTimeout => Reason::StopTimeout,
            &ProcessError::ConfigError(ref err) => Reason::WorkerError(err.clone()),
            &ProcessError::InitFailed => Reason::InitFailed,
            &ProcessError::BootFailed => Reason::BootFailed,
            &ProcessError::Signal(sig) => Reason::Signal(sig),
            &ProcessError::ExitCode(code) => Reason::ExitCode(code),
        }
    }
}


impl Process {

    pub fn start(idx: usize, cfg: &ServiceConfig, addr: Address<FeService>)
                 -> (Pid, Option<Address<Process>>)
    {
        // fork process and esteblish communication
        let (pid, pipe) = match Process::fork(cfg) {
            Ok(res) => res,
            Err(err) => {
                let pid = Pid::from_raw(-1);
                addr.send(
                    service::ProcessFailed(
                        idx, pid,
                        ProcessError::FailedToStart(Some(format!("{}", err)))));

                return (pid, None)
            }
        };

        let timeout = Duration::new(cfg.timeout as u64, 0);
        let startup_timeout = cfg.startup_timeout as u64;
        let shutdown_timeout = cfg.shutdown_timeout as u64;

        // start Process service
        let addr = Process::create_framed(pipe, TransportCodec,
            move |ctx| {
                ctx.add_future(
                    Timeout::new(Duration::new(startup_timeout as u64, 0), Arbiter::handle())
                        .unwrap()
                        .map(|_| ProcessMessage::StartupTimeout)
                );

                Process {
                    idx: idx,
                    pid: pid,
                    state: ProcessState::Starting,
                    hb: Instant::now(),
                    addr: addr,
                    timeout: timeout,
                    startup_timeout: startup_timeout,
                    shutdown_timeout: shutdown_timeout,
                }
            });
        (pid, Some(addr))
    }

    fn fork(cfg: &ServiceConfig) -> Result<(Pid, PipeFile), io::Error>
    {
        let (p_read, p_write, ch_read, ch_write) = Process::create_pipes()?;

        // fork
        let pid = match fork() {
            Ok(ForkResult::Parent{ child }) => child,
            Ok(ForkResult::Child) => {
                let _ = close(p_write);
                let _ = close(ch_read);
                exec_worker(cfg, p_read, ch_write);
                unreachable!();
            },
            Err(err) => {
                error!("Fork failed: {}", err.description());
                return Err(io::Error::new(io::ErrorKind::Other, err.description()))
            }
        };

        // initialize worker communication channel
        let _ = close(p_read);
        let _ = close(ch_write);
        let pipe = PipeFile::new(ch_read, p_write, Arbiter::handle());

        Ok((pid, pipe))
    }

    fn create_pipes() -> Result<(RawFd, RawFd, RawFd, RawFd), io::Error> {
        // open communication pipes
        let (p_read, p_write) = match pipe() {
            Ok((r, w)) => (r, w),
            Err(err) => {
                error!("Can not create pipe: {}", err);
                return Err(io::Error::new(
                    io::ErrorKind::Other, format!("Can not create pipe: {}", err)))
            }
        };
        let (ch_read, ch_write) = match pipe() {
            Ok((r, w)) => (r, w),
            Err(err) => {
                error!("Can not create pipe: {}", err);
                return Err(io::Error::new(
                    io::ErrorKind::Other, format!("Can not create pipe: {}", err)))
            }
        };
        Ok((p_read, p_write, ch_read, ch_write))
    }

    fn kill(&self, ctx: &mut FramedContext<Self>, graceful: bool) {
        if graceful {
            let fut = Box::new(
                Timeout::new(Duration::new(1, 0), Arbiter::handle())
                    .unwrap()
                    .map(|_| ProcessMessage::Kill));
            ctx.add_future(fut);
        } else {
            let _ = kill(self.pid, Signal::SIGKILL);
            ctx.terminate();
        }
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = kill(self.pid, Signal::SIGKILL);
    }
}

impl StreamHandler<ProcessMessage, io::Error> for Process {

    fn finished(&mut self, ctx: &mut FramedContext<Self>) {
        self.kill(ctx, false);
    }
}

impl ResponseType<ProcessMessage> for Process {
    type Item = ();
    type Error = ();
}

impl Handler<ProcessMessage, io::Error> for Process {

    fn error(&mut self, _: io::Error, ctx: &mut FramedContext<Self>) {
        self.kill(ctx, false)
    }

    fn handle(&mut self, msg: ProcessMessage, ctx: &mut FramedContext<Self>)
              -> Response<Self, ProcessMessage>
    {
        match msg {
            ProcessMessage::Message(msg) => match msg {
                WorkerMessage::forked => {
                    debug!("Worker forked (pid:{})", self.pid);
                    let _ = ctx.send(WorkerCommand::prepare);
                }
                WorkerMessage::loaded => {
                    match self.state {
                        ProcessState::Starting => {
                            debug!("Worker loaded (pid:{})", self.pid);
                            self.addr.send(
                                service::ProcessLoaded(self.idx, self.pid));

                            // start heartbeat timer
                            self.state = ProcessState::Running;
                            self.hb = Instant::now();
                            let fut = Box::new(
                                Timeout::new(
                                    Duration::new(HEARTBEAT, 0), Arbiter::handle())
                                    .unwrap()
                                    .map(|_| ProcessMessage::Heartbeat));
                            ctx.add_future(fut);
                        },
                        _ => {
                            warn!("Received `loaded` message from worker (pid:{})", self.pid);
                        }
                    }
                }
                WorkerMessage::hb => {
                    self.hb = Instant::now();
                }
                WorkerMessage::reload => {
                    // worker requests reload
                    info!("Worker requests reload (pid:{})", self.pid);
                    self.addr.send(
                        service::ProcessMessage(
                            self.idx, self.pid, WorkerMessage::reload));
                }
                WorkerMessage::restart => {
                    // worker requests reload
                    info!("Worker requests restart (pid:{})", self.pid);
                    self.addr.send(
                        service::ProcessMessage(
                            self.idx, self.pid, WorkerMessage::restart));
                }
                WorkerMessage::cfgerror(msg) => {
                    error!("Worker config error: {} (pid:{})", msg, self.pid);
                    self.addr.send(
                        service::ProcessFailed(
                            self.idx, self.pid, ProcessError::ConfigError(msg)));
                }
            }
            ProcessMessage::StartupTimeout => {
                match self.state {
                    ProcessState::Starting => {
                        error!("Worker startup timeout after {} secs", self.startup_timeout);
                        self.addr.send(
                            service::ProcessFailed(
                                self.idx, self.pid, ProcessError::StartupTimeout));

                        self.state = ProcessState::Failed;
                        let _ = kill(self.pid, Signal::SIGKILL);
                        ctx.stop();
                        return Response::Empty()
                    },
                    _ => ()
                }
            }
            ProcessMessage::StopTimeout => {
                match self.state {
                    ProcessState::Stopping => {
                        info!("Worker shutdown timeout aftre {} secs", self.shutdown_timeout);
                        self.addr.send(
                            service::ProcessFailed(
                                self.idx, self.pid, ProcessError::StopTimeout));

                        self.state = ProcessState::Failed;
                        let _ = kill(self.pid, Signal::SIGKILL);
                        ctx.stop();
                        return Response::Empty()
                    },
                    _ => ()
                }
            }
            ProcessMessage::Heartbeat => {
                // makes sense only in running state
                if let ProcessState::Running = self.state {
                    if Instant::now().duration_since(self.hb) > self.timeout {
                        // heartbeat timed out
                        error!("Worker heartbeat failed (pid:{}) after {:?} secs",
                               self.pid, self.timeout);
                        self.addr.send(
                            service::ProcessFailed(
                                self.idx, self.pid, ProcessError::Heartbeat));
                    } else {
                        // send heartbeat to worker process and reset hearbeat timer
                        let _ = ctx.send(WorkerCommand::hb);
                        let fut = Box::new(
                                Timeout::new(Duration::new(HEARTBEAT, 0), Arbiter::handle())
                                    .unwrap()
                                    .map(|_| ProcessMessage::Heartbeat));
                        ctx.add_future(fut);
                    }
                }
            }
            ProcessMessage::Kill => {
                println!("kill received");
                let _ = kill(self.pid, Signal::SIGKILL);
                ctx.stop();
                return Response::Empty()
            }
        }
        Response::Empty()
    }
}

pub struct SendCommand(pub WorkerCommand);

impl ResponseType<SendCommand> for Process {
    type Item = ();
    type Error = ();
}

impl Handler<SendCommand> for Process {

    fn handle(&mut self, msg: SendCommand, ctx: &mut FramedContext<Process>)
              -> Response<Self, SendCommand>
    {
        let _ = ctx.send(msg.0);
        Response::Empty()
    }
}

pub struct StartProcess;

impl ResponseType<StartProcess> for Process {
    type Item = ();
    type Error = ();
}

impl Handler<StartProcess> for Process {

    fn handle(&mut self, _: StartProcess, ctx: &mut FramedContext<Process>)
              -> Response<Self, StartProcess>
    {
        let _ = ctx.send(WorkerCommand::start);
        Response::Empty()
    }
}

pub struct PauseProcess;

impl ResponseType<PauseProcess> for Process {
    type Item = ();
    type Error = ();
}

impl Handler<PauseProcess> for Process {

    fn handle(&mut self, _: PauseProcess, ctx: &mut FramedContext<Process>)
              -> Response<Self, PauseProcess>
    {
        let _ = ctx.send(WorkerCommand::pause);
        Response::Empty()
    }
}

pub struct ResumeProcess;

impl ResponseType<ResumeProcess> for Process {
    type Item = ();
    type Error = ();
}

impl Handler<ResumeProcess> for Process {

    fn handle(&mut self, _: ResumeProcess, ctx: &mut FramedContext<Process>)
              -> Response<Self, ResumeProcess>
    {
        let _ = ctx.send(WorkerCommand::resume);
        Response::Empty()
    }
}

pub struct StopProcess;

impl ResponseType<StopProcess> for Process {
    type Item = ();
    type Error = ();
}

impl Handler<StopProcess> for Process {

    fn handle(&mut self, _: StopProcess, ctx: &mut FramedContext<Process>)
              -> Response<Self, StopProcess>
    {
        info!("Stopping worker: (pid:{})", self.pid);
        match self.state {
            ProcessState::Running => {
                let _ = ctx.send(WorkerCommand::stop);

                self.state = ProcessState::Stopping;
                if let Ok(timeout) = Timeout::new(
                    Duration::new(self.shutdown_timeout, 0), Arbiter::handle())
                {
                    ctx.add_future(timeout.map(|_| ProcessMessage::StopTimeout));
                    let _ = kill(self.pid, Signal::SIGTERM);
                } else {
                    // can not create timeout
                    let _ = kill(self.pid, Signal::SIGQUIT);
                    ctx.terminate();
                }
            },
            _ => {
                let _ = kill(self.pid, Signal::SIGQUIT);
                ctx.terminate();
            }
        }
        Response::Empty()
    }
}

pub struct QuitProcess(pub bool);

impl ResponseType<QuitProcess> for Process {
    type Item = ();
    type Error = ();
}

impl Handler<QuitProcess> for Process {

    fn handle(&mut self, msg: QuitProcess, ctx: &mut FramedContext<Process>)
              -> Response<Self, QuitProcess>
    {
        if msg.0 {
            let _ = kill(self.pid, Signal::SIGQUIT);
            self.kill(ctx, true);
        } else {
            self.kill(ctx, false);
            let _ = kill(self.pid, Signal::SIGKILL);
            ctx.terminate();
        }
        Response::Empty()
    }
}

pub struct TransportCodec;

impl Decoder for TransportCodec {
    type Item = ProcessMessage;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let size = {
            if src.len() < 2 {
                return Ok(None)
            }
            BigEndian::read_u16(src.as_ref()) as usize
        };

        if src.len() >= size + 2 {
            src.split_to(2);
            let buf = src.split_to(size);
            Ok(Some(ProcessMessage::Message(json::from_slice::<WorkerMessage>(&buf)?)))
        } else {
            Ok(None)
        }
    }
}

impl Encoder for TransportCodec {
    type Item = WorkerCommand;
    type Error = io::Error;

    fn encode(&mut self, msg: WorkerCommand, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let msg = json::to_string(&msg).unwrap();
        let msg_ref: &[u8] = msg.as_ref();

        dst.reserve(msg_ref.len() + 2);
        dst.put_u16::<BigEndian>(msg_ref.len() as u16);
        dst.put(msg_ref);

        Ok(())
    }
}
