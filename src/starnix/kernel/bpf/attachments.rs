// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(https://github.com/rust-lang/rust/issues/39371): remove
#![allow(non_upper_case_globals)]

use crate::bpf::fs::{get_bpf_object, BpfHandle};
use crate::bpf::program::Program;
use crate::task::CurrentTask;
use crate::vfs::socket::{SocketAddress, SocketDomain, SocketProtocol, SocketType};
use crate::vfs::FdNumber;
use ebpf::{EbpfProgram, EbpfProgramContext, ProgramArgument, Type};
use ebpf_api::{
    AttachType, BaseEbpfRunContext, PinnedMap, ProgramType, BPF_SOCK_ADDR_TYPE, BPF_SOCK_TYPE,
};
use fidl_fuchsia_net_filter as fnet_filter;
use fuchsia_component::client::connect_to_protocol_sync;
use starnix_logging::{log_error, log_warn, track_stub};
use starnix_sync::{BpfPrograms, FileOpsCore, Locked, OrderedRwLock, Unlocked};
use starnix_syscalls::{SyscallResult, SUCCESS};
use starnix_uapi::errors::Errno;
use starnix_uapi::{
    bpf_attr__bindgen_ty_6, bpf_sock, bpf_sock_addr, errno, error, CGROUP2_SUPER_MAGIC,
};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, OnceLock};
use zerocopy::FromBytes;

pub type BpfAttachAttr = bpf_attr__bindgen_ty_6;

fn check_root_cgroup_fd(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    cgroup_fd: FdNumber,
) -> Result<(), Errno> {
    let file = current_task.files.get(cgroup_fd)?;

    // Check that `cgroup_fd` is from the CGROUP2 file system.
    let is_cgroup =
        file.node().fs().statfs(locked, current_task)?.f_type == CGROUP2_SUPER_MAGIC as i64;
    if !is_cgroup {
        log_warn!("bpf_prog_attach(BPF_PROG_ATTACH) is called with an invalid cgroup2 FD.");
        return error!(EINVAL);
    }

    // Currently cgroup attachments are supported only for the root cgroup.
    // TODO(https://fxbug.dev//388077431) Allow attachments to any cgroup once cgroup
    // hierarchy is moved to starnix_core.
    let is_root = file
        .node()
        .fs()
        .maybe_root()
        .map(|root| Arc::ptr_eq(&root.node, file.node()))
        .unwrap_or(false);
    if !is_root {
        log_warn!("bpf_prog_attach(BPF_PROG_ATTACH) is supported only for root cgroup.");
        return error!(EINVAL);
    }

    Ok(())
}

pub fn bpf_prog_attach(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    attr: BpfAttachAttr,
) -> Result<SyscallResult, Errno> {
    // SAFETY: reading i32 field from a union is always safe.
    let bpf_fd = FdNumber::from_raw(attr.attach_bpf_fd as i32);
    let object = get_bpf_object(current_task, bpf_fd)?;
    if matches!(object, BpfHandle::ProgramStub(_)) {
        log_warn!("Stub program. Faking successful attach");
        return Ok(SUCCESS);
    }
    let program = object.as_program()?.clone();
    let attach_type = AttachType::from(attr.attach_type);

    let program_type = program.info.program_type;
    if attach_type.get_program_type() != program_type {
        log_warn!(
            "bpf_prog_attach(BPF_PROG_ATTACH): program not compatible with attach_type \
                   attach_type: {attach_type:?}, program_type: {program_type:?}"
        );
        return error!(EINVAL);
    }

    if !attach_type.is_compatible_with_expected_attach_type(program.info.expected_attach_type) {
        log_warn!(
            "bpf_prog_attach(BPF_PROG_ATTACH): expected_attach_type didn't match attach_type \
                   expected_attach_type: {:?}, attach_type: {:?}",
            program.info.expected_attach_type,
            attach_type
        );
        return error!(EINVAL);
    }

    // SAFETY: reading i32 field from a union is always safe.
    let target_fd = unsafe { attr.__bindgen_anon_1.target_fd };
    let target_fd = FdNumber::from_raw(target_fd as i32);

    current_task.kernel().ebpf_attachments.attach_prog(
        locked,
        current_task,
        attach_type,
        target_fd,
        program,
    )
}

pub fn bpf_prog_detach(
    locked: &mut Locked<Unlocked>,
    current_task: &CurrentTask,
    attr: BpfAttachAttr,
) -> Result<SyscallResult, Errno> {
    let attach_type = AttachType::from(attr.attach_type);

    // SAFETY: reading i32 field from a union is always safe.
    let target_fd = unsafe { attr.__bindgen_anon_1.target_fd };
    let target_fd = FdNumber::from_raw(target_fd as i32);

    current_task.kernel().ebpf_attachments.detach_prog(locked, current_task, attach_type, target_fd)
}

