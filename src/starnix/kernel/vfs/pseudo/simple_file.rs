// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security;
use crate::task::{CurrentTask, Kernel};
use crate::vfs::buffers::{InputBuffer, OutputBuffer};
use crate::vfs::{
    fileops_impl_seekable, fs_node_impl_not_dir, AppendLockGuard, CheckAccessReason, FileObject,
    FileOps, FsNode, FsNodeInfo, FsNodeOps,
};

use crate::vfs::fileops_impl_noop_sync;
use starnix_sync::{FileOpsCore, Locked, RwLock};
use starnix_uapi::as_any::AsAny;
use starnix_uapi::auth::Capabilities;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::Access;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{errno, error};
use std::borrow::Cow;
use std::fmt::Display;
use std::sync::{Arc, Weak};

pub struct SimpleFileNode<F, O>
where
    F: Fn() -> Result<O, Errno>,
    O: FileOps,
{
    create_file_ops: F,

    /// Capabilities that should cause `check_access` to always pass.
    capabilities: Capabilities,
}

impl<F, O> SimpleFileNode<F, O>
where
    F: Fn() -> Result<O, Errno> + Send + Sync,
    O: FileOps,
{
    pub fn new(create_file_ops: F) -> SimpleFileNode<F, O> {
        SimpleFileNode { create_file_ops, capabilities: Capabilities::empty() }
    }

    pub fn new_with_capabilities(
        create_file_ops: F,
        capabilities: Capabilities,
    ) -> SimpleFileNode<F, O> {
        SimpleFileNode { create_file_ops, capabilities }
    }
}

impl<F, O> FsNodeOps for SimpleFileNode<F, O>
where
    F: Fn() -> Result<O, Errno> + Send + Sync + 'static,
    O: FileOps,
{
    fs_node_impl_not_dir!();

    fn check_access(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        node: &FsNode,
        current_task: &CurrentTask,
        access: Access,
        info: &RwLock<FsNodeInfo>,
        reason: CheckAccessReason,
    ) -> Result<(), Errno> {
        if self.capabilities != Capabilities::empty()
            && security::is_task_capable_noaudit(current_task, self.capabilities)
        {
            Ok(())
        } else {
            node.default_check_access_impl(current_task, access, reason, info.read())
        }
    }

    fn create_file_ops(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new((self.create_file_ops)()?))
    }

    fn truncate(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _guard: &AppendLockGuard<'_>,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _length: u64,
    ) -> Result<(), Errno> {
        // TODO(tbodt): Is this right? This is the minimum to handle O_TRUNC
        Ok(())
    }
}

pub fn parse_unsigned_file<T: Into<u64> + std::str::FromStr>(buf: &[u8]) -> Result<T, Errno> {
    let i = buf.iter().position(|c| !char::from(*c).is_ascii_digit()).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..i]).unwrap().parse::<T>().map_err(|_| errno!(EINVAL))
}

pub fn parse_i32_file(buf: &[u8]) -> Result<i32, Errno> {
    let i = buf
        .iter()
        .position(|c| {
            let ch = char::from(*c);
            !(ch.is_ascii_digit() || ch == '-')
        })
        .unwrap_or(buf.len());
    std::str::from_utf8(&buf[..i]).unwrap().parse::<i32>().map_err(|_| errno!(EINVAL))
}

pub fn serialize_for_file<T: Display>(value: T) -> Vec<u8> {
    let string = format!("{}\n", value);
    string.into_bytes()
}

pub struct BytesFile<Ops>(Arc<Ops>);

impl<Ops: BytesFileOps> BytesFile<Ops> {
    pub fn new(data: Ops) -> Self {
        Self(Arc::new(data))
    }

    pub fn new_node(data: Ops) -> impl FsNodeOps {
        let data = Arc::new(data);
        SimpleFileNode::new(move || Ok(BytesFile(Arc::clone(&data))))
    }
}

// Hand-written to avoid an unnecessary `Ops: Clone` bound which the derive would emit.
impl<Ops> std::clone::Clone for BytesFile<Ops> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<Ops: BytesFileOps> FileOps for BytesFile<Ops> {
    fileops_impl_seekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let content = self.0.read(current_task)?;
        if offset >= content.len() {
            return Ok(0);
        }
        data.write(&content[offset..])
    }

    fn write(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let data = data.read_all()?;
        let len = data.len();
        self.0.write(current_task, data)?;
        Ok(len)
    }
}

pub trait BytesFileOps: Send + Sync + AsAny + 'static {
    fn write(&self, _current_task: &CurrentTask, _data: Vec<u8>) -> Result<(), Errno> {
        error!(ENOSYS)
    }
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        error!(ENOSYS)
    }
}

impl BytesFileOps for Vec<u8> {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        Ok(self.into())
    }
}

impl<T> BytesFileOps for T
where
    T: Fn() -> Result<String, Errno> + Send + Sync + 'static,
{
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let data = self()?;
        Ok(data.into_bytes().into())
    }
}

pub fn create_bytes_file_with_handler<F>(kernel: Weak<Kernel>, kernel_handler: F) -> impl FsNodeOps
where
    F: Fn(Arc<Kernel>) -> String + Send + Sync + 'static,
{
    BytesFile::new_node(move || {
        if let Some(kernel) = kernel.upgrade() {
            Ok(kernel_handler(kernel) + "\n")
        } else {
            error!(ENOENT)
        }
    })
}
