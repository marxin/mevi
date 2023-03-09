use std::{
    cmp::Ordering,
    ops::Range,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd},
        unix::{net::UnixListener, process::CommandExt},
    },
    process::{Child, Command},
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};

use humansize::{make_format, BINARY};
use libc::user_regs_struct;
use nix::{
    errno::Errno,
    sys::{
        ptrace::{self},
        signal::Signal,
        wait::{waitpid, WaitStatus},
    },
    unistd::{sysconf, Pid, SysconfVar},
};
use owo_colors::OwoColorize;
use passfd::FdPassingExt;
use rangemap::RangeMap;
use tracing::{debug, info, trace, warn};
use tracing_subscriber::EnvFilter;
use userfaultfd::Uffd;

use crate::TraceeEvent;

pub(crate) fn run(tx: mpsc::SyncSender<TraceeEvent>, listener: UnixListener) {
    let page_size = sysconf(SysconfVar::PAGE_SIZE).unwrap().unwrap() as usize;

    let (stream, _) = listener.accept().unwrap();
    let uffd = unsafe { Uffd::from_raw_fd(stream.recv_fd().unwrap()) };
    tx.send(TraceeEvent::Connected {
        uffd: uffd.as_raw_fd(),
    })
    .unwrap();

    loop {
        let event = uffd.read_event().unwrap().unwrap();
        match event {
            userfaultfd::Event::Pagefault { addr, .. } => {
                unsafe {
                    loop {
                        match uffd.zeropage(addr, page_size, true) {
                            Ok(_) => {
                                // cool!
                                break;
                            }
                            Err(e) => match e {
                                userfaultfd::Error::ZeropageFailed(errno) => match errno as i32 {
                                    libc::EAGAIN => {
                                        // this is actually fine, just try it again
                                        continue;
                                    }
                                    _ => {
                                        panic!("{e}");
                                    }
                                },
                                _ => unreachable!(),
                            },
                        }
                    }
                }
                let addr = addr as usize;
                tx.send(TraceeEvent::PageIn {
                    range: addr..addr + page_size,
                })
                .unwrap();
            }
            userfaultfd::Event::Remap { from, to, len } => {
                let from = from as usize;
                let to = to as usize;
                tx.send(TraceeEvent::Remap {
                    old_range: from..from + len,
                    new_range: to..to + len,
                })
                .unwrap();
            }
            userfaultfd::Event::Remove { start, end } => {
                let start = start as usize;
                let end = end as usize;
                tx.send(TraceeEvent::PageOut { range: start..end }).unwrap();
            }
            userfaultfd::Event::Unmap { start, end } => {
                let start = start as usize;
                let end = end as usize;
                tx.send(TraceeEvent::Unmap { range: start..end }).unwrap();
            }
            _ => {
                warn!("Unexpected event: {:?}", event);
            }
        }
    }
}
