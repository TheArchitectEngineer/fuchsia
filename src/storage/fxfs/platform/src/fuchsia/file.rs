// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fuchsia::directory::FxDirectory;
use crate::fuchsia::errors::map_to_status;
use crate::fuchsia::node::{FxNode, OpenedNode};
use crate::fuchsia::paged_object_handle::PagedObjectHandle;
use crate::fuchsia::pager::{
    default_page_in, MarkDirtyRange, PageInRange, PagerBacked, PagerPacketReceiverRegistration,
};
use crate::fuchsia::volume::{info_to_filesystem_info, FxVolume};
use anyhow::Error;
use fidl_fuchsia_io as fio;
use fxfs::filesystem::{SyncOptions, MAX_FILE_SIZE};
use fxfs::future_with_guard::FutureWithGuard;
use fxfs::log::*;
use fxfs::object_handle::{ObjectHandle, ReadObjectHandle};
use fxfs::object_store::data_object_handle::OverwriteOptions;
use fxfs::object_store::object_record::EncryptionKey;
use fxfs::object_store::transaction::{lock_keys, LockKey, Options};
use fxfs::object_store::{DataObjectHandle, ObjectDescriptor, FSCRYPT_KEY_ID};
use fxfs_macros::ToWeakNode;
use std::fmt::{Debug, Formatter};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use storage_device::buffer;
use vfs::directory::entry::{EntryInfo, GetEntryInfo};
use vfs::directory::entry_container::MutableDirectory;
use vfs::execution_scope::ExecutionScope;
use vfs::file::{File, FileOptions, GetVmo, StreamIoConnection, SyncMode};
use vfs::name::Name;
use vfs::{attributes, ObjectRequestRef, ProtocolsExt};
use zx::{self as zx, HandleBased, Status};

/// In many operating systems, it is possible to delete a file with open handles. In this case the
/// file will continue to use space on disk but will not openable and the storage it uses will be
/// freed when the last handle to the file is closed.
/// To provide this behaviour, we use this constant to denote files that are marked for deletion.
///
/// When the top bit of the open count is set, it means the file has been deleted and when the count
/// drops to zero, it will be tombstoned.  Once it has dropped to zero, it cannot be opened again
/// (assertions will fire).
const TO_BE_PURGED: u64 = 1 << (u64::BITS - 1);

/// This is the second most significant bit of `open_count`. It set, it indicates that the file is
/// an unnamed temporary file (i.e. it lives in the graveyard *temporarily* and can be moved out if
/// it was linked into the filesystem permanently). An unnamed temporary file can be linked into a
/// directory, which gives it a name and makes it permanent. Internally, linking a regular file and
/// an unnamed temporary file is handled slightly differently because the latter resides in the
/// graveyard. We need to be able to identify if a file is an unnamed temporary file whenever there
/// is an attempt to link it into a directory. Once it has been linked into the filesystem, it is no
/// longer temporary (it does not reside in the graveyard anymore) and this bit will be set to 0.
const IS_TEMPORARILY_IN_GRAVEYARD: u64 = 1 << (u64::BITS - 2);

/// The file is dirty and needs to be flushed.  When this bit is set, we hold a strong count to
/// ensure the file cannot be dropped.
const IS_DIRTY: u64 = 1 << (u64::BITS - 3);

/// An unnamed temporary file lives in the graveyard and has to marked to be purged to make sure
/// that the storage this file uses will be freed when the last handle to it closes.
const IS_UNNAMED_TEMPORARY: u64 = IS_TEMPORARILY_IN_GRAVEYARD | TO_BE_PURGED;

/// The maximum value of open counts. The two most significant bits are used to indicate other
/// information regarding the state of the file. See the consts defined above.
const MAX_OPEN_COUNTS: u64 = IS_DIRTY - 1;

struct State(u64);

impl Debug for State {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("State")
            .field("open_count", &self.open_count())
            .field("to_be_purged", &self.to_be_purged())
            .field("is_temporarily_in_graveyard", &self.is_temporarily_in_graveyard())
            .field("is_dirty", &self.is_dirty())
            .finish()
    }
}

impl State {
    fn open_count(&self) -> u64 {
        self.0 & MAX_OPEN_COUNTS
    }

    fn to_be_purged(&self) -> bool {
        self.0 & TO_BE_PURGED != 0
    }

    fn is_temporarily_in_graveyard(&self) -> bool {
        self.0 & IS_TEMPORARILY_IN_GRAVEYARD != 0
    }

    fn is_unnamed_temporary(&self) -> bool {
        self.0 & IS_UNNAMED_TEMPORARY == IS_UNNAMED_TEMPORARY
    }

    fn will_be_tombstoned(&self) -> bool {
        self.to_be_purged() && self.open_count() == 0
    }

    fn is_dirty(&self) -> bool {
        self.0 & IS_DIRTY != 0
    }
}

/// FxFile represents an open connection to a file.
#[derive(ToWeakNode)]
pub struct FxFile {
    handle: PagedObjectHandle,
    state: AtomicU64,
    pager_packet_receiver_registration: PagerPacketReceiverRegistration<Self>,
}

#[fxfs_trace::trace]
impl FxFile {
    /// Creates a new regular FxFile.
    pub fn new(handle: DataObjectHandle<FxVolume>) -> Arc<Self> {
        let size = handle.get_size();
        Arc::new_cyclic(|weak| {
            let (vmo, pager_packet_receiver_registration) = handle
                .owner()
                .pager()
                .create_vmo(
                    weak.clone(),
                    size,
                    zx::VmoOptions::UNBOUNDED | zx::VmoOptions::TRAP_DIRTY,
                )
                .unwrap();
            Self {
                handle: PagedObjectHandle::new(handle, vmo),
                state: AtomicU64::new(0),
                pager_packet_receiver_registration,
            }
        })
    }

    /// Creates a new connection on the given `scope`. May take a read lock on the object.
    pub async fn create_connection_async(
        this: OpenedNode<FxFile>,
        scope: ExecutionScope,
        flags: impl ProtocolsExt,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        if let Some(rights) = flags.rights() {
            if rights.intersects(fio::Operations::READ_BYTES | fio::Operations::WRITE_BYTES) {
                if let Some(fut) = this.handle.pre_fetch_keys() {
                    // Keep the object from being deleted until after the fetch is complete.
                    let fs = this.handle.owner().store().filesystem();
                    let read_lock = fs
                        .clone()
                        .lock_manager()
                        .read_lock(lock_keys!(LockKey::object(
                            this.handle.owner().store().store_object_id(),
                            this.object_id()
                        )))
                        .await
                        .into_owned(fs);
                    this.handle.owner().scope().spawn(FutureWithGuard::new(read_lock, fut));
                }
            }
        }
        object_request
            .create_connection::<StreamIoConnection<_>, _>(scope, this.take(), flags)
            .await
    }

    /// Open the file as a temporary.  The file must have just been created with no other open
    /// counts.
    pub fn open_as_temporary(self: Arc<Self>) -> OpenedNode<dyn FxNode> {
        assert_eq!(self.state.swap(1 | IS_UNNAMED_TEMPORARY, Ordering::Relaxed), 0);
        OpenedNode(self)
    }

    /// Mark the state as permanent (to be used when the file is currently marked as temporary).
    pub fn mark_as_permanent(&self) {
        assert!(State(self.state.fetch_and(!IS_UNNAMED_TEMPORARY, Ordering::Relaxed))
            .is_unnamed_temporary());
    }

    pub fn is_verified_file(&self) -> bool {
        self.handle.uncached_handle().is_verified_file()
    }

    pub fn handle(&self) -> &PagedObjectHandle {
        &self.handle
    }

    /// If this instance has not been marked to be purged, returns an OpenedNode instance.
    /// If marked for purging, returns None.
    pub fn into_opened_node(self: Arc<Self>) -> Option<OpenedNode<FxFile>> {
        self.increment_open_count().then(|| OpenedNode(self))
    }

    /// Persists any unflushed data to disk.
    ///
    /// Flush may be triggered as a background task so this requires an OpenedNode to
    /// ensure that we don't accidentally try to flush a file handle that is in the process of
    /// being removed. (See use of cache in `FxVolume::flush_all_files`.)
    #[trace]
    pub async fn flush(this: &OpenedNode<FxFile>, last_chance: bool) -> Result<(), Error> {
        this.handle.flush(last_chance).await.map(|_| ())
    }

    pub fn get_block_size(&self) -> u64 {
        self.handle.block_size()
    }

