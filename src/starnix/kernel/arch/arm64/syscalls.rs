// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::syscalls::do_clone;
use crate::task::CurrentTask;
use crate::vfs::syscalls::sys_renameat2;
use crate::vfs::FdNumber;
use starnix_sync::{Locked, Unlocked};
use starnix_uapi::errors::Errno;
use starnix_uapi::user_address::{UserAddress, UserCString, UserRef};
use starnix_uapi::{clone_args, tid_t, CSIGNAL};

/// The parameter order for `clone` varies by architecture.
pub fn sys_clone(
    locked: &mut Locked<Unlocked>,
    current_task: &mut CurrentTask,
    flags: u64,
    user_stack: UserAddress,
    user_parent_tid: UserRef<tid_t>,
    user_tls: UserAddress,
    user_child_tid: UserRef<tid_t>,
) -> Result<tid_t, Errno> {
    // Our flags parameter uses the low 8 bits (CSIGNAL mask) of flags to indicate the exit
    // signal. The CloneArgs struct separates these as `flags` and `exit_signal`.
    do_clone(
        locked,
        current_task,
        &clone_args {
            flags: flags & !(CSIGNAL as u64),
            child_tid: user_child_tid.addr().ptr() as u64,
            parent_tid: user_parent_tid.addr().ptr() as u64,
            exit_signal: flags & (CSIGNAL as u64),
            stack: user_stack.ptr() as u64,
            tls: user_tls.ptr() as u64,
            ..Default::default()
        },
    )
}

pub fn sys_renameat(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    old_dir_fd: FdNumber,
    old_user_path: UserCString,
    new_dir_fd: FdNumber,
    new_user_path: UserCString,
) -> Result<(), Errno> {
    sys_renameat2(locked, current_task, old_dir_fd, old_user_path, new_dir_fd, new_user_path, 0)
}

// Syscalls for arch32 usage
#[cfg(feature = "arch32")]
mod arch32 {
    use crate::task::syscalls::do_clone;
    use crate::task::CurrentTask;
    use linux_uapi::clone_args;
    use starnix_logging::track_stub;
    use starnix_sync::{Locked, Unlocked};
    use starnix_uapi::errors::Errno;
    use starnix_uapi::signals::SIGCHLD;
    use starnix_uapi::user_address::UserAddress;
    use starnix_uapi::{tid_t, CLONE_VFORK, CLONE_VM};

    #[allow(non_snake_case)]
    pub fn sys_arch32_ARM_set_tls(
        _locked: &mut Locked<Unlocked>,
        current_task: &mut CurrentTask,
        addr: UserAddress,
    ) -> Result<(), Errno> {
        current_task.thread_state.registers.set_thread_pointer_register(addr.ptr() as u64);
        Ok(())
    }

    #[allow(non_snake_case)]
    pub fn sys_arch32_ARM_cacheflush(
        _locked: &mut Locked<Unlocked>,
        _current_task: &mut CurrentTask,
        _start_addr: UserAddress,
        _end_addr: UserAddress,
        _: u64,
    ) -> Result<(), Errno> {
        track_stub!(TODO("https://fxbug.dev/415739883"), "Implement ARM_cacheflush syscall");
        Ok(())
    }

    pub fn sys_arch32_vfork(
        locked: &mut Locked<Unlocked>,
        current_task: &mut CurrentTask,
    ) -> Result<tid_t, Errno> {
        do_clone(
            locked,
            current_task,
            &clone_args {
                flags: (CLONE_VFORK | CLONE_VM) as u64,
                exit_signal: SIGCHLD.number() as u64,
                ..Default::default()
            },
        )
    }
}

#[cfg(feature = "arch32")]
pub use arch32::*;
