// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// *** DO NOT ADD NEW DEFINTIONS IN THIS FILE ***
//
// This file will be removed. New types or constants should not be added in
// this file. Instead Linux UAPI definitions should be provided by the
// `linux_uapi` create, where they are generated from C headers.

#![allow(dead_code)]
#![allow(non_camel_case_types)]
#![allow(non_upper_case_globals)]

use super::user_address::UserAddress;
use linux_uapi as uapi;
pub use uapi::*;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub use uapi::__SIGRTMIN as SIGRTMIN;

pub const SIG_DFL: uaddr = uaddr { addr: 0 };
pub const SIG_IGN: uaddr = uaddr { addr: 1 };
pub const SIG_ERR: uaddr = uaddr { addr: u64::MAX };

// Note that all the socket-related numbers below are defined in sys/socket.h, which is why they
// can't be generated by bindgen.

pub const AF_UNSPEC: uapi::__kernel_sa_family_t = 0;
pub const AF_UNIX: uapi::__kernel_sa_family_t = 1;
pub const AF_INET: uapi::__kernel_sa_family_t = 2;
pub const AF_NETLINK: uapi::__kernel_sa_family_t = 16;
pub const AF_PACKET: uapi::__kernel_sa_family_t = 17;
pub const AF_INET6: uapi::__kernel_sa_family_t = 10;
pub const AF_VSOCK: uapi::__kernel_sa_family_t = 40;

pub const SOL_IP: u32 = 0;
pub const SOL_IPV6: u32 = 41;

pub const SOCK_CLOEXEC: u32 = O_CLOEXEC;
pub const SOCK_NONBLOCK: u32 = O_NONBLOCK;

pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;
pub const SOCK_RAW: u32 = 3;
pub const SOCK_RDM: u32 = 4;
pub const SOCK_SEQPACKET: u32 = 5;
pub const SOCK_DCCP: u32 = 6;
pub const SOCK_PACKET: u32 = 10;

pub const SHUT_RD: u32 = 0;
pub const SHUT_WR: u32 = 1;
pub const SHUT_RDWR: u32 = 2;

pub const SCM_RIGHTS: u32 = 1;
pub const SCM_CREDENTIALS: u32 = 2;
pub const SCM_SECURITY: u32 = 3;
/// The maximum number of bytes that the file descriptor array can occupy.
pub const SCM_MAX_FD: usize = 253;

pub const MSG_PEEK: u32 = 2;
pub const MSG_DONTROUTE: u32 = 4;
pub const MSG_TRYHARD: u32 = 4;
pub const MSG_CTRUNC: u32 = 8;
pub const MSG_PROXY: u32 = 0x10;
pub const MSG_TRUNC: u32 = 0x20;
pub const MSG_DONTWAIT: u32 = 0x40;
pub const MSG_EOR: u32 = 0x80;
pub const MSG_WAITALL: u32 = 0x100;
pub const MSG_FIN: u32 = 0x200;
pub const MSG_SYN: u32 = 0x400;
pub const MSG_CONFIRM: u32 = 0x800;
pub const MSG_RST: u32 = 0x1000;
pub const MSG_ERRQUEUE: u32 = 0x2000;
pub const MSG_NOSIGNAL: u32 = 0x4000;
pub const MSG_MORE: u32 = 0x8000;
pub const MSG_WAITFORONE: u32 = 0x10000;
pub const MSG_BATCH: u32 = 0x40000;
pub const MSG_FASTOPEN: u32 = 0x20000000;
pub const MSG_CMSG_CLOEXEC: u32 = 0x40000000;
pub const MSG_EOF: u32 = MSG_FIN;
pub const MSG_CMSG_COMPAT: u32 = 0;

pub const MNT_FORCE: u32 = 1;
pub const MNT_DETACH: u32 = 2;
pub const MNT_EXPIRE: u32 = 4;
pub const UMOUNT_NOFOLLOW: u32 = 8;

pub const EFD_CLOEXEC: u32 = O_CLOEXEC;
pub const EFD_NONBLOCK: u32 = O_NONBLOCK;
pub const EFD_SEMAPHORE: u32 = 1;

#[derive(Debug, Default, Copy, Clone, IntoBytes, KnownLayout, FromBytes, Immutable)]
#[repr(C)]
pub struct pselect6_sigmask {
    pub ss: UserAddress,
    pub ss_len: usize,
}