// Wrapper for `bpf_sock_addr` used to implement `ProgramArgument` trait.
#[repr(C)]
#[derive(Default)]
pub struct BpfSockAddr(bpf_sock_addr);

impl Deref for BpfSockAddr {
    type Target = bpf_sock_addr;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BpfSockAddr {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl ProgramArgument for &'_ mut BpfSockAddr {
    fn get_type() -> &'static Type {
        &*BPF_SOCK_ADDR_TYPE
    }
}

// Context for eBPF programs of type BPF_PROG_TYPE_CGROUP_SOCKADDR.
struct SockAddrProgram(EbpfProgram<SockAddrProgram>);

impl EbpfProgramContext for SockAddrProgram {
    type RunContext<'a> = BaseEbpfRunContext<'a>;
    type Packet<'a> = ();
    type Arg1<'a> = &'a mut BpfSockAddr;
    type Arg2<'a> = ();
    type Arg3<'a> = ();
    type Arg4<'a> = ();
    type Arg5<'a> = ();

    type Map = PinnedMap;
}

#[derive(Debug, PartialEq, Eq)]
pub enum SockAddrProgramResult {
    Allow,
    Block,
}

impl SockAddrProgram {
    fn run(&self, addr: &mut BpfSockAddr) -> SockAddrProgramResult {
        let mut run_context = BaseEbpfRunContext::<'_>::default();
        if self.0.run_with_1_argument(&mut run_context, addr) == 0 {
            SockAddrProgramResult::Block
        } else {
            SockAddrProgramResult::Allow
        }
    }
}

type AttachedSockAddrProgramCell = OrderedRwLock<Option<SockAddrProgram>, BpfPrograms>;

// Wrapper for `bpf_sock` used to implement `ProgramArgument` trait.
#[repr(C)]
#[derive(Default)]
pub struct BpfSock(bpf_sock);

impl Deref for BpfSock {
    type Target = bpf_sock;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for BpfSock {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl ProgramArgument for &'_ mut BpfSock {
    fn get_type() -> &'static Type {
        &*BPF_SOCK_TYPE
    }
}

// Context for eBPF programs of type BPF_PROG_TYPE_CGROUP_SOCK.
struct SockProgram(EbpfProgram<SockProgram>);

impl EbpfProgramContext for SockProgram {
    type RunContext<'a> = BaseEbpfRunContext<'a>;
    type Packet<'a> = ();
    type Arg1<'a> = &'a mut BpfSock;
    type Arg2<'a> = ();
    type Arg3<'a> = ();
    type Arg4<'a> = ();
    type Arg5<'a> = ();

    type Map = PinnedMap;
}

#[derive(Debug, PartialEq, Eq)]
pub enum SockProgramResult {
    Allow,
    Block,
}

impl SockProgram {
    fn run(&self, sock: &mut BpfSock) -> SockProgramResult {
        let mut run_context = BaseEbpfRunContext::<'_>::default();
        if self.0.run_with_1_argument(&mut run_context, sock) == 0 {
            SockProgramResult::Block
        } else {
            SockProgramResult::Allow
        }
    }
}

type AttachedSockProgramCell = OrderedRwLock<Option<SockProgram>, BpfPrograms>;

#[derive(Default)]
pub struct CgroupEbpfProgramSet {
    inet4_bind: AttachedSockAddrProgramCell,
    inet6_bind: AttachedSockAddrProgramCell,
    inet4_connect: AttachedSockAddrProgramCell,
    inet6_connect: AttachedSockAddrProgramCell,
    udp4_sendmsg: AttachedSockAddrProgramCell,
    udp6_sendmsg: AttachedSockAddrProgramCell,
    sock_create: AttachedSockProgramCell,
    sock_release: AttachedSockProgramCell,
}

pub enum SockAddrOp {
    Bind,
    Connect,
    UdpSendMsg,
}

#[derive(Debug)]
pub enum SockOp {
    Create,
    Release,
}

impl CgroupEbpfProgramSet {
    fn get_sock_addr_program(
        &self,
        attach_type: AttachType,
    ) -> Result<&AttachedSockAddrProgramCell, Errno> {
        assert!(attach_type.is_cgroup());

        match attach_type {
            AttachType::CgroupInet4Bind => Ok(&self.inet4_bind),
            AttachType::CgroupInet6Bind => Ok(&self.inet6_bind),
            AttachType::CgroupInet4Connect => Ok(&self.inet4_connect),
            AttachType::CgroupInet6Connect => Ok(&self.inet6_connect),
            AttachType::CgroupUdp4Sendmsg => Ok(&self.udp4_sendmsg),
            AttachType::CgroupUdp6Sendmsg => Ok(&self.udp6_sendmsg),
            _ => error!(ENOTSUP),
        }
    }

