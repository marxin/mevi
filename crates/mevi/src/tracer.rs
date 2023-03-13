use std::{
    borrow::Cow,
    collections::HashMap,
    ops::Range,
    os::{fd::AsRawFd, unix::process::CommandExt},
    process::Command,
    sync::mpsc,
};

use color_eyre::Result;
use nix::{
    sys::{
        ptrace,
        signal::Signal,
        wait::{waitpid, WaitStatus},
    },
    unistd::Pid,
};
use owo_colors::OwoColorize;
use tracing::{debug, info, trace, warn};

use crate::{
    ConnectSource, MapGuard, MemState, MeviEvent, PendingUffdsHandle, TraceeId, TraceePayload,
};

pub(crate) fn run(puh: PendingUffdsHandle, tx: mpsc::SyncSender<MeviEvent>) {
    Tracer::new(puh, tx).unwrap().run().unwrap();
}

struct Tracer {
    puh: PendingUffdsHandle,
    tx: mpsc::SyncSender<MeviEvent>,
    tracees: HashMap<TraceeId, Tracee>,
    next_parent: Option<TraceeId>,
}

struct Mapped {
    range: Range<usize>,
    resident: MemState,
}

impl Tracer {
    fn new(puh: PendingUffdsHandle, tx: mpsc::SyncSender<MeviEvent>) -> Result<Self> {
        let mut args = std::env::args();
        // skip our own name
        args.next().unwrap();

        let mut cmd = Command::new(args.next().unwrap());
        for arg in args {
            cmd.arg(arg);
        }

        let exe_path = std::fs::canonicalize(std::env::current_exe()?)?;
        debug!("exe_path = {}", exe_path.display());
        let exe_dir_path = exe_path.parent().unwrap();
        debug!("exe_dir_path = {}", exe_dir_path.display());
        let preload_path = exe_dir_path.join("libmevi_preload.so");
        debug!("Setting LD_PRELOAD to {}", preload_path.display());

        cmd.env("LD_PRELOAD", preload_path);
        unsafe {
            cmd.pre_exec(|| {
                ptrace::traceme()?;
                Ok(())
            });
        }

        let child = cmd.spawn().unwrap();

        let pid = Pid::from_raw(child.id() as _);
        std::mem::forget(child);

        let res = waitpid(pid, None)?;
        trace!("first waitpid: {res:?}");

        ptrace::setoptions(
            pid,
            ptrace::Options::PTRACE_O_TRACESYSGOOD
                | ptrace::Options::PTRACE_O_TRACECLONE
                | ptrace::Options::PTRACE_O_TRACEFORK
                | ptrace::Options::PTRACE_O_TRACEVFORK,
        )?;
        ptrace::syscall(pid, None)?;

        Ok(Self {
            puh,
            tx,
            tracees: Default::default(),
            next_parent: None,
        })
    }

