// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A library to create "pseudo" file systems.  These file systems are backed by in process
//! callbacks.  Examples are: component configuration, debug information or statistics.

#![recursion_limit = "1024"]

pub mod test_utils;

#[macro_use]
pub mod common;

pub mod execution_scope;
pub use ::name;
pub mod path;

pub mod directory;
pub mod file;
pub mod node;
pub mod object_request;
mod protocols;
pub mod remote;
mod request_handler;
pub mod service;
pub mod symlink;
pub mod temp_clone;
pub mod token_registry;
pub mod tree_builder;

// --- pseudo_directory ---

// pseudo_directory! uses helper functions that live in this module.  It needs to be accessible
// from the outside of this crate.
#[doc(hidden)]
pub mod pseudo_directory;

/// Builds a pseudo directory using a simple DSL, potentially containing files and nested pseudo
/// directories.
///
/// A directory is described using a sequence of rules of the following form:
///
///   <name> `=>` <something that implements DirectoryEntry>
///
/// separated by commas, with an optional trailing comma.
///
/// It generates a nested pseudo directory, using [`directory::immutable::Simple::new()`] then
/// adding all the specified entries in it, by calling
/// [`crate::directory::helper::DirectlyMutable::add_entry`].
///
/// Note: Names specified as literals (both `str` and `[u8]`) are compared during compilation time,
/// so you should get a nice error message, if you specify the same entry name twice.  As entry
/// names can be specified as expressions, you can easily work around this check - you will still
/// get an error, but it would be a `panic!` in this case.  In any case the error message will
/// contain details of the location of the generating macro and the duplicate entry name.
///
/// # Examples
///
/// This will construct a small tree of read-only files:
/// ```
/// let root = pseudo_directory! {
///     "etc" => pseudo_directory! {
///         "fstab" => read_only(b"/dev/fs /"),
///         "passwd" => read_only(b"[redacted]"),
///         "shells" => read_only(b"/bin/bash"),
///         "ssh" => pseudo_directory! {
///           "sshd_config" => read_only(b"# Empty"),
///         },
///     },
///     "uname" => read_only(b"Fuchsia"),
/// };
/// ```
pub use vfs_macros::pseudo_directory;

pub use common::CreationMode;
pub use execution_scope::ExecutionScope;
pub use object_request::{ObjectRequest, ObjectRequestRef, ToObjectRequest};
pub use path::Path;
pub use protocols::ProtocolsExt;

// This allows the pseudo_directory! macro to use absolute paths within this crate to refer to the
// helper functions. External crates that use pseudo_directory! will rely on the pseudo_directory
// export above.
#[cfg(test)]
extern crate self as vfs;

use directory::entry_container::Directory;
use fidl_fuchsia_io as fio;
use std::sync::Arc;

/// Helper function to serve a new connection to the directory at `path` under `root` with `flags`.
/// Errors will be communicated via epitaph on the returned proxy. A new [`ExecutionScope`] will be
/// created for the request.
///
/// To serve `root` itself, use [`crate::directory::serve`] or set `path` to [`Path::dot`].
pub fn serve_directory<D: Directory + ?Sized>(
    root: Arc<D>,
    path: Path,
    flags: fio::Flags,
) -> fio::DirectoryProxy {
    let (proxy, server) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    let request = flags.to_object_request(server);
    request.handle(|request| root.open(ExecutionScope::new(), path, flags, request));
    proxy
}

/// Helper function to serve a new connection to the file at `path` under `root` with `flags`.
/// Errors will be communicated via epitaph on the returned proxy. A new [`ExecutionScope`] will be
/// created for the request.
///
/// To serve an object that implements [`crate::file::File`], use [`crate::file::serve`].
pub fn serve_file<D: Directory + ?Sized>(
    root: Arc<D>,
    path: Path,
    flags: fio::Flags,
) -> fio::FileProxy {
    let (proxy, server) = fidl::endpoints::create_proxy::<fio::FileMarker>();
    let request = flags.to_object_request(server);
    request.handle(|request| root.open(ExecutionScope::new(), path, flags, request));
    proxy
}