    fn get_sock_program(&self, attach_type: AttachType) -> Result<&AttachedSockProgramCell, Errno> {
        assert!(attach_type.is_cgroup());

        match attach_type {
            AttachType::CgroupInetSockCreate => Ok(&self.sock_create),
            AttachType::CgroupInetSockRelease => Ok(&self.sock_release),
            _ => error!(ENOTSUP),
        }
    }

    pub fn run_sock_addr_prog(
        &self,
        locked: &mut Locked<FileOpsCore>,
        op: SockAddrOp,
        domain: SocketDomain,
        socket_type: SocketType,
        protocol: SocketProtocol,
        socket_address: &SocketAddress,
    ) -> Result<SockAddrProgramResult, Errno> {
        let prog_cell = match (domain, op) {
            (SocketDomain::Inet, SockAddrOp::Bind) => Some(&self.inet4_bind),
            (SocketDomain::Inet6, SockAddrOp::Bind) => Some(&self.inet6_bind),
            (SocketDomain::Inet, SockAddrOp::Connect) => Some(&self.inet4_connect),
            (SocketDomain::Inet6, SockAddrOp::Connect) => Some(&self.inet6_connect),
            (SocketDomain::Inet, SockAddrOp::UdpSendMsg) => Some(&self.udp4_sendmsg),
            (SocketDomain::Inet6, SockAddrOp::UdpSendMsg) => Some(&self.udp6_sendmsg),
            _ => None,
        };
        let prog_guard = prog_cell.map(|cell| cell.read(locked));
        let Some(prog) = prog_guard.as_ref().and_then(|guard| guard.as_ref()) else {
            return Ok(SockAddrProgramResult::Allow);
        };

        let mut bpf_sockaddr = BpfSockAddr::default();
        bpf_sockaddr.family = domain.as_raw().into();
        bpf_sockaddr.type_ = socket_type.as_raw();
        bpf_sockaddr.protocol = protocol.as_raw();

        match socket_address {
            SocketAddress::Inet(addr) => {
                let sockaddr =
                    linux_uapi::sockaddr_in::ref_from_prefix(&addr).map_err(|_| errno!(EINVAL))?.0;
                bpf_sockaddr.user_family = linux_uapi::AF_INET;
                bpf_sockaddr.user_port = sockaddr.sin_port.into();
                bpf_sockaddr.user_ip4 = sockaddr.sin_addr.s_addr;
            }
            SocketAddress::Inet6(addr) => {
                let sockaddr =
                    linux_uapi::sockaddr_in6::ref_from_prefix(&addr).map_err(|_| errno!(EINVAL))?.0;
                bpf_sockaddr.user_family = linux_uapi::AF_INET6;
                bpf_sockaddr.user_port = sockaddr.sin6_port.into();
                // SAFETY: reading an array of u32 from a union is safe.
                bpf_sockaddr.user_ip6 = unsafe { sockaddr.sin6_addr.in6_u.u6_addr32 };
            }
            _ => (),
        };

        Ok(prog.run(&mut bpf_sockaddr))
    }

    pub fn run_sock_prog(
        &self,
        locked: &mut Locked<FileOpsCore>,
        op: SockOp,
        domain: SocketDomain,
        socket_type: SocketType,
        protocol: SocketProtocol,
    ) -> SockProgramResult {
        let prog_cell = match op {
            SockOp::Create => &self.sock_create,
            SockOp::Release => &self.sock_release,
        };
        let prog_guard = prog_cell.read(locked);
        let Some(prog) = prog_guard.as_ref() else {
            return SockProgramResult::Allow;
        };

        let mut bpf_sock = BpfSock::default();
        bpf_sock.family = domain.as_raw().into();
        bpf_sock.type_ = socket_type.as_raw();
        bpf_sock.protocol = protocol.as_raw();

        prog.run(&mut bpf_sock)
    }
}

fn attach_type_to_netstack_hook(attach_type: AttachType) -> Option<fnet_filter::SocketHook> {
    let hook = match attach_type {
        AttachType::CgroupInetEgress => fnet_filter::SocketHook::Egress,
        AttachType::CgroupInetIngress => fnet_filter::SocketHook::Ingress,
        _ => return None,
    };
    Some(hook)
}