    pub async fn is_allocated(&self, start_offset: u64) -> Result<(bool, u64), Status> {
        self.handle.uncached_handle().is_allocated(start_offset).await.map_err(map_to_status)
    }

    // TODO(https://fxbug.dev/42171261): might be better to have a cached/uncached mode for file and call
    // this when in uncached mode
    pub async fn write_at_uncached(&self, offset: u64, content: &[u8]) -> Result<u64, Status> {
        let mut buf = self.handle.uncached_handle().allocate_buffer(content.len()).await;
        buf.as_mut_slice().copy_from_slice(content);
        let _ = self
            .handle
            .uncached_handle()
            .overwrite(
                offset,
                buf.as_mut(),
                OverwriteOptions { allow_allocations: true, ..Default::default() },
            )
            .await
            .map_err(map_to_status)?;
        Ok(content.len() as u64)
    }

    // TODO(https://fxbug.dev/42171261): might be better to have a cached/uncached mode for file and call
    // this when in uncached mode
    pub async fn read_at_uncached(&self, offset: u64, buffer: &mut [u8]) -> Result<u64, Status> {
        let mut buf = self.handle.uncached_handle().allocate_buffer(buffer.len()).await;
        buf.as_mut_slice().fill(0);
        let bytes_read = self
            .handle
            .uncached_handle()
            .read(offset, buf.as_mut())
            .await
            .map_err(map_to_status)?;
        buffer.copy_from_slice(buf.as_slice());
        Ok(bytes_read as u64)
    }

    pub async fn get_size_uncached(&self) -> u64 {
        self.handle.uncached_handle().get_size()
    }

    async fn fscrypt_wrapping_key_id(&self) -> Result<Option<[u8; 16]>, zx::Status> {
        if self.handle.store().is_encrypted() {
            if let Some(key) = self
                .handle
                .store()
                .get_keys(self.object_id())
                .await
                .map_err(map_to_status)?
                .get(FSCRYPT_KEY_ID)
            {
                if let EncryptionKey::Fxfs(fxfs_key) = key {
                    return Ok(Some(fxfs_key.wrapping_key_id.to_le_bytes()));
                } else {
                    error!("Unexpected key type: {:?}", key);
                    return Ok(None);
                }
            }
        }
        Ok(None)
    }

    /// Forcibly marks the file as clean.
    pub fn force_clean(&self) {
        let old = State(self.state.fetch_and(!IS_DIRTY, Ordering::Relaxed));
        if old.is_dirty() {
            if self.handle.needs_flush() {
                warn!("File {} was forcibly marked clean; data may be lost", self.object_id(),);
            }
            // SAFETY: The IS_DIRTY bit means we took a reference.
            unsafe {
                let _ = Arc::from_raw(self);
            }
        }
    }

    // Increments the open count by 1. Returns true if successful.
    #[must_use]
    fn increment_open_count(&self) -> bool {
        let mut old = self.load_state();
        loop {
            if old.will_be_tombstoned() {
                return false;
            }

            assert!(old.open_count() < MAX_OPEN_COUNTS);

            match self.state.compare_exchange_weak(
                old.0,
                old.0 + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(new_value) => old.0 = new_value,
            }
        }
    }

    fn load_state(&self) -> State {
        State(self.state.load(Ordering::Relaxed))
    }
}

impl Drop for FxFile {
    fn drop(&mut self) {
        let volume = self.handle.owner();
        volume.cache().remove(self);
    }
}

impl FxNode for FxFile {
    fn object_id(&self) -> u64 {
        self.handle.object_id()
    }

    fn parent(&self) -> Option<Arc<FxDirectory>> {
        unreachable!(); // Add a parent back-reference if needed.
    }

    fn set_parent(&self, _parent: Arc<FxDirectory>) {
        // NOP
    }

    fn open_count_add_one(&self) {
        assert!(self.increment_open_count());
    }

    fn open_count_sub_one(self: Arc<Self>) {
        let mut current = self.load_state();
        loop {
            // If it's the last open count, we need to handle flushing dirty data, and purging if it
            // is so marked.
            if current.open_count() == 1 {
                if current.to_be_purged() {
                    match self.state.compare_exchange_weak(
                        current.0,
                        (current.0 & !IS_DIRTY) - 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            if current.is_dirty() {
                                // SAFETY: The IS_DIRTY bit means we took a reference.
                                unsafe {
                                    let _ = Arc::from_raw(Arc::as_ptr(&self));
                                }
                            }
                            // This node is marked `TO_BE_PURGED` and there are no more references
                            // to it. This file will be tombstoned. Actual purging is queued to be
                            // done asynchronously. We don't need to do any flushing in this case -
                            // if the file is going to be deleted anyway, there is no point.
                            self.handle.owner().clone().spawn(async move {
                                let store = self.handle.store();
                                store.filesystem().graveyard().queue_tombstone_object(
                                    store.store_object_id(),
                                    self.object_id(),
                                );
                            });
                            return;
                        }
                        Err(old) => {
                            current.0 = old;
                            continue;
                        }
                    }
                }
                // If the file is dirty, we need to hold a strong reference to make sure the file
                // doesn't go away until it has been flushed.
                if self.handle.needs_flush() {
                    if !current.is_dirty() {
                        match self.state.compare_exchange_weak(
                            current.0,
                            (current.0 | IS_DIRTY) - 1,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => {
                                // We need to hold a strong reference to prevent the node from being
                                // dropped.
                                let _ = Arc::into_raw(self);
                                return;
                            }
                            Err(old) => {
                                current.0 = old;
                                continue;
                            }
                        }
                    }
                } else if current.is_dirty() {
                    // File is no longer dirty.
                    match self.state.compare_exchange_weak(
                        current.0,
                        (current.0 & !IS_DIRTY) - 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => {
                            // SAFETY: The IS_DIRTY bit means we took a reference.
                            unsafe {
                                let _ = Arc::from_raw(Arc::as_ptr(&self));
                            }
                            return;
                        }
                        Err(old) => {
                            current.0 = old;
                            continue;
                        }
                    }
                }
            }
            match self.state.compare_exchange_weak(
                current.0,
                current.0 - 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(old) => current.0 = old,
            }
        }
    }

    fn object_descriptor(&self) -> ObjectDescriptor {
        ObjectDescriptor::File
    }

    fn terminate(&self) {
        self.pager_packet_receiver_registration.stop_watching_for_zero_children();
    }

    // Mark the state as to be purged. Returns true if there are no open references.
    fn mark_to_be_purged(&self) -> bool {
        let old = State(self.state.fetch_or(TO_BE_PURGED, Ordering::Relaxed));
        assert!(!old.to_be_purged());
        old.open_count() == 0
    }
}

impl GetEntryInfo for FxFile {
    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(self.object_id(), fio::DirentType::File)
    }
}

impl vfs::node::Node for FxFile {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, zx::Status> {
        let mut props = self.handle.get_properties().await.map_err(map_to_status)?;
        let descriptor =
            self.handle.uncached_handle().get_descriptor().await.map_err(map_to_status)?;
        // In most cases, the reference count of objects can be used as the link count. There are
        // two cases where this is not the case - for unnamed temporary files and unlink files with
        // no more open references to it. For these two cases, the link count should be zero (the
        // object reference count is one as they live in the graveyard). In both cases,
        // `TO_BE_PURGED` will be set and `refs` is one.
        let to_be_purged = self.load_state().to_be_purged();
        let link_count = if to_be_purged && props.refs == 1 { 0 } else { props.refs };

        if requested_attributes.contains(fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE) {
            self.handle
                .store()
                .update_access_time(self.handle.object_id(), &mut props)
                .await
                .map_err(map_to_status)?;
        }

        Ok(attributes!(
            requested_attributes,
            Mutable {
                creation_time: props.creation_time.as_nanos(),
                modification_time: props.modification_time.as_nanos(),
                access_time: props.access_time.as_nanos(),
                mode: props.posix_attributes.map(|a| a.mode),
                uid: props.posix_attributes.map(|a| a.uid),
                gid: props.posix_attributes.map(|a| a.gid),
                rdev: props.posix_attributes.map(|a| a.rdev),
                selinux_context: self
                    .handle
                    .uncached_handle()
                    .get_inline_selinux_context()
                    .await
                    .map_err(map_to_status)?,
                wrapping_key_id: self.fscrypt_wrapping_key_id().await?,
            },
            Immutable {
                protocols: fio::NodeProtocolKinds::FILE,
                abilities: fio::Operations::GET_ATTRIBUTES
                    | fio::Operations::UPDATE_ATTRIBUTES
                    | fio::Operations::READ_BYTES
                    | fio::Operations::WRITE_BYTES,
                content_size: props.data_attribute_size,
                storage_size: props.allocated_size,
                link_count: link_count,
                id: self.handle.object_id(),
                change_time: props.change_time.as_nanos(),
                options: descriptor.clone().map(|a| a.0),
                root_hash: descriptor.clone().map(|a| a.1),
                verity_enabled: self.is_verified_file(),
            }
        ))
    }

