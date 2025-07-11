// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::signals::SignalDetail;
use crate::task::{CurrentTask, EventHandler, WaitCanceler, Waiter};
use crate::vfs::buffers::{InputBuffer, OutputBuffer};
use crate::vfs::{
    fileops_impl_nonseekable, fileops_impl_noop_sync, Anon, FileHandle, FileObject, FileOps,
};
use starnix_sync::{FileOpsCore, Locked, Mutex};
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::signals::SigSet;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{errno, error, signalfd_siginfo, SFD_NONBLOCK};
use zerocopy::IntoBytes;

pub struct SignalFd {
    mask: Mutex<SigSet>,
}

impl SignalFd {
    pub fn new_file(current_task: &CurrentTask, mask: SigSet, flags: u32) -> FileHandle {
        let mut open_flags = OpenFlags::RDONLY;
        if flags & SFD_NONBLOCK != 0 {
            open_flags |= OpenFlags::NONBLOCK;
        }
        Anon::new_private_file(
            current_task,
            Box::new(SignalFd { mask: Mutex::new(mask) }),
            open_flags,
            "[signalfd]",
        )
    }

    pub fn set_mask(&self, mask: SigSet) {
        *self.mask.lock() = mask;
    }
}

impl FileOps for SignalFd {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        locked: &mut Locked<FileOpsCore>,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        file.blocking_op(locked, current_task, FdEvents::POLLIN | FdEvents::POLLHUP, None, |_| {
            let mask = *self.mask.lock();
            let data_len = data.available();
            let mut buf = Vec::new();
            while buf.len() + std::mem::size_of::<signalfd_siginfo>() <= data_len {
                let signal = current_task
                    .write()
                    .take_signal_with_mask(!mask)
                    .ok_or_else(|| errno!(EAGAIN))?;
                let mut siginfo = signalfd_siginfo {
                    ssi_signo: signal.signal.number(),
                    ssi_errno: signal.errno,
                    ssi_code: signal.code,
                    ..Default::default()
                };
                // Any future variants of SignalDetail need a match arm here that copies the relevant
                // fields into the signalfd_siginfo.
                match signal.detail {
                    SignalDetail::None => {}
                    SignalDetail::Kill { pid, uid } => {
                        siginfo.ssi_pid = pid as u32;
                        siginfo.ssi_uid = uid;
                    }
                    SignalDetail::SIGCHLD { pid, uid, status } => {
                        siginfo.ssi_pid = pid as u32;
                        siginfo.ssi_uid = uid;
                        siginfo.ssi_status = status;
                    }
                    SignalDetail::SigFault { addr } => {
                        siginfo.ssi_addr = addr;
                    }
                    SignalDetail::SIGSYS { call_addr, syscall, arch } => {
                        siginfo.ssi_call_addr = call_addr.into();
                        siginfo.ssi_syscall = syscall;
                        siginfo.ssi_arch = arch;
                    }
                    SignalDetail::Raw { data } => {
                        // these offsets are taken from the gVisor offsets in the SignalInfo struct
                        // in //pkg/abi/linux/signal.go and the definition of __sifields in
                        // /usr/include/asm-generic/siginfo.h
                        siginfo.ssi_uid = u32::from_ne_bytes(data[4..8].try_into().unwrap());
                        siginfo.ssi_pid = u32::from_ne_bytes(data[0..4].try_into().unwrap());
                        siginfo.ssi_fd = i32::from_ne_bytes(data[8..12].try_into().unwrap());
                        siginfo.ssi_tid = u32::from_ne_bytes(data[0..4].try_into().unwrap());
                        siginfo.ssi_band = u32::from_ne_bytes(data[0..4].try_into().unwrap());
                        siginfo.ssi_overrun = u32::from_ne_bytes(data[4..8].try_into().unwrap());
                        siginfo.ssi_status = i32::from_ne_bytes(data[8..12].try_into().unwrap());
                        siginfo.ssi_int = i32::from_ne_bytes(data[8..12].try_into().unwrap());
                        siginfo.ssi_ptr = u64::from_ne_bytes(data[8..16].try_into().unwrap());
                        siginfo.ssi_addr = u64::from_ne_bytes(data[0..8].try_into().unwrap());
                        siginfo.ssi_syscall = i32::from_ne_bytes(data[8..12].try_into().unwrap());
                        siginfo.ssi_call_addr = u64::from_ne_bytes(data[0..8].try_into().unwrap());
                        siginfo.ssi_arch = u32::from_ne_bytes(data[12..16].try_into().unwrap());
                        siginfo.ssi_utime = u64::from_ne_bytes(data[12..20].try_into().unwrap());
                        siginfo.ssi_stime = u64::from_ne_bytes(data[20..28].try_into().unwrap());
                    }
                    SignalDetail::Timer { timer } => {
                        siginfo.ssi_tid = timer.timer_id as u32;
                        siginfo.ssi_overrun = timer.overrun_cur() as u32;
                        siginfo.ssi_int = timer.signal_event.value.0 as i32;
                        siginfo.ssi_ptr = timer.signal_event.value.0;
                    }
                }
                buf.extend_from_slice(siginfo.as_bytes());
            }
            data.write_all(&buf)
        })
    }

    fn wait_async(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        let task_state = current_task.read();
        Some(task_state.wait_on_signal_fd_events(waiter, events, handler))
    }

    fn query_events(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        let mut events = FdEvents::empty();
        if current_task.read().is_any_signal_allowed_by_mask(!*self.mask.lock()) {
            events |= FdEvents::POLLIN;
        }
        Ok(events)
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        _data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        error!(EINVAL)
    }
}