// Defined a location where eBPF programs can be attached.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AttachLocation {
    // Attached in Starnix kernel.
    Kernel,

    // Attached in Netstack.
    Netstack,

    // The program type is not attached, but attach operation should not fail
    // to avoid breaking apps that depend it.
    Stub,
}

impl TryFrom<AttachType> for AttachLocation {
    type Error = Errno;

    fn try_from(attach_type: AttachType) -> Result<Self, Self::Error> {
        match attach_type {
            AttachType::CgroupInet4Bind
            | AttachType::CgroupInet6Bind
            | AttachType::CgroupInet4Connect
            | AttachType::CgroupInet6Connect
            | AttachType::CgroupUdp4Sendmsg
            | AttachType::CgroupUdp6Sendmsg
            | AttachType::CgroupInetSockCreate
            | AttachType::CgroupInetSockRelease => Ok(AttachLocation::Kernel),

            AttachType::CgroupInetEgress | AttachType::CgroupInetIngress => {
                Ok(AttachLocation::Netstack)
            }

            AttachType::CgroupGetsockopt
            | AttachType::CgroupSetsockopt
            | AttachType::CgroupUdp4Recvmsg
            | AttachType::CgroupUdp6Recvmsg => {
                track_stub!(TODO("https://fxbug.dev/322873416"), "BPF_PROG_ATTACH", attach_type);

                // Fake success to avoid breaking apps that depends on the attachments above.
                // TODO(https://fxbug.dev/391380601) Actually implement these attachments.
                Ok(AttachLocation::Stub)
            }

            AttachType::CgroupDevice
            | AttachType::CgroupInet4Getpeername
            | AttachType::CgroupInet4Getsockname
            | AttachType::CgroupInet4PostBind
            | AttachType::CgroupInet6Getpeername
            | AttachType::CgroupInet6Getsockname
            | AttachType::CgroupInet6PostBind
            | AttachType::CgroupSysctl
            | AttachType::CgroupUnixConnect
            | AttachType::CgroupUnixGetpeername
            | AttachType::CgroupUnixGetsockname
            | AttachType::CgroupUnixRecvmsg
            | AttachType::CgroupUnixSendmsg
            | AttachType::CgroupSockOps
            | AttachType::SkSkbStreamParser
            | AttachType::SkSkbStreamVerdict
            | AttachType::SkMsgVerdict
            | AttachType::LircMode2
            | AttachType::FlowDissector
            | AttachType::TraceRawTp
            | AttachType::TraceFentry
            | AttachType::TraceFexit
            | AttachType::ModifyReturn
            | AttachType::LsmMac
            | AttachType::TraceIter
            | AttachType::XdpDevmap
            | AttachType::XdpCpumap
            | AttachType::SkLookup
            | AttachType::Xdp
            | AttachType::SkSkbVerdict
            | AttachType::SkReuseportSelect
            | AttachType::SkReuseportSelectOrMigrate
            | AttachType::PerfEvent
            | AttachType::TraceKprobeMulti
            | AttachType::LsmCgroup
            | AttachType::StructOps
            | AttachType::Netfilter
            | AttachType::TcxIngress
            | AttachType::TcxEgress
            | AttachType::TraceUprobeMulti
            | AttachType::NetkitPrimary
            | AttachType::NetkitPeer
            | AttachType::TraceKprobeSession => {
                track_stub!(TODO("https://fxbug.dev/322873416"), "BPF_PROG_ATTACH", attach_type);
                error!(ENOTSUP)
            }

            AttachType::Unspecified | AttachType::Invalid(_) => {
                error!(EINVAL)
            }
        }
    }
}

#[derive(Default)]
pub struct EbpfAttachments {
    root_cgroup: CgroupEbpfProgramSet,
    socket_control: OnceLock<fnet_filter::SocketControlSynchronousProxy>,
}

impl EbpfAttachments {
    pub fn root_cgroup(&self) -> &CgroupEbpfProgramSet {
        &self.root_cgroup
    }

    fn socket_control(&self) -> &fnet_filter::SocketControlSynchronousProxy {
        self.socket_control.get_or_init(|| {
            connect_to_protocol_sync::<fnet_filter::SocketControlMarker>()
                .expect("Failed to connect to fuchsia.net.filter.SocketControl.")
        })
    }