    fn will_clone(&self) {
        self.open_count_add_one();
    }

    fn close(self: Arc<Self>) {
        self.open_count_sub_one();
    }

    async fn link_into(
        self: Arc<Self>,
        destination_dir: Arc<dyn MutableDirectory>,
        name: Name,
    ) -> Result<(), zx::Status> {
        let dir = destination_dir.into_any().downcast::<FxDirectory>().unwrap();
        let store = self.handle.store();
        let object_id = self.object_id();
        let transaction = store
            .filesystem()
            .clone()
            .new_transaction(
                lock_keys![
                    LockKey::object(store.store_object_id(), object_id),
                    LockKey::object(store.store_object_id(), dir.object_id()),
                ],
                Options::default(),
            )
            .await
            .map_err(map_to_status)?;

        dir.check_fscrypt_hard_link_conditions(
            self.fscrypt_wrapping_key_id().await?.map(|x| u128::from_le_bytes(x)),
        )?;

        let state = self.load_state();
        let is_unnamed_temporary = state.is_unnamed_temporary();
        let to_be_purged = state.to_be_purged();
        if is_unnamed_temporary {
            // Remove object from graveyard and link it to `name`.
            dir.link_graveyard_object(transaction, &name, object_id, ObjectDescriptor::File, || {
                self.mark_as_permanent()
            })
            .await
        } else {
            // Check that we're not unlinked.
            if to_be_purged {
                return Err(zx::Status::NOT_FOUND);
            }
            dir.link_object(transaction, &name, object_id, ObjectDescriptor::File).await
        }
    }

    fn query_filesystem(&self) -> Result<fio::FilesystemInfo, Status> {
        let store = self.handle.store();
        Ok(info_to_filesystem_info(
            store.filesystem().get_info(),
            store.filesystem().block_size(),
            store.object_count(),
            self.handle.owner().id(),
        ))
    }
}

impl File for FxFile {
    fn writable(&self) -> bool {
        true
    }

    async fn open_file(&self, _options: &FileOptions) -> Result<(), Status> {
        Ok(())
    }

    async fn truncate(&self, length: u64) -> Result<(), Status> {
        self.handle.truncate(length).await.map_err(map_to_status)?;
        Ok(())
    }

    async fn enable_verity(&self, options: fio::VerificationOptions) -> Result<(), Status> {
        self.handle.set_read_only();
        self.handle.flush(false).await.map_err(map_to_status)?;
        self.handle.uncached_handle().enable_verity(options).await.map_err(map_to_status)
    }

    // Returns a VMO handle that supports paging.
    async fn get_backing_memory(&self, flags: fio::VmoFlags) -> Result<zx::Vmo, Status> {
        // We do not support executable VMO handles.
        if flags.contains(fio::VmoFlags::EXECUTE) {
            error!("get_backing_memory does not support execute rights!");
            return Err(Status::NOT_SUPPORTED);
        }

        let vmo = self.handle.vmo();
        let mut rights = zx::Rights::BASIC | zx::Rights::MAP | zx::Rights::GET_PROPERTY;
        if flags.contains(fio::VmoFlags::READ) {
            rights |= zx::Rights::READ;
        }
        if flags.contains(fio::VmoFlags::WRITE) {
            rights |= zx::Rights::WRITE;
        }

        let child_vmo = if flags.contains(fio::VmoFlags::PRIVATE_CLONE) {
            // Allow for the VMO's content size and name to be changed even without ZX_RIGHT_WRITE.
            rights |= zx::Rights::SET_PROPERTY;
            let mut child_options = zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE;
            if flags.contains(fio::VmoFlags::WRITE) {
                child_options |= zx::VmoChildOptions::RESIZABLE;
                rights |= zx::Rights::RESIZE;
            }
            vmo.create_child(child_options, 0, vmo.get_stream_size()?)?
        } else {
            vmo.create_child(zx::VmoChildOptions::REFERENCE, 0, 0)?
        };

        let child_vmo = child_vmo.replace_handle(rights)?;
        if self.handle.owner().pager().watch_for_zero_children(self).map_err(map_to_status)? {
            // Take an open count so that we keep this object alive if it is unlinked.
            self.open_count_add_one();
        }
        Ok(child_vmo)
    }

    async fn get_size(&self) -> Result<u64, Status> {
        Ok(self.handle.get_size())
    }

    async fn update_attributes(
        &self,
        attributes: fio::MutableNodeAttributes,
    ) -> Result<(), Status> {
        if attributes == fio::MutableNodeAttributes::default() {
            return Ok(());
        }

        self.handle.update_attributes(&attributes).await.map_err(map_to_status)?;
        Ok(())
    }

    async fn list_extended_attributes(&self) -> Result<Vec<Vec<u8>>, Status> {
        self.handle.store_handle().list_extended_attributes().await.map_err(map_to_status)
    }

    async fn get_extended_attribute(&self, name: Vec<u8>) -> Result<Vec<u8>, Status> {
        self.handle.store_handle().get_extended_attribute(name).await.map_err(map_to_status)
    }

    async fn set_extended_attribute(
        &self,
        name: Vec<u8>,
        value: Vec<u8>,
        mode: fio::SetExtendedAttributeMode,
    ) -> Result<(), Status> {
        self.handle
            .store_handle()
            .set_extended_attribute(name, value, mode.into())
            .await
            .map_err(map_to_status)
    }

    async fn remove_extended_attribute(&self, name: Vec<u8>) -> Result<(), Status> {
        self.handle.store_handle().remove_extended_attribute(name).await.map_err(map_to_status)
    }

    async fn allocate(
        &self,
        offset: u64,
        length: u64,
        _mode: fio::AllocateMode,
    ) -> Result<(), Status> {
        // NB: FILE_BIG is used so the error converts to EFBIG when passed through starnix, which
        // is the required error code when the requested range is larger than the file size.
        let range = offset..offset.checked_add(length).ok_or(Status::FILE_BIG)?;
        self.handle.allocate(range).await.map_err(map_to_status)
    }

    async fn sync(&self, mode: SyncMode) -> Result<(), Status> {
        self.handle.flush(false).await.map_err(map_to_status)?;

        // TODO(https://fxbug.dev/42178163): at the moment, this doesn't send a flush to the device, which
        // doesn't match minfs.
        if mode == SyncMode::Normal {
            self.handle
                .store()
                .filesystem()
                .sync(SyncOptions::default())
                .await
                .map_err(map_to_status)?;
        }

        Ok(())
    }
}

#[fxfs_trace::trace]
impl PagerBacked for FxFile {
    fn pager(&self) -> &crate::pager::Pager {
        self.handle.owner().pager()
    }

    fn pager_packet_receiver_registration(&self) -> &PagerPacketReceiverRegistration<Self> {
        &self.pager_packet_receiver_registration
    }

    fn vmo(&self) -> &zx::Vmo {
        self.handle.vmo()
    }

    fn page_in(self: Arc<Self>, range: PageInRange<Self>) {
        let read_ahead_size = self.handle.owner().read_ahead_size();
        default_page_in(self, range, read_ahead_size);
    }

    #[trace]
    fn mark_dirty(self: Arc<Self>, range: MarkDirtyRange<Self>) {
        let (valid_pages, invalid_pages) = range.split(MAX_FILE_SIZE);
        if let Some(invalid_pages) = invalid_pages {
            invalid_pages.report_failure(zx::Status::FILE_BIG);
        }
        let range = match valid_pages {
            Some(range) => range,
            None => return,
        };

        let byte_count = range.len();
        self.handle.owner().clone().report_pager_dirty(byte_count, move || {
            if let Err(_) = self.handle.mark_dirty(range) {
                // Undo the report of the dirty pages since mark_dirty failed.
                self.handle.owner().report_pager_clean(byte_count);
            }
        });
    }

    fn on_zero_children(self: Arc<Self>) {
        // Drop the open count that we took in `get_backing_memory`.
        self.open_count_sub_one();
    }