    fn run(&mut self) -> Result<()> {
        loop {
            let wait_status = match waitpid(None, None) {
                Ok(s) => s,
                Err(e) => {
                    if e == nix::errno::Errno::ECHILD {
                        info!("no more children, exiting");
                        std::process::exit(0);
                    } else {
                        panic!("waitpid failed: {}", e);
                    }
                }
            };

            trace!("wait_status: {:?}", wait_status.yellow());
            match wait_status {
                WaitStatus::Stopped(pid, sig) => {
                    let tid: TraceeId = pid.into();
                    debug!("{tid} caught sig {sig}");
                    match sig {
                        Signal::SIGTRAP => {
                            // probably ptrace stuff?
                            ptrace::syscall(pid, None)?;
                        }
                        Signal::SIGSTOP => {
                            // probably a new thread after clone?
                            info!("{tid} is that a new thread? (just got SIGSTOP)");

                            if let Some(ptid) = self.next_parent.take() {
                                info!("{tid} might be a child of {ptid}, methinks");

                                if let Some(uffd) = self
                                    .puh
                                    .lock()
                                    .unwrap()
                                    .get_mut(&ptid)
                                    .and_then(|q| q.pop_front())
                                {
                                    info!(
                                        "{tid}<={ptid} well we got uffd {} for this",
                                        uffd.as_raw_fd()
                                    );
                                    self.tx
                                        .send(MeviEvent::TraceeEvent(
                                            tid,
                                            TraceePayload::Connected {
                                                source: ConnectSource::Fork,
                                                uffd: uffd.as_raw_fd(),
                                            },
                                        ))
                                        .unwrap();
                                    info!(
                                        "{tid}<={ptid} well we got uffd {} for this... and sent!",
                                        uffd.as_raw_fd()
                                    );
                                    std::thread::sleep(std::time::Duration::from_millis(10));
                                } else {
                                    info!("{tid}<={ptid} well we don't have a uffd for this");
                                }
                            }

                            ptrace::syscall(pid, None)?;
                        }
                        _ => {
                            // probably not ptrace stuff, forward the signal?
                            ptrace::syscall(pid, sig)?;
                        }
                    }
                    continue;
                }
                WaitStatus::Exited(pid, status) => {
                    if status == 0 {
                        debug!("{pid} exited with status {status}");
                    } else {
                        warn!("{pid} exited with non-zero status {status}");
                    }
                    let ev = MeviEvent::TraceeEvent(pid.into(), TraceePayload::Exit);
                    self.tx.send(ev).unwrap();
                }
                WaitStatus::PtraceSyscall(pid) => {
                    let tid: TraceeId = pid.into();
                    debug!("{tid} in sys_enter / sys_exit");
                    let tracee = self.tracees.entry(tid).or_insert_with(|| Tracee {
                        was_in_syscall: false,
                        tid,
                        heap_range: None,
                    });
                    if tracee.was_in_syscall {
                        tracee.was_in_syscall = false;
                        if let Some(Mapped { range, resident }) =
                            tracee.on_sys_exit(&mut self.tx)?
                        {
                            let (tx, rx) = mpsc::channel();
                            let ev = MeviEvent::TraceeEvent(
                                tid,
                                TraceePayload::Map {
                                    range,
                                    state: resident,
                                    _guard: MapGuard { _inner: Some(tx) },
                                },
                            );
                            self.tx.send(ev).unwrap();

                            // this will fail, because it's been dropped. but it'll
                            // wait until it's dropped, which is what we want
                            _ = rx.recv();
                        }
                        if let Err(e) = ptrace::syscall(pid, None) {
                            if e == nix::errno::Errno::ESRCH {
                                // the process has exited, we don't care
                                info!(
                                    "{pid} exited while we were spying on its syscalls, that's ok"
                                );
                            }
                        }
                    } else {
                        tracee.was_in_syscall = true;
                        match ptrace::syscall(pid, None) {
                            Ok(_) => {}
                            Err(e) => {
                                if e == nix::errno::Errno::ESRCH {
                                    // the process has exited, we don't care
                                    info!(
                                        "{tid} exited while we were spying on its syscalls, that's ok"
                                    );
                                } else {
                                    panic!("{tid} ptrace::syscall failed: {e}");
                                }
                            }
                        }
                    }
                }
                WaitStatus::PtraceEvent(pid, sig, event) => {
                    let tid: TraceeId = pid.into();
                    if event == libc::PTRACE_EVENT_FORK {
                        self.next_parent = Some(tid);
                    }

                    let event_name: Cow<'static, str> = match event {
                        libc::PTRACE_EVENT_CLONE => "clone".into(),
                        libc::PTRACE_EVENT_FORK => "fork".into(),
                        libc::PTRACE_EVENT_VFORK => "vfork".into(),
                        other => format!("unknown event {}", other).into(),
                    };

                    info!("{tid} got event {event_name} with sig {sig}");
                    ptrace::syscall(pid, None)?;
                }
                WaitStatus::Signaled(pid, signal, core_dump) => {
                    let tid: TraceeId = pid.into();
                    info!("{tid} was terminated with signal {signal} with, WCOREDUMP({core_dump})");
                }
                other => {
                    panic!("unexpected wait status: {:?}", other);
                }
            }
        }
    }
}

struct Tracee {
    was_in_syscall: bool,
    tid: TraceeId,
    heap_range: Option<Range<usize>>,
}

impl Tracee {
    fn on_sys_exit(&mut self, tx: &mut mpsc::SyncSender<MeviEvent>) -> Result<Option<Mapped>> {
        let regs = ptrace::getregs(self.tid.into())?;
        trace!("on sys_exit: {regs:?}");
        let ret = regs.rax as usize;

        match regs.orig_rax as i64 {
            libc::SYS_execve | libc::SYS_execveat => {
                info!("{} is about to execve, getting rid of heap_range", self.tid);

                self.heap_range = None;
                tx.send(MeviEvent::TraceeEvent(self.tid, TraceePayload::Execve))
                    .unwrap();

                return Ok(None);
            }
            libc::SYS_mmap => {
                let fd = regs.r8 as i32;
                let addr_in = regs.rdi;
                let len = regs.rsi as usize;

                if fd == -1 && addr_in == 0 {
                    return Ok(Some(Mapped {
                        range: ret..ret + len,
                        resident: MemState::NotResident,
                    }));
                }
            }
            libc::SYS_brk => {
                if regs.rdi == 0 {
                    // just a query: remember the top of the heap
                    if self.heap_range.is_none() {
                        self.heap_range = Some(ret..ret);
                        info!("{} initial heap_range: {:x?}", self.tid, self.heap_range);
                    }
                } else if let Some(heap_range) = self.heap_range.as_mut() {
                    // either growing or shrinking the heap,
                    // and we know the previous top
                    let old_top = heap_range.end;
                    heap_range.end = ret;

                    if heap_range.end > old_top {
                        // heap just grew - shrinking will be handled by
                        // userfaultfd
                        return Ok(Some(Mapped {
                            range: old_top..heap_range.end,
                            resident: MemState::Resident,
                        }));
                    }
                }
            }
            _ => {
                // let's ignore those
            }
        }

        Ok(None)
    }
}