    fn attach_prog(
        &self,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        attach_type: AttachType,
        target_fd: FdNumber,
        program: Arc<Program>,
    ) -> Result<SyscallResult, Errno> {
        let location: AttachLocation = attach_type.try_into()?;
        let program_type = attach_type.get_program_type();
        match (location, program_type) {
            (AttachLocation::Kernel, ProgramType::CgroupSockAddr) => {
                check_root_cgroup_fd(locked, current_task, target_fd)?;

                let linked_program =
                    SockAddrProgram(program.link(attach_type.get_program_type(), &[], &[])?);
                *self.root_cgroup.get_sock_addr_program(attach_type)?.write(locked) =
                    Some(linked_program);

                Ok(SUCCESS)
            }

            (AttachLocation::Kernel, ProgramType::CgroupSock) => {
                check_root_cgroup_fd(locked, current_task, target_fd)?;

                let helpers = ebpf_api::get_cgroup_sock_helpers();
                let linked_program =
                    SockProgram(program.link(attach_type.get_program_type(), &[], &helpers)?);
                *self.root_cgroup.get_sock_program(attach_type)?.write(locked) =
                    Some(linked_program);

                Ok(SUCCESS)
            }

            (AttachLocation::Kernel, _) => {
                unreachable!();
            }

            (AttachLocation::Netstack, _) => {
                check_root_cgroup_fd(locked, current_task, target_fd)?;
                self.attach_prog_in_netstack(attach_type, program)
            }

            (AttachLocation::Stub, _) => Ok(SUCCESS),
        }
    }

    fn detach_prog(
        &self,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        attach_type: AttachType,
        target_fd: FdNumber,
    ) -> Result<SyscallResult, Errno> {
        let location = attach_type.try_into()?;
        let program_type = attach_type.get_program_type();
        match (location, program_type) {
            (AttachLocation::Kernel, ProgramType::CgroupSockAddr) => {
                check_root_cgroup_fd(locked, current_task, target_fd)?;

                let mut prog_guard =
                    self.root_cgroup.get_sock_addr_program(attach_type)?.write(locked);
                if prog_guard.is_none() {
                    return error!(ENOENT);
                }

                *prog_guard = None;

                Ok(SUCCESS)
            }

            (AttachLocation::Kernel, ProgramType::CgroupSock) => {
                check_root_cgroup_fd(locked, current_task, target_fd)?;

                let mut prog_guard = self.root_cgroup.get_sock_program(attach_type)?.write(locked);
                if prog_guard.is_none() {
                    return error!(ENOENT);
                }

                *prog_guard = None;

                Ok(SUCCESS)
            }

            (AttachLocation::Kernel, _) => {
                unreachable!();
            }

            (AttachLocation::Netstack, _) => {
                check_root_cgroup_fd(locked, current_task, target_fd)?;
                self.detach_prog_in_netstack(attach_type)
            }

            (AttachLocation::Stub, _) => {
                error!(ENOTSUP)
            }
        }
    }

    fn attach_prog_in_netstack(
        &self,
        attach_type: AttachType,
        program: Arc<Program>,
    ) -> Result<SyscallResult, Errno> {
        let hook = attach_type_to_netstack_hook(attach_type).ok_or_else(|| errno!(ENOTSUP))?;
        let opts = fnet_filter::AttachEbpfProgramOptions {
            hook: Some(hook),
            program: Some((&*program).try_into()?),
            ..Default::default()
        };
        self.socket_control()
            .attach_ebpf_program(opts, zx::MonotonicInstant::INFINITE)
            .map_err(|e| {
                log_error!(
                    "failed to send fuchsia.net.filter/SocketControl.AttachEbpfProgram: {}",
                    e
                );
                errno!(EIO)
            })?
            .map_err(|e| {
                use fnet_filter::SocketControlAttachEbpfProgramError as Error;
                match e {
                    Error::NotSupported => errno!(ENOTSUP),
                    Error::LinkFailed => errno!(EINVAL),
                    Error::MapFailed => errno!(EIO),
                    Error::DuplicateAttachment => errno!(EEXIST),
                }
            })?;

        Ok(SUCCESS)
    }

    fn detach_prog_in_netstack(&self, attach_type: AttachType) -> Result<SyscallResult, Errno> {
        let hook = attach_type_to_netstack_hook(attach_type).ok_or_else(|| errno!(ENOTSUP))?;
        self.socket_control()
            .detach_ebpf_program(hook, zx::MonotonicInstant::INFINITE)
            .map_err(|e| {
                log_error!(
                    "failed to send fuchsia.net.filter/SocketControl.DetachEbpfProgram: {}",
                    e
                );
                errno!(EIO)
            })?
            .map_err(|e| {
                use fnet_filter::SocketControlDetachEbpfProgramError as Error;
                match e {
                    Error::NotFound => errno!(ENOENT),
                }
            })?;
        Ok(SUCCESS)
    }
}