    fn byte_size(&self) -> u64 {
        self.handle.uncached_size()
    }

    #[trace("len" => (range.end - range.start))]
    async fn aligned_read(&self, range: Range<u64>) -> Result<buffer::Buffer<'_>, Error> {
        let buffer = self.handle.read_uncached(range).await?;
        Ok(buffer)
    }
}

impl GetVmo for FxFile {
    const PAGER_ON_FIDL_EXECUTOR: bool = true;

    fn get_vmo(&self) -> &zx::Vmo {
        self.vmo()
    }
}

#[cfg(test)]
mod tests {
    use crate::fuchsia::testing::{
        close_file_checked, open_dir_checked, open_file, open_file_checked, TestFixture,
        TestFixtureOptions,
    };
    use anyhow::format_err;
    use fsverity_merkle::{FsVerityHasher, FsVerityHasherOptions};
    use fuchsia_fs::file;
    use futures::join;
    use fxfs::fsck::fsck;
    use fxfs::object_handle::INVALID_OBJECT_ID;
    use fxfs::object_store::Timestamp;
    use rand::{thread_rng, Rng};
    use std::sync::atomic::{self, AtomicBool};
    use std::sync::Arc;
    use storage_device::fake_device::FakeDevice;
    use storage_device::DeviceHolder;
    use zx::Status;
    use {fidl_fuchsia_io as fio, fuchsia_async as fasync};

    #[fuchsia::test(threads = 10)]
    async fn test_empty_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let buf = file
            .read(fio::MAX_BUF)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("read failed");
        assert!(buf.is_empty());

        let (mutable_attrs, immutable_attrs) = file
            .get_attributes(fio::NodeAttributesQuery::all())
            .await
            .expect("FIDL call failed")
            .expect("GetAttributes failed");
        assert_ne!(immutable_attrs.id.unwrap(), INVALID_OBJECT_ID);
        assert_eq!(immutable_attrs.content_size.unwrap(), 0u64);
        assert_eq!(immutable_attrs.storage_size.unwrap(), 0u64);
        assert_eq!(immutable_attrs.link_count.unwrap(), 1u64);
        assert_ne!(mutable_attrs.creation_time.unwrap(), 0u64);
        assert_ne!(mutable_attrs.modification_time.unwrap(), 0u64);
        assert_eq!(mutable_attrs.creation_time.unwrap(), mutable_attrs.modification_time.unwrap());

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_write_read() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let inputs = vec!["hello, ", "world!"];
        let expected_output = "hello, world!";
        for input in inputs {
            let bytes_written = file
                .write(input.as_bytes())
                .await
                .expect("write failed")
                .map_err(Status::from_raw)
                .expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());
        }

        let buf = file
            .read_at(fio::MAX_BUF, 0)
            .await
            .expect("read_at failed")
            .map_err(Status::from_raw)
            .expect("File read was successful");
        assert_eq!(buf.len(), expected_output.as_bytes().len());
        assert!(buf.iter().eq(expected_output.as_bytes().iter()));

        let (_, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
            )
            .await
            .expect("FIDL call failed")
            .expect("get_attributes failed");

        assert_eq!(
            immutable_attributes.content_size.unwrap(),
            expected_output.as_bytes().len() as u64
        );
        assert_eq!(immutable_attributes.storage_size.unwrap(), fixture.fs().block_size() as u64);

        let () = file
            .sync()
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("sync failed");

        let (_, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
            )
            .await
            .expect("FIDL call failed")
            .expect("get_attributes failed");

        assert_eq!(
            immutable_attributes.content_size.unwrap(),
            expected_output.as_bytes().len() as u64
        );
        assert_eq!(immutable_attributes.storage_size.unwrap(), fixture.fs().block_size() as u64);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_page_in() {
        let input = "hello, world!";
        let reused_device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            let bytes_written = file
                .write(input.as_bytes())
                .await
                .expect("write failed")
                .map_err(Status::from_raw)
                .expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());
            assert!(file.sync().await.expect("Sync failed").is_ok());

            close_file_checked(file).await;
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let vmo =
            file.get_backing_memory(fio::VmoFlags::READ).await.expect("Fidl failure").unwrap();
        let mut readback = vec![0; input.as_bytes().len()];
        assert!(vmo.read(&mut readback, 0).is_ok());
        assert_eq!(input.as_bytes(), readback);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_page_in_io_error() {
        let mut device = FakeDevice::new(8192, 512);
        let succeed_requests = Arc::new(AtomicBool::new(true));
        let succeed_requests_clone = succeed_requests.clone();
        device.set_op_callback(Box::new(move |_| {
            if succeed_requests_clone.load(atomic::Ordering::Relaxed) {
                Ok(())
            } else {
                Err(format_err!("Fake error."))
            }
        }));

        let input = "hello, world!";
        let reused_device = {
            let fixture = TestFixture::open(
                DeviceHolder::new(device),
                TestFixtureOptions { format: true, ..Default::default() },
            )
            .await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            let bytes_written = file
                .write(input.as_bytes())
                .await
                .expect("write failed")
                .map_err(Status::from_raw)
                .expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());

            close_file_checked(file).await;
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let vmo =
            file.get_backing_memory(fio::VmoFlags::READ).await.expect("Fidl failure").unwrap();
        succeed_requests.store(false, atomic::Ordering::Relaxed);
        let mut readback = vec![0; input.as_bytes().len()];
        assert!(vmo.read(&mut readback, 0).is_err());

        succeed_requests.store(true, atomic::Ordering::Relaxed);
        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_writes_persist() {
        let mut device = DeviceHolder::new(FakeDevice::new(8192, 512));
        for i in 0..2 {
            let fixture = TestFixture::open(
                device,
                TestFixtureOptions { format: i == 0, ..Default::default() },
            )
            .await;
            let root = fixture.root();

            let flags = if i == 0 {
                fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE
            } else {
                fio::PERM_READABLE | fio::PERM_WRITABLE
            };
            let file = open_file_checked(
                &root,
                "foo",
                flags | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            if i == 0 {
                let _: u64 = file
                    .write(&vec![0xaa as u8; 8192])
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::from_raw)
                    .expect("File write was successful");
            } else {
                let buf = file
                    .read(8192)
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::from_raw)
                    .expect("File read was successful");
                assert_eq!(buf, vec![0xaa as u8; 8192]);
            }

            let (_, immutable_attributes) = file
                .get_attributes(
                    fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
                )
                .await
                .expect("FIDL call failed")
                .expect("get_attributes failed");

            assert_eq!(immutable_attributes.content_size.unwrap(), 8192u64);
            assert_eq!(immutable_attributes.storage_size.unwrap(), 8192u64);

            close_file_checked(file).await;
            device = fixture.close().await;
        }
    }

    #[fuchsia::test(threads = 10)]
    async fn test_append() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let inputs = vec!["hello, ", "world!"];
        let expected_output = "hello, world!";
        for input in inputs {
            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::FILE_APPEND
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            let bytes_written = file
                .write(input.as_bytes())
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());
            close_file_checked(file).await;
        }

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        let buf = file
            .read_at(fio::MAX_BUF, 0)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("File read was successful");
        assert_eq!(buf.len(), expected_output.as_bytes().len());
        assert_eq!(&buf[..], expected_output.as_bytes());

        let (_, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
            )
            .await
            .expect("FIDL call failed")
            .expect("get_attributes failed");

        assert_eq!(
            immutable_attributes.content_size.unwrap(),
            expected_output.as_bytes().len() as u64
        );
        assert_eq!(immutable_attributes.storage_size.unwrap(), fixture.fs().block_size() as u64);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_seek() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let input = "hello, world!";
        let _: u64 = file
            .write(input.as_bytes())
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("File write was successful");

        {
            let offset = file
                .seek(fio::SeekOrigin::Start, 0)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("seek was successful");
            assert_eq!(offset, 0);
            let buf = file
                .read(5)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("File read was successful");
            assert!(buf.iter().eq("hello".as_bytes().into_iter()));
        }
        {
            let offset = file
                .seek(fio::SeekOrigin::Current, 2)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("seek was successful");
            assert_eq!(offset, 7);
            let buf = file
                .read(5)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("File read was successful");
            assert!(buf.iter().eq("world".as_bytes().into_iter()));
        }
        {
            let offset = file
                .seek(fio::SeekOrigin::Current, -5)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("seek was successful");
            assert_eq!(offset, 7);
            let buf = file
                .read(5)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("File read was successful");
            assert!(buf.iter().eq("world".as_bytes().into_iter()));
        }
        {
            let offset = file
                .seek(fio::SeekOrigin::End, -1)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("seek was successful");
            assert_eq!(offset, 12);
            let buf = file
                .read(1)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("File read was successful");
            assert!(buf.iter().eq("!".as_bytes().into_iter()));
        }

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_resize_extend() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let input = "hello, world!";
        let len: usize = 16 * 1024;

        let _: u64 = file
            .write(input.as_bytes())
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("File write was successful");

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let () = file
            .resize(len as u64)
            .await
            .expect("resize failed")
            .map_err(Status::from_raw)
            .expect("resize error");

        let mut expected_buf = vec![0 as u8; len];
        expected_buf[..input.as_bytes().len()].copy_from_slice(input.as_bytes());

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        // Write something at the end of the gap.
        expected_buf[len - 1..].copy_from_slice("a".as_bytes());

        let _: u64 = file
            .write_at("a".as_bytes(), (len - 1) as u64)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("File write was successful");

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_resize_shrink() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let len: usize = 2 * 1024;
        let input = {
            let mut v = vec![0 as u8; len];
            for i in 0..v.len() {
                v[i] = ('a' as u8) + (i % 13) as u8;
            }
            v
        };
        let short_len: usize = 513;

        file::write(&file, &input).await.expect("File write was successful");

        let () = file
            .resize(short_len as u64)
            .await
            .expect("resize failed")
            .map_err(Status::from_raw)
            .expect("resize error");

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), short_len);
        assert_eq!(buf, input[..short_len]);

        // Resize to the original length and verify the data's zeroed.
        let () = file
            .resize(len as u64)
            .await
            .expect("resize failed")
            .map_err(Status::from_raw)
            .expect("resize error");

        let expected_buf = {
            let mut v = vec![0 as u8; len];
            v[..short_len].copy_from_slice(&input[..short_len]);
            v
        };

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek failed")
            .map_err(Status::from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_resize_shrink_repeated() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let orig_len: usize = 4 * 1024;
        let mut len = orig_len;
        let input = {
            let mut v = vec![0 as u8; len];
            for i in 0..v.len() {
                v[i] = ('a' as u8) + (i % 13) as u8;
            }
            v
        };
        let short_len: usize = 513;

        file::write(&file, &input).await.expect("File write was successful");

        while len > short_len {
            len -= std::cmp::min(len - short_len, 512);
            let () = file
                .resize(len as u64)
                .await
                .expect("resize failed")
                .map_err(Status::from_raw)
                .expect("resize error");
        }

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("Seek failed")
            .map_err(Status::from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), short_len);
        assert_eq!(buf, input[..short_len]);

        // Resize to the original length and verify the data's zeroed.
        let () = file
            .resize(orig_len as u64)
            .await
            .expect("resize failed")
            .map_err(Status::from_raw)
            .expect("resize error");

        let expected_buf = {
            let mut v = vec![0 as u8; orig_len];
            v[..short_len].copy_from_slice(&input[..short_len]);
            v
        };

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek failed")
            .map_err(Status::from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), orig_len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_unlink_with_open_race() {
        let fixture = Arc::new(TestFixture::new().await);
        let fixture1 = fixture.clone();
        let fixture2 = fixture.clone();
        let fixture3 = fixture.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done1 = done.clone();
        let done2 = done.clone();
        join!(
            fasync::Task::spawn(async move {
                let root = fixture1.root();
                while !done1.load(atomic::Ordering::Relaxed) {
                    let file = open_file_checked(
                        &root,
                        "foo",
                        fio::Flags::FLAG_MAYBE_CREATE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::PROTOCOL_FILE,
                        &Default::default(),
                    )
                    .await;
                    let _: u64 = file
                        .write(b"hello")
                        .await
                        .expect("write failed")
                        .map_err(Status::from_raw)
                        .expect("write error");
                }
            }),
            fasync::Task::spawn(async move {
                let root = fixture2.root();
                while !done2.load(atomic::Ordering::Relaxed) {
                    let file = open_file_checked(
                        &root,
                        "foo",
                        fio::Flags::FLAG_MAYBE_CREATE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::PROTOCOL_FILE,
                        &Default::default(),
                    )
                    .await;
                    let _: u64 = file
                        .write(b"hello")
                        .await
                        .expect("write failed")
                        .map_err(Status::from_raw)
                        .expect("write error");
                }
            }),
            fasync::Task::spawn(async move {
                let root = fixture3.root();
                for _ in 0..300 {
                    let file = open_file_checked(
                        &root,
                        "foo",
                        fio::Flags::FLAG_MAYBE_CREATE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::PROTOCOL_FILE,
                        &Default::default(),
                    )
                    .await;
                    assert_eq!(
                        file.close().await.expect("FIDL call failed").map_err(Status::from_raw),
                        Ok(())
                    );
                    root.unlink("foo", &fio::UnlinkOptions::default())
                        .await
                        .expect("FIDL call failed")
                        .expect("unlink failed");
                }
                done.store(true, atomic::Ordering::Relaxed);
            })
        );

        Arc::try_unwrap(fixture).unwrap_or_else(|_| panic!()).close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_backing_memory_shared_vmo_right_write() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.resize(4096)
            .await
            .expect("resize failed")
            .map_err(Status::from_raw)
            .expect("resize error");

        let vmo = file
            .get_backing_memory(fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ)
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::from_raw)
            .expect("Failed to get VMO");
        let err = vmo.write(&[0, 1, 2, 3], 0).expect_err("VMO should not be writable");
        assert_eq!(Status::ACCESS_DENIED, err);

        let vmo = file
            .get_backing_memory(
                fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ | fio::VmoFlags::WRITE,
            )
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::from_raw)
            .expect("Failed to get VMO");
        vmo.write(&[0, 1, 2, 3], 0).expect("VMO should be writable");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_backing_memory_shared_vmo_right_read() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.resize(4096)
            .await
            .expect("resize failed")
            .map_err(Status::from_raw)
            .expect("resize error");

        let mut data = [0u8; 4];
        let vmo = file
            .get_backing_memory(fio::VmoFlags::SHARED_BUFFER)
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::from_raw)
            .expect("Failed to get VMO");
        let err = vmo.read(&mut data, 0).expect_err("VMO should not be readable");
        assert_eq!(Status::ACCESS_DENIED, err);

        let vmo = file
            .get_backing_memory(fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ)
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::from_raw)
            .expect("Failed to get VMO");
        vmo.read(&mut data, 0).expect("VMO should be readable");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_backing_memory_shared_vmo_resize() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let vmo = file
            .get_backing_memory(
                fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ | fio::VmoFlags::WRITE,
            )
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::from_raw)
            .expect("Failed to get VMO");

        // No RESIZE right.
        let err = vmo.set_size(4096).expect_err("VMO should not be resizable");
        assert_eq!(Status::UNAVAILABLE, err);
        // No SET_PROPERTY right.
        let err =
            vmo.set_content_size(&10).expect_err("content size should not be directly modifiable");
        assert_eq!(Status::ACCESS_DENIED, err);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_backing_memory_private_vmo_resize() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let vmo = file
            .get_backing_memory(
                fio::VmoFlags::PRIVATE_CLONE | fio::VmoFlags::READ | fio::VmoFlags::WRITE,
            )
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::from_raw)
            .expect("Failed to get VMO");
        vmo.set_size(10).expect("VMO should be resizable");
        vmo.set_content_size(&20).expect("content size should be modifiable");
        vmo.set_stream_size(20).expect("stream size should be modifiable");

        let vmo = file
            .get_backing_memory(fio::VmoFlags::PRIVATE_CLONE | fio::VmoFlags::READ)
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::from_raw)
            .expect("Failed to get VMO");
        let err = vmo.set_size(10).expect_err("VMO should not be resizable");
        assert_eq!(err, Status::ACCESS_DENIED);
        // This zeroes pages, which can't be done on a read-only VMO.
        vmo.set_stream_size(20).expect_err("stream size is not modifiable");
        vmo.set_content_size(&20).expect_err("content is not modifiable");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn extended_attributes() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let name = b"security.selinux";
        let value_vec = b"bar".to_vec();

        {
            let (iterator_client, iterator_server) =
                fidl::endpoints::create_proxy::<fio::ExtendedAttributeIteratorMarker>();
            file.list_extended_attributes(iterator_server).expect("Failed to make FIDL call");
            let (chunk, last) = iterator_client
                .get_next()
                .await
                .expect("Failed to make FIDL call")
                .expect("Failed to get next iterator chunk");
            assert!(last);
            assert_eq!(chunk, Vec::<Vec<u8>>::new());
        }
        assert_eq!(
            file.get_extended_attribute(name)
                .await
                .expect("Failed to make FIDL call")
                .expect_err("Got successful message back for missing attribute"),
            Status::NOT_FOUND.into_raw(),
        );

        file.set_extended_attribute(
            name,
            fio::ExtendedAttributeValue::Bytes(value_vec.clone()),
            fio::SetExtendedAttributeMode::Set,
        )
        .await
        .expect("Failed to make FIDL call")
        .expect("Failed to set extended attribute");

        {
            let (iterator_client, iterator_server) =
                fidl::endpoints::create_proxy::<fio::ExtendedAttributeIteratorMarker>();
            file.list_extended_attributes(iterator_server).expect("Failed to make FIDL call");
            let (chunk, last) = iterator_client
                .get_next()
                .await
                .expect("Failed to make FIDL call")
                .expect("Failed to get next iterator chunk");
            assert!(last);
            assert_eq!(chunk, vec![name]);
        }
        assert_eq!(
            file.get_extended_attribute(name)
                .await
                .expect("Failed to make FIDL call")
                .expect("Failed to get extended attribute"),
            fio::ExtendedAttributeValue::Bytes(value_vec)
        );

        file.remove_extended_attribute(name)
            .await
            .expect("Failed to make FIDL call")
            .expect("Failed to remove extended attribute");

        {
            let (iterator_client, iterator_server) =
                fidl::endpoints::create_proxy::<fio::ExtendedAttributeIteratorMarker>();
            file.list_extended_attributes(iterator_server).expect("Failed to make FIDL call");
            let (chunk, last) = iterator_client
                .get_next()
                .await
                .expect("Failed to make FIDL call")
                .expect("Failed to get next iterator chunk");
            assert!(last);
            assert_eq!(chunk, Vec::<Vec<u8>>::new());
        }
        assert_eq!(
            file.get_extended_attribute(name)
                .await
                .expect("Failed to make FIDL call")
                .expect_err("Got successful message back for missing attribute"),
            Status::NOT_FOUND.into_raw(),
        );

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_flush_when_closed_from_on_zero_children() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.resize(50).await.expect("resize (FIDL) failed").expect("resize failed");

        {
            let vmo = file
                .get_backing_memory(fio::VmoFlags::READ | fio::VmoFlags::WRITE)
                .await
                .expect("get_backing_memory (FIDL) failed")
                .map_err(Status::from_raw)
                .expect("get_backing_memory failed");

            std::mem::drop(file);

            fasync::unblock(move || vmo.write(b"hello", 0).expect("write failed")).await;
        }

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_get_attributes_fsverity_enabled_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let mut data: Vec<u8> = vec![0x00u8; 1052672];
        thread_rng().fill(&mut data[..]);

        for chunk in data.chunks(8192) {
            file.write(chunk)
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("write failed");
        }

        let tree = fsverity_merkle::from_slice(
            &data,
            FsVerityHasher::Sha256(FsVerityHasherOptions::new(vec![0xFF; 8], 4096)),
        );
        let mut expected_root = tree.root().to_vec();
        expected_root.extend_from_slice(&[0; 32]);

        let expected_descriptor = fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        };

        file.enable_verity(&expected_descriptor)
            .await
            .expect("FIDL transport error")
            .expect("enable verity failed");

        let (_, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::ROOT_HASH | fio::NodeAttributesQuery::OPTIONS)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("get_attributes failed");

        assert_eq!(
            immutable_attributes
                .options
                .expect("verification options not present in immutable attributes"),
            expected_descriptor
        );
        assert_eq!(
            immutable_attributes.root_hash.expect("root hash not present in immutable attributes"),
            expected_root
        );

        fixture.close().await;
    }

    /// Verify that once we enable verity on a file, it can never be written to or resized.
    /// This applies even to connections that have [`fio::PERM_WRITABLE`].
    #[fuchsia::test]
    async fn test_write_fail_fsverity_enabled_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.write(&[8; 8192])
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("write failed");

        let descriptor = fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        };

        file.enable_verity(&descriptor)
            .await
            .expect("FIDL transport error")
            .expect("enable verity failed");

        async fn assert_file_is_not_writable(file: &fio::FileProxy) {
            // Writes via FIDL should fail
            file.write(&[2; 8192])
                .await
                .expect("FIDL transport error")
                .map_err(Status::from_raw)
                .expect_err("write succeeded on fsverity-enabled file");
            // Writes via the pager should fail
            let vmo = file
                .get_backing_memory(fio::VmoFlags::READ | fio::VmoFlags::WRITE)
                .await
                .expect("FIDL transport error")
                .map_err(Status::from_raw)
                .expect("get_backing_memory failed");
            fasync::unblock(move || {
                vmo.write(&[2; 8192], 0)
                    .expect_err("write via VMO succeeded on fsverity-enabled file");
            })
            .await;
            // Truncation should fail
            file.resize(1)
                .await
                .expect("FIDL transport error")
                .map_err(Status::from_raw)
                .expect_err("resize succeeded on fsverity-enabled file");
        }

        assert_file_is_not_writable(&file).await;
        close_file_checked(file).await;

        // Ensure that even if new writable connections are created, those also cannot write.
        let file =
            open_file(&root, "foo", fio::PERM_READABLE | fio::PERM_WRITABLE, &Default::default())
                .await
                .expect("failed to open fsverity-enabled file");
        assert_file_is_not_writable(&file).await;
        close_file_checked(file).await;

        // Reopen the filesystem and ensure that the file can't be written to.
        let device = fixture.close().await;
        device.ensure_unique();
        device.reopen(false);
        let fixture =
            TestFixture::open(device, TestFixtureOptions { format: false, ..Default::default() })
                .await;

        let root = fixture.root();
        let file =
            open_file(&root, "foo", fio::PERM_READABLE | fio::PERM_WRITABLE, &Default::default())
                .await
                .expect("failed to open fsverity-enabled file");
        assert_file_is_not_writable(&file).await;
        close_file_checked(file).await;

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_fsverity_enabled_file_verified_reads() {
        let mut data: Vec<u8> = vec![0x00u8; 1052672];
        thread_rng().fill(&mut data[..]);
        let mut num_chunks = 0;

        let reused_device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            for chunk in data.chunks(fio::MAX_BUF as usize) {
                file.write(chunk)
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::from_raw)
                    .expect("write failed");
                num_chunks += 1;
            }

            let descriptor = fio::VerificationOptions {
                hash_algorithm: Some(fio::HashAlgorithm::Sha256),
                salt: Some(vec![0xFF; 8]),
                ..Default::default()
            };

            file.enable_verity(&descriptor)
                .await
                .expect("FIDL transport error")
                .expect("enable verity failed");

            assert!(file.sync().await.expect("Sync failed").is_ok());
            close_file_checked(file).await;
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        for chunk in 0..num_chunks {
            let buffer = file
                .read(fio::MAX_BUF)
                .await
                .expect("transport error on read")
                .expect("read failed");
            let start = chunk * fio::MAX_BUF as usize;
            assert_eq!(&buffer, &data[start..start + buffer.len()]);
        }

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_enabling_verity_on_verified_file_fails() {
        let reused_device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            file.write(&[1; 8192])
                .await
                .expect("FIDL call failed")
                .map_err(Status::from_raw)
                .expect("write failed");

            let descriptor = fio::VerificationOptions {
                hash_algorithm: Some(fio::HashAlgorithm::Sha256),
                salt: Some(vec![0xFF; 8]),
                ..Default::default()
            };

            file.enable_verity(&descriptor)
                .await
                .expect("FIDL transport error")
                .expect("enable verity failed");

            file.enable_verity(&descriptor)
                .await
                .expect("FIDL transport error")
                .expect_err("enabling verity on a verity-enabled file should fail.");

            assert!(file.sync().await.expect("Sync failed").is_ok());
            close_file_checked(file).await;
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let descriptor = fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        };

        file.enable_verity(&descriptor)
            .await
            .expect("FIDL transport error")
            .expect_err("enabling verity on a verity-enabled file should fail.");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_get_attributes_fsverity_not_enabled() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let mut data: Vec<u8> = vec![0x00u8; 8192];
        thread_rng().fill(&mut data[..]);

        file.write(&data)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("write failed");

        let () = file
            .sync()
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("sync failed");

        let (_, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::ROOT_HASH | fio::NodeAttributesQuery::OPTIONS)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("get_attributes failed");

        assert_eq!(immutable_attributes.options, None);
        assert_eq!(immutable_attributes.root_hash, None);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_update_attributes_also_updates_ctime() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        // Writing to file should update ctime
        file.write("hello, world!".as_bytes())
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("write failed");
        let (_mutable_attributes, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::CHANGE_TIME)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("get_attributes failed");
        let ctime_after_write = immutable_attributes.change_time;

        // Updating file attributes updates ctime as well
        file.update_attributes(&fio::MutableNodeAttributes {
            mode: Some(111),
            gid: Some(222),
            ..Default::default()
        })
        .await
        .expect("FIDL call failed")
        .map_err(Status::from_raw)
        .expect("update_attributes failed");
        let (_mutable_attributes, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::CHANGE_TIME)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("get_attributes failed");
        let ctime_after_update = immutable_attributes.change_time;
        assert!(ctime_after_update > ctime_after_write);

        // Flush metadata
        file.sync()
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("sync failed");
        let (_mutable_attributes, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::CHANGE_TIME)
            .await
            .expect("FIDL call failed")
            .map_err(Status::from_raw)
            .expect("get_attributes failed");
        let ctime_after_sync = immutable_attributes.change_time;
        assert_eq!(ctime_after_sync, ctime_after_update);
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_can_read_and_write_to_it() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;

        let buf = vec![0xaa as u8; 8];
        file::write(&tmpfile, buf.as_slice()).await.expect("Failed to write to file");

        tmpfile
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek failed")
            .map_err(zx::Status::from_raw)
            .expect("seek error");
        let read_buf = file::read(&tmpfile).await.expect("read failed");
        assert_eq!(read_buf, buf);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_get_space_back_after_closing_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;

        const BUFFER_SIZE: u64 = 1024 * 1024;
        let buf = vec![0xaa as u8; BUFFER_SIZE as usize];
        file::write(&tmpfile, buf.as_slice()).await.expect("Failed to write to file");

        let info_after_writing_to_tmpfile = root
            .query_filesystem()
            .await
            .expect("Failed wire call to query filesystem")
            .1
            .expect("Failed to query filesystem");

        close_file_checked(tmpfile).await;

        // We will get space back soon after closing the file buy maybe not immediately.
        for i in 1..50 {
            let info = root
                .query_filesystem()
                .await
                .expect("Failed wire call to query filesystem")
                .1
                .expect("Failed to query filesystem");

            // We should claim back at least that amount of data we wrote to the file. There might
            // be some metadata left that will not be removed until compaction.
            if info_after_writing_to_tmpfile.used_bytes - info.used_bytes >= BUFFER_SIZE {
                break;
            }
            if i == 49 {
                panic!("Did not get space back from unnamed temporary file after closing it.");
            }
        }

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_get_space_back_after_closing_device() {
        const BUFFER_SIZE: u64 = 1024 * 1024;

        let (reused_device, info_after_writing_to_tmpfile) = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let tmpfile = open_file_checked(
                &root,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_WRITABLE,
                &fio::Options::default(),
            )
            .await;

            let buf = vec![0xaa as u8; BUFFER_SIZE as usize];
            file::write(&tmpfile, buf.as_slice()).await.expect("Failed to write to file");

            let info_after_writing_to_tmpfile = root
                .query_filesystem()
                .await
                .expect("Failed wire call to query filesystem")
                .1
                .expect("Failed to query filesystem");

            (fixture.close().await, info_after_writing_to_tmpfile)
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let info = root
            .query_filesystem()
            .await
            .expect("Failed wire call to query filesystem")
            .1
            .expect("Failed to query filesystem");

        // We should claim back at least that amount of data we wrote to the file after rebooting
        // device. There might be some metadata left that will not be removed until compaction.
        assert!(info_after_writing_to_tmpfile.used_bytes - info.used_bytes >= BUFFER_SIZE);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_can_link_into() {
        const FILE1: &str = "foo";
        const FILE2: &str = "bar";
        const BUFFER_SIZE: u64 = 1024 * 1024;
        let buf = vec![0xaa as u8; BUFFER_SIZE as usize];

        let reused_device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let tmpfile = open_file_checked(
                &root,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                &fio::Options::default(),
            )
            .await;

            // Link temporary unnamed file into filesystem, making it permanent.
            let (status, dst_token) = root.get_token().await.expect("FIDL call failed");
            zx::Status::ok(status).expect("get_token failed");
            tmpfile
                .link_into(zx::Event::from(dst_token.unwrap()), FILE1)
                .await
                .expect("link_into wire message failed")
                .map_err(zx::Status::from_raw)
                .expect("link_into failed");

            // We should be able to link the temporary file proxy multiple times.
            let (status, dst_token) = root.get_token().await.expect("FIDL call failed");
            zx::Status::ok(status).expect("get_token failed");
            tmpfile
                .link_into(zx::Event::from(dst_token.unwrap()), FILE2)
                .await
                .expect("link_into wire message failed")
                .map_err(zx::Status::from_raw)
                .expect("link_into failed");

            // Write to tmpfile, we should see the contents of it when reading from FILE1 or FILE2.
            file::write(&tmpfile, buf.as_slice()).await.expect("Failed to write to file");

            root.unlink(FILE1, &fio::UnlinkOptions::default())
                .await
                .expect("unlink wire call failed")
                .map_err(zx::Status::from_raw)
                .expect("unlink failed");
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        // FILE1 was unlinked, so we should not be able to open a connection to it.
        assert_eq!(
            open_file(
                &root,
                FILE1,
                fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
                &fio::Options::default()
            )
            .await
            .expect_err("Open succeeded unexpectedly")
            .root_cause()
            .downcast_ref::<zx::Status>()
            .expect("No status"),
            &zx::Status::NOT_FOUND,
        );

        // The temporary unnamed file was linked to FILE2. We should find the same contents written
        // to it.
        let permanent_file = open_file_checked(
            &root,
            FILE2,
            fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE,
            &fio::Options::default(),
        )
        .await;
        permanent_file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek wire message failed")
            .map_err(zx::Status::from_raw)
            .expect("seek error");
        let read_buf = file::read(&permanent_file).await.expect("read failed");
        assert!(read_buf == buf);

        fsck(fixture.fs().clone()).await.expect("fsck failed");

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_in_encrypted_directory() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        // Set up encrypted directory
        let crypt = fixture.crypt().unwrap();
        let encrypted_directory = open_dir_checked(
            &root,
            "encrypted_directory",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_DIRECTORY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            fio::Options::default(),
        )
        .await;
        let wrapping_key_id = 123;
        crypt.add_wrapping_key(wrapping_key_id, [1; 32].into());
        encrypted_directory
            .update_attributes(&fio::MutableNodeAttributes {
                wrapping_key_id: Some(wrapping_key_id.to_le_bytes()),
                ..Default::default()
            })
            .await
            .expect("update_attributes wire call failed")
            .map_err(zx::ok)
            .expect("update_attributes failed");

        // Create a temporary unnamed file in that directory, it should have the same wrapping key.
        let encryped_tmpfile = open_file_checked(
            &encrypted_directory,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        let (mutable_attributes, _immutable_attributes) = encryped_tmpfile
            .get_attributes(fio::NodeAttributesQuery::WRAPPING_KEY_ID)
            .await
            .expect("get_attributes wire call failed")
            .map_err(zx::Status::from_raw)
            .expect("get_attributes failed");
        assert_eq!(mutable_attributes.wrapping_key_id, Some(wrapping_key_id.to_le_bytes()));

        // Similar to a regular file, linking a temporary unnamed file into the directory will only
        // work if they have the same wrapping key ID.
        let (status, dst_token) = encrypted_directory.get_token().await.expect("FIDL call failed");
        zx::Status::ok(status).expect("get_token failed");
        encryped_tmpfile
            .link_into(zx::Event::from(dst_token.unwrap()), "foo")
            .await
            .expect("link_into wire message failed")
            .expect("link_into failed");

        let unencryped_tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        let (mutable_attributes, _immutable_attributes) = unencryped_tmpfile
            .get_attributes(fio::NodeAttributesQuery::WRAPPING_KEY_ID)
            .await
            .expect("get_attributes wire call failed")
            .map_err(zx::Status::from_raw)
            .expect("get_attributes failed");
        assert_eq!(mutable_attributes.wrapping_key_id, None);
        let (status, dst_token) = encrypted_directory.get_token().await.expect("FIDL call failed");
        zx::Status::ok(status).expect("get_token failed");
        assert_eq!(
            unencryped_tmpfile
                .link_into(zx::Event::from(dst_token.unwrap()), "bar")
                .await
                .expect("link_into wire message failed")
                .map_err(zx::Status::from_raw)
                .expect_err("link_into passed unexpectedly"),
            zx::Status::BAD_STATE,
        );

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_in_locked_directory() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        // Set up encrypted directory
        let crypt = fixture.crypt().unwrap();
        let encrypted_directory = open_dir_checked(
            &root,
            "encrypted_directory",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_DIRECTORY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            fio::Options::default(),
        )
        .await;
        let wrapping_key_id = 123;
        crypt.add_wrapping_key(wrapping_key_id, [1; 32].into());
        encrypted_directory
            .update_attributes(&fio::MutableNodeAttributes {
                wrapping_key_id: Some(wrapping_key_id.to_le_bytes()),
                ..Default::default()
            })
            .await
            .expect("update_attributes wire call failed")
            .map_err(zx::ok)
            .expect("update_attributes failed");

        // This locks the directory
        crypt.remove_wrapping_key(wrapping_key_id);

        // Open unnamed temporary file in a locked directory and should return (key) NOT_FOUND.
        assert_eq!(
            open_file(
                &encrypted_directory,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                &fio::Options::default()
            )
            .await
            .expect_err("Open succeeded unexpectedly")
            .root_cause()
            .downcast_ref::<zx::Status>()
            .expect("No status"),
            &zx::Status::NOT_FOUND,
        );
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_link_into_with_race() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        for i in 1..100 {
            let tmpfile = open_file_checked(
                &root,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                &fio::Options::default(),
            )
            .await;

            // Clone tmpfile proxy to use in the separate threads.
            let (tmpfile_clone1, tmpfile_server1) =
                fidl::endpoints::create_proxy::<fio::FileMarker>();
            tmpfile.clone(tmpfile_server1.into_channel().into()).expect("clone failed");
            let (tmpfile_clone2, tmpfile_server2) =
                fidl::endpoints::create_proxy::<fio::FileMarker>();
            tmpfile.clone(tmpfile_server2.into_channel().into()).expect("clone failed");

            // Get the open connection to the sub directory which we would attempt to link the
            // unnamed temporary file into.
            let sub_dir = open_dir_checked(
                &root,
                "A",
                fio::Flags::PROTOCOL_DIRECTORY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::FLAG_MAYBE_CREATE,
                fio::Options::default(),
            )
            .await;

            // Get tokens to the sub directory to use for `link_into`.
            let (status, dst_token1) = sub_dir.get_token().await.expect("FIDL call failed");
            zx::Status::ok(status).expect("get_token failed");
            let (status, dst_token2) = sub_dir.get_token().await.expect("FIDL call failed");
            zx::Status::ok(status).expect("get_token failed");

            join!(
                fasync::Task::spawn(async move {
                    tmpfile_clone1
                        .link_into(zx::Event::from(dst_token1.unwrap()), &(2 * i).to_string())
                        .await
                        .expect("link_into wire message failed")
                        .expect("link_into failed");
                }),
                fasync::Task::spawn(async move {
                    tmpfile_clone2
                        .link_into(zx::Event::from(dst_token2.unwrap()), &(2 * i + 1).to_string())
                        .await
                        .expect("link_into wire message failed")
                        .expect("link_into failed");
                })
            );
            let (_, immutable_attributes) = tmpfile
                .get_attributes(fio::NodeAttributesQuery::LINK_COUNT)
                .await
                .expect("Failed get_attributes wire call")
                .expect("get_attributes failed");
            assert_eq!(immutable_attributes.link_count.unwrap(), 2);
            close_file_checked(tmpfile).await;
        }
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_update_attributes_persists() {
        const FILE: &str = "foo";
        let mtime = Some(Timestamp::now().as_nanos());
        let atime = Some(Timestamp::now().as_nanos());
        let mode = Some(111);

        let device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                FILE,
                fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_WRITABLE | fio::Flags::PROTOCOL_FILE,
                &fio::Options::default(),
            )
            .await;

            file.update_attributes(&fio::MutableNodeAttributes {
                modification_time: mtime,
                access_time: atime,
                mode: Some(111),
                ..Default::default()
            })
            .await
            .expect("update_attributes FIDL call failed")
            .map_err(zx::ok)
            .expect("update_attributes failed");

            // Calling close should flush the node attributes to the device.
            fixture.close().await
        };

        let fixture =
            TestFixture::open(device, TestFixtureOptions { format: false, ..Default::default() })
                .await;
        let root = fixture.root();
        let file = open_file_checked(
            &root,
            FILE,
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &fio::Options::default(),
        )
        .await;

        let (mutable_attributes, _immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::MODIFICATION_TIME
                    | fio::NodeAttributesQuery::ACCESS_TIME
                    | fio::NodeAttributesQuery::MODE,
            )
            .await
            .expect("update_attributesFIDL call failed")
            .map_err(zx::ok)
            .expect("get_attributes failed");
        assert_eq!(mutable_attributes.modification_time, mtime);
        assert_eq!(mutable_attributes.access_time, atime);
        assert_eq!(mutable_attributes.mode, mode);
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_atime_from_pending_access_time_update_request() {
        const FILE: &str = "foo";

        let (device, expected_atime, expected_ctime) = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                FILE,
                fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_WRITABLE | fio::Flags::PROTOCOL_FILE,
                &fio::Options::default(),
            )
            .await;

            let (mutable_attributes, immutable_attributes) = file
                .get_attributes(
                    fio::NodeAttributesQuery::CHANGE_TIME
                        | fio::NodeAttributesQuery::ACCESS_TIME
                        | fio::NodeAttributesQuery::MODIFICATION_TIME,
                )
                .await
                .expect("update_attributes FIDL call failed")
                .map_err(zx::ok)
                .expect("get_attributes failed");
            let initial_ctime = immutable_attributes.change_time;
            let initial_atime = mutable_attributes.access_time;
            // When creating a file, ctime, mtime, and atime are all updated to the current time.
            assert_eq!(initial_atime, initial_ctime);
            assert_eq!(initial_atime, mutable_attributes.modification_time);

            // Client manages atime and they signal to Fxfs that a file access has occurred and it
            // may require an access time update. They do so by querying with
            // `fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE`.
            let (mutable_attributes, immutable_attributes) = file
                .get_attributes(
                    fio::NodeAttributesQuery::CHANGE_TIME
                        | fio::NodeAttributesQuery::ACCESS_TIME
                        | fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE,
                )
                .await
                .expect("update_attributes FIDL call failed")
                .map_err(zx::ok)
                .expect("get_attributes failed");
            // atime will be updated as atime <= ctime (or mtime)
            assert!(initial_atime < mutable_attributes.access_time);
            let updated_atime = mutable_attributes.access_time;
            // Calling get_attributes with PENDING_ACCESS_TIME_UPDATE will trigger an update of
            // object attributes if access_time needs to be updated. Check that ctime isn't updated.
            assert_eq!(initial_ctime, immutable_attributes.change_time);

            let (mutable_attributes, _) = file
                .get_attributes(
                    fio::NodeAttributesQuery::ACCESS_TIME
                        | fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE,
                )
                .await
                .expect("update_attributes FIDL call failed")
                .map_err(zx::ok)
                .expect("get_attributes failed");
            // atime will be not be updated as atime > ctime (or mtime)
            assert_eq!(updated_atime, mutable_attributes.access_time);

            (fixture.close().await, mutable_attributes.access_time, initial_ctime)
        };

        let fixture =
            TestFixture::open(device, TestFixtureOptions { format: false, ..Default::default() })
                .await;
        let root = fixture.root();
        let file = open_file_checked(
            &root,
            FILE,
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &fio::Options::default(),
        )
        .await;

        // Make sure that the pending atime update persisted.
        let (mutable_attributes, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::CHANGE_TIME | fio::NodeAttributesQuery::ACCESS_TIME,
            )
            .await
            .expect("update_attributesFIDL call failed")
            .map_err(zx::ok)
            .expect("get_attributes failed");

        assert_eq!(immutable_attributes.change_time, expected_ctime);
        assert_eq!(mutable_attributes.access_time, expected_atime);
        fixture.close().await;
    }
}
