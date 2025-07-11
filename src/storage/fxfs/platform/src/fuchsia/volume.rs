// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fuchsia::component::map_to_raw_status;
use crate::fuchsia::directory::FxDirectory;
use crate::fuchsia::dirent_cache::DirentCache;
use crate::fuchsia::file::FxFile;
use crate::fuchsia::memory_pressure::{MemoryPressureLevel, MemoryPressureMonitor};
use crate::fuchsia::node::{FxNode, GetResult, NodeCache};
use crate::fuchsia::pager::Pager;
use crate::fuchsia::profile::ProfileState;
use crate::fuchsia::symlink::FxSymlink;
use crate::fuchsia::volumes_directory::VolumesDirectory;
use anyhow::{bail, ensure, Error};
use async_trait::async_trait;
use fidl::endpoints::ServerEnd;
use fidl::AsHandleRef;
use fidl_fuchsia_fxfs::{
    BytesAndNodes, FileBackedVolumeProviderRequest, FileBackedVolumeProviderRequestStream,
    ProjectIdRequest, ProjectIdRequestStream, ProjectIterToken,
};
use fs_inspect::{FsInspectVolume, VolumeData};
use fuchsia_sync::Mutex;
use futures::channel::oneshot;
use futures::stream::{self, FusedStream, Stream};
use futures::{FutureExt, StreamExt, TryStreamExt};
use fxfs::errors::FxfsError;
use fxfs::filesystem::{self, SyncOptions};
use fxfs::future_with_guard::FutureWithGuard;
use fxfs::log::*;
use fxfs::object_store::directory::Directory;
use fxfs::object_store::transaction::{lock_keys, LockKey, Options};
use fxfs::object_store::{HandleOptions, HandleOwner, ObjectDescriptor, ObjectStore};
use std::future::Future;
use std::pin::pin;
#[cfg(any(test, feature = "testing"))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;
use vfs::directory::entry::DirectoryEntry;
use vfs::directory::simple::Simple;
use vfs::execution_scope::ExecutionScope;

use {fidl_fuchsia_io as fio, fuchsia_async as fasync};

// LINT.IfChange
// TODO:(b/299919008) Fix this number to something reasonable, or maybe just for fxblob.
const DIRENT_CACHE_LIMIT: usize = 2000;
// LINT.ThenChange(//src/storage/stressor/src/aggressive.rs)

/// The smallest read-ahead size. All other read-ahead sizes will be a multiple of this read-ahead
/// size. Having this property allows for chunking metadata at this granularity.
pub const BASE_READ_AHEAD_SIZE: u64 = 32 * 1024;
pub const MAX_READ_AHEAD_SIZE: u64 = BASE_READ_AHEAD_SIZE * 4;

const PROFILE_DIRECTORY: &str = "profiles";

#[derive(Clone)]
pub struct MemoryPressureLevelConfig {
    /// The period to wait between flushes, as well as perform other background maintenance tasks
    /// (e.g. purging caches).
    pub background_task_period: Duration,

    /// The limit of cached nodes.
    pub cache_size_limit: usize,

    /// The initial delay before the background task runs. The background task has a longer initial
    /// delay to avoid running the task during boot.
    pub background_task_initial_delay: Duration,

    /// The amount of read-ahead to do. The read-ahead size is reduce when under memory pressure.
    /// The kernel starts evicting pages when under memory pressure so over supplied pages are less
    /// likely to be used before being evicted.
    pub read_ahead_size: u64,
}

impl Default for MemoryPressureLevelConfig {
    fn default() -> Self {
        Self {
            background_task_period: Duration::from_secs(20),
            cache_size_limit: DIRENT_CACHE_LIMIT,
            background_task_initial_delay: Duration::from_secs(70),
            read_ahead_size: MAX_READ_AHEAD_SIZE,
        }
    }
}

#[derive(Clone)]
pub struct MemoryPressureConfig {
    /// The configuration to use at [`MemoryPressureLevel::Normal`].
    pub mem_normal: MemoryPressureLevelConfig,

    /// The configuration to use at [`MemoryPressureLevel::Warning`].
    pub mem_warning: MemoryPressureLevelConfig,

    /// The configuration to use at [`MemoryPressureLevel::Critical`].
    pub mem_critical: MemoryPressureLevelConfig,
}

impl MemoryPressureConfig {
    pub fn for_level(&self, level: &MemoryPressureLevel) -> &MemoryPressureLevelConfig {
        match level {
            MemoryPressureLevel::Normal => &self.mem_normal,
            MemoryPressureLevel::Warning => &self.mem_warning,
            MemoryPressureLevel::Critical => &self.mem_critical,
        }
    }
}

impl Default for MemoryPressureConfig {
    fn default() -> Self {
        // TODO(https://fxbug.dev/42061389): investigate a smarter strategy for determining flush
        // frequency.
        Self {
            mem_normal: MemoryPressureLevelConfig {
                background_task_period: Duration::from_secs(20),
                cache_size_limit: DIRENT_CACHE_LIMIT,
                background_task_initial_delay: Duration::from_secs(70),
                read_ahead_size: MAX_READ_AHEAD_SIZE,
            },
            mem_warning: MemoryPressureLevelConfig {
                background_task_period: Duration::from_secs(5),
                cache_size_limit: 100,
                background_task_initial_delay: Duration::from_secs(5),
                read_ahead_size: BASE_READ_AHEAD_SIZE * 2,
            },
            mem_critical: MemoryPressureLevelConfig {
                background_task_period: Duration::from_millis(1500),
                cache_size_limit: 20,
                background_task_initial_delay: Duration::from_millis(1500),
                read_ahead_size: BASE_READ_AHEAD_SIZE,
            },
        }
    }
}

/// FxVolume represents an opened volume. It is also a (weak) cache for all opened Nodes within the
/// volume.
pub struct FxVolume {
    parent: Weak<VolumesDirectory>,
    cache: NodeCache,
    store: Arc<ObjectStore>,
    pager: Pager,
    executor: fasync::EHandle,

    // A tuple of the actual task and a channel to signal to terminate the task.
    background_task: Mutex<Option<(fasync::Task<()>, oneshot::Sender<()>)>>,

    // Unique identifier of the filesystem that owns this volume.
    fs_id: u64,

    // The execution scope for this volume.
    scope: ExecutionScope,

    dirent_cache: DirentCache,

    profile_state: Mutex<Option<Box<dyn ProfileState>>>,

    /// This is updated based on the memory-pressure level.
    read_ahead_size: AtomicU64,

    #[cfg(any(test, feature = "testing"))]
    poisoned: AtomicBool,

    #[cfg(any(test, feature = "refault-tracking"))]
    refault_tracker: RefaultTracker,
}

#[fxfs_trace::trace]
impl FxVolume {
    pub fn new(
        parent: Weak<VolumesDirectory>,
        store: Arc<ObjectStore>,
        fs_id: u64,
    ) -> Result<Self, Error> {
        let scope = ExecutionScope::new();
        Ok(Self {
            parent,
            cache: NodeCache::new(),
            store,
            pager: Pager::new(scope.clone())?,
            executor: fasync::EHandle::local(),
            background_task: Mutex::new(None),
            fs_id,
            scope,
            dirent_cache: DirentCache::new(DIRENT_CACHE_LIMIT),
            profile_state: Mutex::new(None),
            read_ahead_size: AtomicU64::new(
                MemoryPressureConfig::default().mem_normal.read_ahead_size,
            ),
            #[cfg(any(test, feature = "testing"))]
            poisoned: AtomicBool::new(false),
            #[cfg(any(test, feature = "refault-tracking"))]
            refault_tracker: RefaultTracker::default(),
        })
    }

    pub fn store(&self) -> &Arc<ObjectStore> {
        &self.store
    }

    pub fn cache(&self) -> &NodeCache {
        &self.cache
    }

    pub fn dirent_cache(&self) -> &DirentCache {
        &self.dirent_cache
    }

    pub fn pager(&self) -> &Pager {
        &self.pager
    }

    pub fn id(&self) -> u64 {
        self.fs_id
    }

    pub fn scope(&self) -> &ExecutionScope {
        &self.scope
    }

    /// Stop profiling, recover resources from it and finalize recordings.
    pub async fn stop_profile_tasks(self: &Arc<Self>) {
        let Some(mut state) = self.profile_state.lock().take() else { return };
        state.wait_for_replay_to_finish().await;
        self.pager.set_recorder(None);
        let _ = state.wait_for_recording_to_finish().await;
    }

    /// Opens or creates the profile directory in the volume's internal directory.
    pub async fn get_profile_directory(self: &Arc<Self>) -> Result<Directory<FxVolume>, Error> {
        let internal_dir = self
            .get_or_create_internal_dir()
            .await
            .map_err(|e| e.context("Opening internal directory"))?;
        // Have to do separate calls to create the profile dir if necessary.
        let mut transaction = self
            .store()
            .filesystem()
            .new_transaction(
                lock_keys![LockKey::object(
                    self.store().store_object_id(),
                    internal_dir.object_id(),
                )],
                Options::default(),
            )
            .await?;
        Ok(match internal_dir.directory().lookup(PROFILE_DIRECTORY).await? {
            Some((object_id, _, _)) => {
                Directory::open_unchecked(self.clone(), object_id, None, false)
            }
            None => {
                let new_dir = internal_dir
                    .directory()
                    .create_child_dir(&mut transaction, PROFILE_DIRECTORY)
                    .await?;
                transaction.commit().await?;
                new_dir
            }
        })
    }

    /// Starts recording a profile for the volume under the name given, and if a profile exists
    /// under that same name it is replayed and will be replaced after by the new recording if it
    /// is cleanly shutdown and finalized.
    pub async fn record_or_replay_profile(
        self: &Arc<Self>,
        mut state: Box<dyn ProfileState>,
        name: &str,
    ) -> Result<(), Error> {
        // We don't meddle in FxDirectory or FxFile here because we don't want a paged object.
        // Normally we ensure that there's only one copy by using the Node cache on the volume, but
        // that would create FxFile, so in this case we just assume that only one profile operation
        // should be ongoing at a time, as that is ensured in `VolumesDirectory`.

        // If there is a recording already, prepare to replay it.
        let profile_dir = self.get_profile_directory().await?;
        let replay_handle = if let Some((id, descriptor, _)) = profile_dir.lookup(name).await? {
            ensure!(matches!(descriptor, ObjectDescriptor::File), FxfsError::Inconsistent);
            Some(Box::new(
                ObjectStore::open_object(self, id, HandleOptions::default(), None).await?,
            ))
        } else {
            None
        };

        let mut profile_state = self.profile_state.lock();

        info!("Recording new profile '{name}' for volume object {}", self.store.store_object_id());
        // Begin recording first to ensure that we capture any activity from the replay.
        self.pager.set_recorder(Some(state.record_new(self, name)));
        if let Some(handle) = replay_handle {
            if let Some(guard) = self.scope().try_active_guard() {
                state.replay_profile(handle, self.clone(), guard);
                info!(
                    "Replaying existing profile '{name}' for volume object {}",
                    self.store.store_object_id()
                );
            }
        }
        *profile_state = Some(state);
        Ok(())
    }

    async fn get_or_create_internal_dir(self: &Arc<Self>) -> Result<Arc<FxDirectory>, Error> {
        let internal_data_id = self.store().get_or_create_internal_directory_id().await?;
        let internal_dir = self
            .get_or_load_node(internal_data_id, ObjectDescriptor::Directory, None)
            .await?
            .into_any()
            .downcast::<FxDirectory>()
            .unwrap();
        Ok(internal_dir)
    }

    pub async fn terminate(&self) {
        let task = std::mem::replace(&mut *self.background_task.lock(), None);
        if let Some((task, terminate)) = task {
            let _ = terminate.send(());
            task.await;
        }

        // `NodeCache::terminate` will break any strong reference cycles contained within nodes
        // (pager registration). The only remaining nodes should be those with open FIDL
        // connections or vmo references in the process of handling the VMO_ZERO_CHILDREN signal.
        // `ExecutionScope::shutdown` + `ExecutionScope::wait` will close the open FIDL connections
        // and synchonrize the signal handling which should result in all nodes flushing and then
        // dropping. Any async tasks required to flush a node should take an active guard on the
        // `ExecutionScope` which will prevent `ExecutionScope::wait` from completing until all
        // nodes are flushed.
        self.scope.shutdown();
        self.cache.terminate();
        self.scope.wait().await;

        // The dirent_cache must be cleared *after* shutting down the scope because there can be
        // tasks that insert entries into the cache.
        self.dirent_cache.clear();

        if self.store.filesystem().options().read_only {
            // If the filesystem is read only, we don't need to flush/sync anything.
            if self.store.is_unlocked() {
                self.store.lock_read_only();
            }
            return;
        }

        self.flush_all_files(true).await;
        self.store.filesystem().graveyard().flush().await;
        if self.store.crypt().is_some() {
            if let Err(e) = self.store.lock().await {
                // The store will be left in a safe state and there won't be data-loss unless
                // there's an issue flushing the journal later.
                warn!(error:? = e; "Locking store error");
            }
        }
        let sync_status = self
            .store
            .filesystem()
            .sync(SyncOptions { flush_device: true, ..Default::default() })
            .await;
        if let Err(e) = sync_status {
            error!(error:? = e; "Failed to sync filesystem; data may be lost");
        }
    }

    /// Attempts to get a node from the node cache. If the node wasn't present in the cache, loads
    /// the object from the object store, installing the returned node into the cache and returns
    /// the newly created FxNode backed by the loaded object.  |parent| is only set on the node if
    /// the node was not present in the cache.  Otherwise, it is ignored.
    pub async fn get_or_load_node(
        self: &Arc<Self>,
        object_id: u64,
        object_descriptor: ObjectDescriptor,
        parent: Option<Arc<FxDirectory>>,
    ) -> Result<Arc<dyn FxNode>, Error> {
        match self.cache.get_or_reserve(object_id).await {
            GetResult::Node(node) => Ok(node),
            GetResult::Placeholder(placeholder) => {
                let node = match object_descriptor {
                    ObjectDescriptor::File => FxFile::new(
                        ObjectStore::open_object(self, object_id, HandleOptions::default(), None)
                            .await?,
                    ) as Arc<dyn FxNode>,
                    ObjectDescriptor::Directory => {
                        // Can't use open_unchecked because we don't know if the dir is casefolded
                        // or encrypted.
                        Arc::new(FxDirectory::new(parent, Directory::open(self, object_id).await?))
                            as Arc<dyn FxNode>
                    }
                    ObjectDescriptor::Symlink => Arc::new(FxSymlink::new(self.clone(), object_id)),
                    _ => bail!(FxfsError::Inconsistent),
                };
                placeholder.commit(&node);
                Ok(node)
            }
        }
    }

    /// Marks the given directory deleted.
    pub fn mark_directory_deleted(&self, object_id: u64) {
        if let Some(node) = self.cache.get(object_id) {
            // It's possible that node is a placeholder, in which case we don't need to wait for it
            // to be resolved because it should be blocked behind the locks that are held by the
            // caller, and once they're dropped, it'll be found to be deleted via the tree.
            if let Ok(dir) = node.into_any().downcast::<FxDirectory>() {
                dir.set_deleted();
            }
        }
    }

    /// Removes resources associated with |object_id| (which ought to be a file), if there are no
    /// open connections to that file.
    ///
    /// This must be called *after committing* a transaction which deletes the last reference to
    /// |object_id|, since before that point, new connections could be established.
    pub(super) async fn maybe_purge_file(&self, object_id: u64) -> Result<(), Error> {
        if let Some(node) = self.cache.get(object_id) {
            if !node.mark_to_be_purged() {
                return Ok(());
            }
        }
        // If this fails, the graveyard should clean it up on next mount.
        self.store
            .tombstone_object(
                object_id,
                Options { borrow_metadata_space: true, ..Default::default() },
            )
            .await?;
        Ok(())
    }

    /// Starts the background work task.  This task will periodically:
    ///   - scan all files and flush them to disk, and
    ///   - purge unused cached data.
    /// The task will hold a strong reference to the FxVolume while it is running, so the task must
    /// be closed later with Self::terminate, or the FxVolume will never be dropped.
    pub fn start_background_task(
        self: &Arc<Self>,
        config: MemoryPressureConfig,
        mem_monitor: Option<&MemoryPressureMonitor>,
    ) {
        let mut background_task = self.background_task.lock();
        if background_task.is_none() {
            let (tx, rx) = oneshot::channel();

            let task = if let Some(mem_monitor) = mem_monitor {
                fasync::Task::spawn(self.clone().background_task(
                    config,
                    mem_monitor.get_level_stream(),
                    rx,
                ))
            } else {
                // With no memory pressure monitoring, just stub the stream out as always pending.
                fasync::Task::spawn(self.clone().background_task(config, stream::pending(), rx))
            };

            *background_task = Some((task, tx));
        }
    }

    #[trace]
    async fn background_task(
        self: Arc<Self>,
        config: MemoryPressureConfig,
        mut level_stream: impl Stream<Item = MemoryPressureLevel> + FusedStream + Unpin,
        terminate: oneshot::Receiver<()>,
    ) {
        debug!(store_id = self.store.store_object_id(); "FxVolume::background_task start");
        let mut terminate = terminate.fuse();
        // Default to the normal period until updates come from the `level_stream`.
        let mut level = MemoryPressureLevel::Normal;
        let mut timer =
            pin!(fasync::Timer::new(config.for_level(&level).background_task_initial_delay));

        loop {
            let mut should_terminate = false;
            let mut should_flush = false;
            let mut should_purge_layer_files = false;
            let mut should_update_cache_limit = false;

            futures::select_biased! {
                _ = terminate => should_terminate = true,
                new_level = level_stream.next() => {
                    // Because `level_stream` will never terminate, this is safe to unwrap.
                    let new_level = new_level.unwrap();
                    // At critical levels, it's okay to undertake expensive work immediately
                    // to reclaim memory.
                    should_flush = matches!(new_level, MemoryPressureLevel::Critical);
                    should_purge_layer_files = true;
                    if new_level != level {
                        level = new_level;
                        should_update_cache_limit = true;
                        let level_config = config.for_level(&level);
                        self.read_ahead_size.store(level_config.read_ahead_size, Ordering::Relaxed);
                        timer.as_mut().reset(fasync::MonotonicInstant::after(
                            level_config.background_task_period.into())
                        );
                        debug!(
                            "Background task period changed to {:?} due to new memory pressure \
                            level ({:?}).",
                            config.for_level(&level).background_task_period, level
                        );
                    }
                }
                _ = timer => {
                    timer.as_mut().reset(fasync::MonotonicInstant::after(
                        config.for_level(&level).background_task_period.into())
                    );
                    should_flush = true;
                    // Only purge layer file caches once we have elevated memory pressure.
                    should_purge_layer_files = !matches!(level, MemoryPressureLevel::Normal);

                    #[cfg(feature = "refault-tracking")]
                    self.refault_tracker.output_stats();
                }
            };
            if should_terminate {
                break;
            }

            if should_flush {
                self.flush_all_files(false).await;
                self.dirent_cache.recycle_stale_files();
            }
            if should_purge_layer_files {
                for layer in self.store.tree().immutable_layer_set().layers {
                    layer.purge_cached_data();
                }
            }
            if should_update_cache_limit {
                self.dirent_cache.set_limit(config.for_level(&level).cache_size_limit);
            }
        }
        debug!(store_id = self.store.store_object_id(); "FxVolume::background_task end");
    }

    /// Reports that a certain number of bytes will be dirtied in a pager-backed VMO.
    ///
    /// Note that this function may await flush tasks.
    pub fn report_pager_dirty(
        self: Arc<Self>,
        byte_count: u64,
        mark_dirty: impl FnOnce() + Send + 'static,
    ) {
        if let Some(parent) = self.parent.upgrade() {
            parent.report_pager_dirty(byte_count, self, mark_dirty);
        } else {
            mark_dirty();
        }
    }

    /// Reports that a certain number of bytes were cleaned in a pager-backed VMO.
    pub fn report_pager_clean(&self, byte_count: u64) {
        if let Some(parent) = self.parent.upgrade() {
            parent.report_pager_clean(byte_count);
        }
    }

    #[trace]
    pub async fn flush_all_files(&self, last_chance: bool) {
        let mut flushed = 0;
        for file in self.cache.files() {
            if let Some(node) = file.into_opened_node() {
                if let Err(e) = FxFile::flush(&node, last_chance).await {
                    warn!(
                        store_id = self.store.store_object_id(),
                        oid = node.object_id(),
                        error:? = e;
                        "Failed to flush",
                    )
                }
                if last_chance {
                    let file = node.clone();
                    std::mem::drop(node);
                    file.force_clean();
                }
            }
            flushed += 1;
        }
        debug!(store_id = self.store.store_object_id(), file_count = flushed; "FxVolume flushed");
    }

    /// Spawns a short term task for the volume that includes a guard that will prevent termination.
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        if let Some(guard) = self.scope.try_active_guard() {
            self.executor.spawn_detached(FutureWithGuard::new(guard, task));
        }
    }

    /// Returns the current read-ahead size that should be used based on the current memory-pressure
    /// level.
    pub fn read_ahead_size(&self) -> u64 {
        self.read_ahead_size.load(Ordering::Relaxed)
    }

    /// Tries to unwrap this volume.  If it fails, it will poison the volume so that when it is
    /// dropped, you get a backtrace.
    #[cfg(any(test, feature = "testing"))]
    pub fn try_unwrap(self: Arc<Self>) -> Option<FxVolume> {
        self.poisoned.store(true, Ordering::Relaxed);
        match Arc::try_unwrap(self) {
            Ok(volume) => {
                volume.poisoned.store(false, Ordering::Relaxed);
                Some(volume)
            }
            Err(this) => {
                // Log details about all the places where there might be a reference cycle.
                info!(
                    "background_task: {}, profile_state: {}, dirent_cache count: {}, \
                     pager strong file refs={}, no tasks={}",
                    this.background_task.lock().is_some(),
                    this.profile_state.lock().is_some(),
                    this.dirent_cache.len(),
                    crate::pager::STRONG_FILE_REFS.load(Ordering::Relaxed),
                    {
                        let mut no_tasks = pin!(this.scope.wait());
                        no_tasks
                            .poll_unpin(&mut std::task::Context::from_waker(
                                &futures::task::noop_waker(),
                            ))
                            .is_ready()
                    },
                );
                None
            }
        }
    }

    #[cfg(any(test, feature = "refault-tracking"))]
    pub fn refault_tracker(&self) -> &RefaultTracker {
        &self.refault_tracker
    }
}

#[cfg(any(test, feature = "testing"))]
impl Drop for FxVolume {
    fn drop(&mut self) {
        assert!(!*self.poisoned.get_mut());
    }
}

impl HandleOwner for FxVolume {}

impl AsRef<ObjectStore> for FxVolume {
    fn as_ref(&self) -> &ObjectStore {
        &self.store
    }
}

#[async_trait]
impl FsInspectVolume for FxVolume {
    async fn get_volume_data(&self) -> VolumeData {
        let object_count = self.store().object_count();
        let (used_bytes, bytes_limit) =
            self.store.filesystem().allocator().owner_allocation_info(self.store.store_object_id());
        let encrypted = self.store().crypt().is_some();
        let port_koid =
            fasync::EHandle::local().port().as_handle_ref().get_koid().unwrap().raw_koid();
        VolumeData { bytes_limit, used_bytes, used_nodes: object_count, encrypted, port_koid }
    }
}

pub trait RootDir: FxNode + DirectoryEntry {
    fn as_directory_entry(self: Arc<Self>) -> Arc<dyn DirectoryEntry>;

    fn serve(self: Arc<Self>, flags: fio::Flags, server_end: ServerEnd<fio::DirectoryMarker>);

    fn as_node(self: Arc<Self>) -> Arc<dyn FxNode>;

    fn register_additional_volume_services(
        self: Arc<Self>,
        _svc_dir: &Simple,
    ) -> Result<(), Error> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct FxVolumeAndRoot {
    volume: Arc<FxVolume>,
    root: Arc<dyn RootDir>,
}

impl FxVolumeAndRoot {
    pub async fn new<T: From<Directory<FxVolume>> + RootDir>(
        parent: Weak<VolumesDirectory>,
        store: Arc<ObjectStore>,
        unique_id: u64,
    ) -> Result<Self, Error> {
        let volume = Arc::new(FxVolume::new(parent, store, unique_id)?);
        let root_object_id = volume.store().root_directory_object_id();
        let root_dir = Directory::open(&volume, root_object_id).await?;
        let root = Arc::<T>::new(root_dir.into()) as Arc<dyn RootDir>;
        volume
            .cache
            .get_or_reserve(root_object_id)
            .await
            .placeholder()
            .unwrap()
            .commit(&root.clone().as_node());
        Ok(Self { volume, root })
    }

    pub fn volume(&self) -> &Arc<FxVolume> {
        &self.volume
    }

    pub fn root(&self) -> &Arc<dyn RootDir> {
        &self.root
    }

    // The same as root but downcasted to FxDirectory.
    pub fn root_dir(&self) -> Arc<FxDirectory> {
        self.root().clone().into_any().downcast::<FxDirectory>().expect("Invalid type for root")
    }

    pub async fn handle_project_id_requests(
        &self,
        mut requests: ProjectIdRequestStream,
    ) -> Result<(), Error> {
        let store_id = self.volume.store.store_object_id();
        while let Some(request) = requests.try_next().await? {
            match request {
                ProjectIdRequest::SetLimit { responder, project_id, bytes, nodes } => responder
                    .send(
                    self.volume.store().set_project_limit(project_id, bytes, nodes).await.map_err(
                        |error| {
                            error!(error:?, store_id, project_id; "Failed to set project limit");
                            map_to_raw_status(error)
                        },
                    ),
                )?,
                ProjectIdRequest::Clear { responder, project_id } => {
                    responder
                        .send(self.volume.store().clear_project_limit(project_id).await.map_err(
                        |error| {
                            error!(error:?, store_id, project_id; "Failed to clear project limit");
                            map_to_raw_status(error)
                        },
                    ))?
                }
                ProjectIdRequest::SetForNode { responder, node_id, project_id } => responder.send(
                    self.volume.store().set_project_for_node(node_id, project_id).await.map_err(
                        |error| {
                            error!(error:?, store_id, node_id, project_id; "Failed to apply node.");
                            map_to_raw_status(error)
                        },
                    ),
                )?,
                ProjectIdRequest::GetForNode { responder, node_id } => responder.send(
                    self.volume.store().get_project_for_node(node_id).await.map_err(|error| {
                        error!(error:?, store_id, node_id; "Failed to get node.");
                        map_to_raw_status(error)
                    }),
                )?,
                ProjectIdRequest::ClearForNode { responder, node_id } => responder.send(
                    self.volume.store().clear_project_for_node(node_id).await.map_err(|error| {
                        error!(error:?, store_id, node_id; "Failed to clear for node.");
                        map_to_raw_status(error)
                    }),
                )?,
                ProjectIdRequest::List { responder, token } => {
                    responder.send(match self.list_projects(&token).await {
                        Ok((ref entries, ref next_token)) => Ok((entries, next_token.as_ref())),
                        Err(error) => {
                            error!(error:?, store_id, token:?; "Failed to list projects.");
                            Err(map_to_raw_status(error))
                        }
                    })?
                }
                ProjectIdRequest::Info { responder, project_id } => {
                    responder.send(match self.project_info(project_id).await {
                        Ok((ref limit, ref usage)) => Ok((limit, usage)),
                        Err(error) => {
                            error!(error:?, store_id, project_id; "Failed to get project info.");
                            Err(map_to_raw_status(error))
                        }
                    })?
                }
            }
        }
        Ok(())
    }

    pub async fn handle_file_backed_volume_provider_requests(
        &self,
        mut requests: FileBackedVolumeProviderRequestStream,
    ) -> Result<(), Error> {
        while let Some(request) = requests.try_next().await? {
            match request {
                FileBackedVolumeProviderRequest::Open {
                    parent_directory_token,
                    name,
                    server_end,
                    control_handle: _,
                } => match self
                    .volume
                    .scope
                    .token_registry()
                    // NB: For now, we only expect these calls in a regular (non-blob) volume.
                    // Hard-code the type for simplicity; attempts to call on a blob volume will
                    // get an error.
                    .get_owner(parent_directory_token)
                    .and_then(|dir| {
                        dir.ok_or(zx::Status::BAD_HANDLE).and_then(|dir| {
                            dir.into_any()
                                .downcast::<FxDirectory>()
                                .map_err(|_| zx::Status::BAD_HANDLE)
                        })
                    }) {
                    Ok(dir) => {
                        dir.open_block_file(&name, server_end).await;
                    }
                    Err(status) => {
                        let _ = server_end.close_with_epitaph(status)?;
                    }
                },
            }
        }
        Ok(())
    }

    pub fn into_volume(self) -> Arc<FxVolume> {
        self.volume
    }

    // Maximum entries to fit based on 64KiB message size minus 16 bytes of header, 16 bytes
    // of vector header, 16 bytes for the optional token header, and 8 bytes of token value.
    // https://fuchsia.dev/fuchsia-src/development/languages/fidl/guides/max-out-pagination
    const MAX_PROJECT_ENTRIES: usize = 8184;

    // Calls out to the inner volume to list available projects, removing and re-adding the fidl
    // wrapper types for the pagination token.
    async fn list_projects(
        &self,
        last_token: &Option<Box<ProjectIterToken>>,
    ) -> Result<(Vec<u64>, Option<ProjectIterToken>), Error> {
        let (entries, token) = self
            .volume
            .store()
            .list_projects(
                match last_token {
                    None => 0,
                    Some(v) => v.value,
                },
                Self::MAX_PROJECT_ENTRIES,
            )
            .await?;
        Ok((entries, token.map(|value| ProjectIterToken { value })))
    }

    async fn project_info(&self, project_id: u64) -> Result<(BytesAndNodes, BytesAndNodes), Error> {
        let (limit, usage) = self.volume.store().project_info(project_id).await?;
        // At least one of them needs to be around to return anything.
        ensure!(limit.is_some() || usage.is_some(), FxfsError::NotFound);
        Ok((
            limit.map_or_else(
                || BytesAndNodes { bytes: u64::MAX, nodes: u64::MAX },
                |v| BytesAndNodes { bytes: v.0, nodes: v.1 },
            ),
            usage.map_or_else(
                || BytesAndNodes { bytes: 0, nodes: 0 },
                |v| BytesAndNodes { bytes: v.0, nodes: v.1 },
            ),
        ))
    }
}

// The correct number here is arguably u64::MAX - 1 (because node 0 is reserved). There's a bug
// where inspect test cases fail if we try and use that, possibly because of a signed/unsigned bug.
// See https://fxbug.dev/42168242.  Until that's fixed, we'll have to use i64::MAX.
const TOTAL_NODES: u64 = i64::MAX as u64;

// An array used to initialize the FilesystemInfo |name| field. This just spells "fxfs" 0-padded to
// 32 bytes.
const FXFS_INFO_NAME_FIDL: [i8; 32] = [
    0x66, 0x78, 0x66, 0x73, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0, 0, 0,
];

pub fn info_to_filesystem_info(
    info: filesystem::Info,
    block_size: u64,
    object_count: u64,
    fs_id: u64,
) -> fio::FilesystemInfo {
    fio::FilesystemInfo {
        total_bytes: info.total_bytes,
        used_bytes: info.used_bytes,
        total_nodes: TOTAL_NODES,
        used_nodes: object_count,
        // TODO(https://fxbug.dev/42175592): Support free_shared_pool_bytes.
        free_shared_pool_bytes: 0,
        fs_id,
        block_size: block_size as u32,
        max_filename_size: fio::MAX_NAME_LENGTH as u32,
        fs_type: fidl_fuchsia_fs::VfsType::Fxfs.into_primitive(),
        padding: 0,
        name: FXFS_INFO_NAME_FIDL,
    }
}

#[cfg(any(test, feature = "refault-tracking"))]
#[derive(Default)]
pub struct RefaultTracker(Mutex<RefaultTrackerInner>);

#[cfg(any(test, feature = "refault-tracking"))]
#[derive(Default)]
pub struct RefaultTrackerInner {
    count: u64,
    bytes: u64,
    histogram: [u64; 8],
}

#[cfg(any(test, feature = "refault-tracking"))]
impl RefaultTracker {
    pub fn record_refault(&self, bytes: u64, chunk_refault_count: u8) {
        let mut this = self.0.lock();
        this.count += 1;
        this.bytes += bytes;
        if chunk_refault_count == 1 {
            this.histogram[0] = this.histogram[0].wrapping_add(1);
        } else {
            let old_bucket = (chunk_refault_count - 1).ilog2();
            let new_bucket = (chunk_refault_count).ilog2();
            if old_bucket != new_bucket {
                this.histogram[old_bucket as usize] =
                    this.histogram[old_bucket as usize].wrapping_sub(1);
                this.histogram[new_bucket as usize] =
                    this.histogram[new_bucket as usize].wrapping_add(1);
            }
        }
    }

    #[cfg(feature = "refault-tracking")]
    fn output_stats(&self) {
        let this = self.0.lock();
        if this.count > 0 {
            info!(
                "blob refault stats: count={:?} bytes={:?} hist={:?}",
                this.count, this.bytes, this.histogram,
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn count(&self) -> u64 {
        self.0.lock().count
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> u64 {
        self.0.lock().bytes
    }
}

#[cfg(test)]
mod tests {
    use super::{RefaultTracker, DIRENT_CACHE_LIMIT};
    use crate::fuchsia::directory::FxDirectory;
    use crate::fuchsia::file::FxFile;
    use crate::fuchsia::fxblob::testing::{self as blob_testing, BlobFixture};
    use crate::fuchsia::fxblob::BlobDirectory;
    use crate::fuchsia::memory_pressure::{MemoryPressureLevel, MemoryPressureMonitor};
    use crate::fuchsia::pager::PagerBacked;
    use crate::fuchsia::profile::{new_profile_state, RECORDED};
    use crate::fuchsia::testing::{
        close_dir_checked, close_file_checked, open_dir, open_dir_checked, open_file,
        open_file_checked, write_at, TestFixture,
    };
    use crate::fuchsia::volume::{
        FxVolume, FxVolumeAndRoot, MemoryPressureConfig, MemoryPressureLevelConfig,
        BASE_READ_AHEAD_SIZE,
    };
    use crate::fuchsia::volumes_directory::VolumesDirectory;
    use crate::volume::MAX_READ_AHEAD_SIZE;
    use delivery_blob::CompressionMode;
    use fidl_fuchsia_fs_startup::VolumeMarker;
    use fidl_fuchsia_fxfs::{BytesAndNodes, ProjectIdMarker};
    use fuchsia_component_client::connect_to_protocol_at_dir_svc;
    use fuchsia_fs::file;
    use fxfs::filesystem::{FxFilesystem, FxFilesystemBuilder};
    use fxfs::fsck::{fsck, fsck_volume};
    use fxfs::object_handle::ObjectHandle;
    use fxfs::object_store::directory::replace_child;
    use fxfs::object_store::transaction::{lock_keys, LockKey, Options};
    use fxfs::object_store::volume::root_volume;
    use fxfs::object_store::{HandleOptions, ObjectDescriptor, ObjectStore, NO_OWNER};
    use fxfs_insecure_crypto::InsecureCrypt;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Weak};
    use std::time::Duration;
    use storage_device::fake_device::FakeDevice;
    use storage_device::DeviceHolder;
    use vfs::directory::entry_container::Directory;
    use vfs::execution_scope::ExecutionScope;
    use vfs::path::Path;
    use zx::Status;
    use {fidl_fuchsia_io as fio, fuchsia_async as fasync};

    #[fuchsia::test(threads = 10)]
    async fn test_rename_different_dirs() {
        use zx::Event;

        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let src = open_dir_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;

        let dst = open_dir_checked(
            &root,
            "bar",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;

        let f = open_file_checked(
            &root,
            "foo/a",
            fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        close_file_checked(f).await;

        let (status, dst_token) = dst.get_token().await.expect("FIDL call failed");
        Status::ok(status).expect("get_token failed");
        src.rename("a", Event::from(dst_token.unwrap()), "b")
            .await
            .expect("FIDL call failed")
            .expect("rename failed");

        assert_eq!(
            open_file(&root, "foo/a", fio::Flags::PROTOCOL_FILE, &Default::default())
                .await
                .expect_err("Open succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::NOT_FOUND,
        );
        let f =
            open_file_checked(&root, "bar/b", fio::Flags::PROTOCOL_FILE, &Default::default()).await;
        close_file_checked(f).await;

        close_dir_checked(dst).await;
        close_dir_checked(src).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_rename_same_dir() {
        use zx::Event;
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let src = open_dir_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;

        let f = open_file_checked(
            &root,
            "foo/a",
            fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        close_file_checked(f).await;

        let (status, src_token) = src.get_token().await.expect("FIDL call failed");
        Status::ok(status).expect("get_token failed");
        src.rename("a", Event::from(src_token.unwrap()), "b")
            .await
            .expect("FIDL call failed")
            .expect("rename failed");

        assert_eq!(
            open_file(&root, "foo/a", fio::Flags::PROTOCOL_FILE, &Default::default())
                .await
                .expect_err("Open succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::NOT_FOUND,
        );
        let f =
            open_file_checked(&root, "foo/b", fio::Flags::PROTOCOL_FILE, &Default::default()).await;
        close_file_checked(f).await;

        close_dir_checked(src).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_rename_overwrites_file() {
        use zx::Event;
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let src = open_dir_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;

        let dst = open_dir_checked(
            &root,
            "bar",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;

        // The src file is non-empty.
        let src_file = open_file_checked(
            &root,
            "foo/a",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        let buf = vec![0xaa as u8; 8192];
        file::write(&src_file, buf.as_slice()).await.expect("Failed to write to file");
        close_file_checked(src_file).await;

        // The dst file is empty (so we can distinguish it).
        let f = open_file_checked(
            &root,
            "bar/b",
            fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        close_file_checked(f).await;

        let (status, dst_token) = dst.get_token().await.expect("FIDL call failed");
        Status::ok(status).expect("get_token failed");
        src.rename("a", Event::from(dst_token.unwrap()), "b")
            .await
            .expect("FIDL call failed")
            .expect("rename failed");

        assert_eq!(
            open_file(&root, "foo/a", fio::Flags::PROTOCOL_FILE, &Default::default())
                .await
                .expect_err("Open succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::NOT_FOUND,
        );
        let file = open_file_checked(
            &root,
            "bar/b",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        let buf = file::read(&file).await.expect("read file failed");
        assert_eq!(buf, vec![0xaa as u8; 8192]);
        close_file_checked(file).await;

        close_dir_checked(dst).await;
        close_dir_checked(src).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_rename_overwrites_dir() {
        use zx::Event;
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let src = open_dir_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;

        let dst = open_dir_checked(
            &root,
            "bar",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;

        // The src dir is non-empty.
        open_dir_checked(
            &root,
            "foo/a",
            fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_WRITABLE | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;
        open_file_checked(
            &root,
            "foo/a/file",
            fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        open_dir_checked(
            &root,
            "bar/b",
            fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_DIRECTORY,
            Default::default(),
        )
        .await;

        let (status, dst_token) = dst.get_token().await.expect("FIDL call failed");
        Status::ok(status).expect("get_token failed");
        src.rename("a", Event::from(dst_token.unwrap()), "b")
            .await
            .expect("FIDL call failed")
            .expect("rename failed");

        assert_eq!(
            open_dir(&root, "foo/a", fio::Flags::PROTOCOL_DIRECTORY, &Default::default())
                .await
                .expect_err("Open succeeded")
                .root_cause()
                .downcast_ref::<Status>()
                .expect("No status"),
            &Status::NOT_FOUND,
        );
        let f =
            open_file_checked(&root, "bar/b/file", fio::Flags::PROTOCOL_FILE, &Default::default())
                .await;
        close_file_checked(f).await;

        close_dir_checked(dst).await;
        close_dir_checked(src).await;

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_background_flush() {
        // We have to do a bit of set-up ourselves for this test, since we want to be able to access
        // the underlying DataObjectHandle at the same time as the FxFile which corresponds to it.
        let device = DeviceHolder::new(FakeDevice::new(8192, 512));
        let filesystem = FxFilesystem::new_empty(device).await.unwrap();
        {
            let root_volume = root_volume(filesystem.clone()).await.unwrap();
            let volume = root_volume
                .new_volume("vol", NO_OWNER, Some(Arc::new(InsecureCrypt::new())))
                .await
                .unwrap();
            let mut transaction = filesystem
                .clone()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let object_id = ObjectStore::create_object(
                &volume,
                &mut transaction,
                HandleOptions::default(),
                None,
            )
            .await
            .expect("create_object failed")
            .object_id();
            transaction.commit().await.expect("commit failed");
            let vol =
                FxVolumeAndRoot::new::<FxDirectory>(Weak::new(), volume.clone(), 0).await.unwrap();

            let file = vol
                .volume()
                .get_or_load_node(object_id, ObjectDescriptor::File, None)
                .await
                .expect("get_or_load_node failed")
                .into_any()
                .downcast::<FxFile>()
                .expect("Not a file");

            // Write some data to the file, which will only go to the cache for now.
            write_at(&file, 0, &[123u8]).await.expect("write_at failed");

            let data_has_persisted = || async {
                // We have to reopen the object each time since this is a distinct handle from the
                // one managed by the FxFile.
                let object =
                    ObjectStore::open_object(&volume, object_id, HandleOptions::default(), None)
                        .await
                        .expect("open_object failed");
                let data = object.contents(8192).await.expect("read failed");
                data.len() == 1 && data[..] == [123u8]
            };
            assert!(!data_has_persisted().await);

            vol.volume().start_background_task(
                MemoryPressureConfig {
                    mem_normal: MemoryPressureLevelConfig {
                        background_task_period: Duration::from_millis(100),
                        background_task_initial_delay: Duration::from_millis(100),
                        ..Default::default()
                    },
                    mem_warning: Default::default(),
                    mem_critical: Default::default(),
                },
                None,
            );

            let mut wait = 100;
            loop {
                if data_has_persisted().await {
                    break;
                }
                fasync::Timer::new(Duration::from_millis(wait)).await;
                wait *= 2;
            }

            std::mem::drop(file);
            vol.volume().terminate().await;
        }

        filesystem.close().await.expect("close filesystem failed");
        let device = filesystem.take_device().await;
        device.ensure_unique();
    }

    #[fuchsia::test(threads = 2)]
    async fn test_background_flush_with_warning_memory_pressure() {
        // We have to do a bit of set-up ourselves for this test, since we want to be able to access
        // the underlying DataObjectHandle at the same time as the FxFile which corresponds to it.
        let device = DeviceHolder::new(FakeDevice::new(8192, 512));
        let filesystem = FxFilesystem::new_empty(device).await.unwrap();
        {
            let root_volume = root_volume(filesystem.clone()).await.unwrap();
            let volume = root_volume
                .new_volume("vol", NO_OWNER, Some(Arc::new(InsecureCrypt::new())))
                .await
                .unwrap();
            let mut transaction = filesystem
                .clone()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let object_id = ObjectStore::create_object(
                &volume,
                &mut transaction,
                HandleOptions::default(),
                None,
            )
            .await
            .expect("create_object failed")
            .object_id();
            transaction.commit().await.expect("commit failed");
            let vol =
                FxVolumeAndRoot::new::<FxDirectory>(Weak::new(), volume.clone(), 0).await.unwrap();

            let file = vol
                .volume()
                .get_or_load_node(object_id, ObjectDescriptor::File, None)
                .await
                .expect("get_or_load_node failed")
                .into_any()
                .downcast::<FxFile>()
                .expect("Not a file");

            // Write some data to the file, which will only go to the cache for now.
            write_at(&file, 0, &[123u8]).await.expect("write_at failed");

            // Initialized to the default size.
            assert_eq!(vol.volume().dirent_cache().limit(), DIRENT_CACHE_LIMIT);

            let data_has_persisted = || async {
                // We have to reopen the object each time since this is a distinct handle from the
                // one managed by the FxFile.
                let object =
                    ObjectStore::open_object(&volume, object_id, HandleOptions::default(), None)
                        .await
                        .expect("open_object failed");
                let data = object.contents(8192).await.expect("read failed");
                data.len() == 1 && data[..] == [123u8]
            };
            assert!(!data_has_persisted().await);

            let (watcher_proxy, watcher_server) = fidl::endpoints::create_proxy();
            let mem_pressure = MemoryPressureMonitor::try_from(watcher_server)
                .expect("Failed to create MemoryPressureMonitor");

            // Configure the flush task to only flush quickly on warning.
            let flush_config = MemoryPressureConfig {
                mem_normal: MemoryPressureLevelConfig {
                    background_task_period: Duration::from_secs(20),
                    cache_size_limit: DIRENT_CACHE_LIMIT,
                    ..Default::default()
                },
                mem_warning: MemoryPressureLevelConfig {
                    background_task_period: Duration::from_millis(100),
                    cache_size_limit: 100,
                    background_task_initial_delay: Duration::from_millis(100),
                    ..Default::default()
                },
                mem_critical: MemoryPressureLevelConfig {
                    background_task_period: Duration::from_secs(20),
                    cache_size_limit: 50,
                    ..Default::default()
                },
            };
            vol.volume().start_background_task(flush_config, Some(&mem_pressure));

            // Send the memory pressure update.
            let _ = watcher_proxy
                .on_level_changed(MemoryPressureLevel::Warning)
                .await
                .expect("Failed to send memory pressure level change");

            // Wait a bit of time for the flush to occur (but less than the normal and critical
            // periods).
            const MAX_WAIT: Duration = Duration::from_secs(3);
            let wait_increments = Duration::from_millis(400);
            let mut total_waited = Duration::ZERO;

            while total_waited < MAX_WAIT {
                fasync::Timer::new(wait_increments).await;
                total_waited += wait_increments;

                if data_has_persisted().await {
                    break;
                }
            }

            assert!(data_has_persisted().await);
            assert_eq!(vol.volume().dirent_cache().limit(), 100);

            std::mem::drop(file);
            vol.volume().terminate().await;
        }

        filesystem.close().await.expect("close filesystem failed");
        let device = filesystem.take_device().await;
        device.ensure_unique();
    }

    #[fuchsia::test(threads = 2)]
    async fn test_background_flush_with_critical_memory_pressure() {
        // We have to do a bit of set-up ourselves for this test, since we want to be able to access
        // the underlying DataObjectHandle at the same time as the FxFile which corresponds to it.
        let device = DeviceHolder::new(FakeDevice::new(8192, 512));
        let filesystem = FxFilesystem::new_empty(device).await.unwrap();
        {
            let root_volume = root_volume(filesystem.clone()).await.unwrap();
            let volume = root_volume
                .new_volume("vol", NO_OWNER, Some(Arc::new(InsecureCrypt::new())))
                .await
                .unwrap();
            let mut transaction = filesystem
                .clone()
                .new_transaction(lock_keys![], Options::default())
                .await
                .expect("new_transaction failed");
            let object_id = ObjectStore::create_object(
                &volume,
                &mut transaction,
                HandleOptions::default(),
                None,
            )
            .await
            .expect("create_object failed")
            .object_id();
            transaction.commit().await.expect("commit failed");
            let vol =
                FxVolumeAndRoot::new::<FxDirectory>(Weak::new(), volume.clone(), 0).await.unwrap();

            let file = vol
                .volume()
                .get_or_load_node(object_id, ObjectDescriptor::File, None)
                .await
                .expect("get_or_load_node failed")
                .into_any()
                .downcast::<FxFile>()
                .expect("Not a file");

            // Initialized to the default size.
            assert_eq!(vol.volume().dirent_cache().limit(), DIRENT_CACHE_LIMIT);

            // Write some data to the file, which will only go to the cache for now.
            write_at(&file, 0, &[123u8]).await.expect("write_at failed");

            let data_has_persisted = || async {
                // We have to reopen the object each time since this is a distinct handle from the
                // one managed by the FxFile.
                let object =
                    ObjectStore::open_object(&volume, object_id, HandleOptions::default(), None)
                        .await
                        .expect("open_object failed");
                let data = object.contents(8192).await.expect("read failed");
                data.len() == 1 && data[..] == [123u8]
            };
            assert!(!data_has_persisted().await);

            let (watcher_proxy, watcher_server) = fidl::endpoints::create_proxy();
            let mem_pressure = MemoryPressureMonitor::try_from(watcher_server)
                .expect("Failed to create MemoryPressureMonitor");

            let flush_config = MemoryPressureConfig {
                mem_normal: MemoryPressureLevelConfig {
                    cache_size_limit: DIRENT_CACHE_LIMIT,
                    ..Default::default()
                },
                mem_warning: MemoryPressureLevelConfig {
                    cache_size_limit: 100,
                    ..Default::default()
                },
                mem_critical: MemoryPressureLevelConfig {
                    cache_size_limit: 50,
                    ..Default::default()
                },
            };
            vol.volume().start_background_task(flush_config, Some(&mem_pressure));

            // Send the memory pressure update.
            watcher_proxy
                .on_level_changed(MemoryPressureLevel::Critical)
                .await
                .expect("Failed to send memory pressure level change");

            // Critical memory should trigger a flush immediately so expect a flush very quickly.
            const MAX_WAIT: Duration = Duration::from_secs(2);
            let wait_increments = Duration::from_millis(400);
            let mut total_waited = Duration::ZERO;

            while total_waited < MAX_WAIT {
                fasync::Timer::new(wait_increments).await;
                total_waited += wait_increments;

                if data_has_persisted().await {
                    break;
                }
            }

            assert!(data_has_persisted().await);
            assert_eq!(vol.volume().dirent_cache().limit(), 50);

            std::mem::drop(file);
            vol.volume().terminate().await;
        }

        filesystem.close().await.expect("close filesystem failed");
        let device = filesystem.take_device().await;
        device.ensure_unique();
    }

    #[fuchsia::test(threads = 2)]
    async fn test_memory_pressure_signal_updates_read_ahead_size() {
        let fixture = TestFixture::new().await;
        {
            let (watcher_proxy, watcher_server) = fidl::endpoints::create_proxy();
            let mem_pressure = MemoryPressureMonitor::try_from(watcher_server)
                .expect("Failed to create MemoryPressureMonitor");

            let flush_config = MemoryPressureConfig {
                mem_normal: MemoryPressureLevelConfig {
                    read_ahead_size: 12 * 1024,
                    ..Default::default()
                },
                mem_warning: MemoryPressureLevelConfig {
                    read_ahead_size: 8 * 1024,
                    ..Default::default()
                },
                mem_critical: MemoryPressureLevelConfig {
                    read_ahead_size: 4 * 1024,
                    ..Default::default()
                },
            };
            let volume = fixture.volume().volume().clone();
            volume.start_background_task(flush_config, Some(&mem_pressure));
            let wait_for_read_ahead_to_change =
                async move |level: MemoryPressureLevel, expected: u64| {
                    watcher_proxy
                        .on_level_changed(level)
                        .await
                        .expect("Failed to send memory pressure level change");
                    const MAX_WAIT: Duration = Duration::from_secs(2);
                    let wait_increments = Duration::from_millis(400);
                    let mut total_waited = Duration::ZERO;
                    while total_waited < MAX_WAIT {
                        if volume.read_ahead_size() == expected {
                            break;
                        }
                        fasync::Timer::new(wait_increments).await;
                        total_waited += wait_increments;
                    }
                    assert_eq!(volume.read_ahead_size(), expected);
                };
            wait_for_read_ahead_to_change(MemoryPressureLevel::Critical, 4 * 1024).await;
            wait_for_read_ahead_to_change(MemoryPressureLevel::Warning, 8 * 1024).await;
            wait_for_read_ahead_to_change(MemoryPressureLevel::Normal, 12 * 1024).await;
            wait_for_read_ahead_to_change(MemoryPressureLevel::Critical, 4 * 1024).await;
        }
        fixture.close().await;
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_project_limit_persistence() {
        const BYTES_LIMIT_1: u64 = 123456;
        const NODES_LIMIT_1: u64 = 4321;
        const BYTES_LIMIT_2: u64 = 456789;
        const NODES_LIMIT_2: u64 = 9876;
        const VOLUME_NAME: &str = "A";
        const FILE_NAME: &str = "B";
        const PROJECT_ID: u64 = 42;
        const PROJECT_ID2: u64 = 343;
        let volume_store_id;
        let node_id;
        let mut device = DeviceHolder::new(FakeDevice::new(8192, 512));
        {
            let filesystem = FxFilesystem::new_empty(device).await.unwrap();
            let volumes_directory = VolumesDirectory::new(
                root_volume(filesystem.clone()).await.unwrap(),
                Weak::new(),
                None,
            )
            .await
            .unwrap();

            let volume_and_root = volumes_directory
                .create_and_mount_volume(VOLUME_NAME, None, false)
                .await
                .expect("create unencrypted volume failed");
            volume_store_id = volume_and_root.volume().store().store_object_id();

            // TODO(https://fxbug.dev/378924259): Migrate to open3.
            let (volume_proxy, volume_server_end) = fidl::endpoints::create_proxy::<VolumeMarker>();
            volumes_directory.directory_node().clone().deprecated_open(
                ExecutionScope::new(),
                fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
                Path::validate_and_split(VOLUME_NAME).unwrap(),
                volume_server_end.into_channel().into(),
            );

            let (volume_dir_proxy, dir_server_end) =
                fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
            volumes_directory
                .serve_volume(&volume_and_root, dir_server_end, false)
                .expect("serve_volume failed");

            let project_proxy =
                connect_to_protocol_at_dir_svc::<ProjectIdMarker>(&volume_dir_proxy)
                    .expect("Unable to connect to project id service");

            project_proxy
                .set_limit(0, BYTES_LIMIT_1, NODES_LIMIT_1)
                .await
                .unwrap()
                .expect_err("Should not set limits for project id 0");

            project_proxy
                .set_limit(PROJECT_ID, BYTES_LIMIT_1, NODES_LIMIT_1)
                .await
                .unwrap()
                .expect("To set limits");
            {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").0;
                assert_eq!(bytes, BYTES_LIMIT_1);
                assert_eq!(nodes, NODES_LIMIT_1);
            }

            let file_proxy = {
                let (root_proxy, root_server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
                volume_dir_proxy
                    .open(
                        "root",
                        fio::PERM_READABLE | fio::PERM_WRITABLE | fio::Flags::PROTOCOL_DIRECTORY,
                        &Default::default(),
                        root_server_end.into_channel(),
                    )
                    .expect("Failed to open volume root");

                open_file_checked(
                    &root_proxy,
                    FILE_NAME,
                    fio::Flags::FLAG_MAYBE_CREATE
                        | fio::PERM_READABLE
                        | fio::PERM_WRITABLE
                        | fio::Flags::PROTOCOL_FILE,
                    &Default::default(),
                )
                .await
            };

            let (_, immutable_attributes) =
                file_proxy.get_attributes(fio::NodeAttributesQuery::ID).await.unwrap().unwrap();
            node_id = immutable_attributes.id.unwrap();

            project_proxy
                .set_for_node(node_id, 0)
                .await
                .unwrap()
                .expect_err("Should not set 0 project id");

            project_proxy
                .set_for_node(node_id, PROJECT_ID)
                .await
                .unwrap()
                .expect("Setting project on node");

            project_proxy
                .set_limit(PROJECT_ID, BYTES_LIMIT_2, NODES_LIMIT_2)
                .await
                .unwrap()
                .expect("To set limits");
            {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").0;
                assert_eq!(bytes, BYTES_LIMIT_2);
                assert_eq!(nodes, NODES_LIMIT_2);
            }

            assert_eq!(
                project_proxy.get_for_node(node_id).await.unwrap().expect("Checking project"),
                PROJECT_ID
            );

            std::mem::drop(volume_proxy);
            volumes_directory.terminate().await;
            std::mem::drop(volumes_directory);
            filesystem.close().await.expect("close filesystem failed");
            device = filesystem.take_device().await;
        }
        {
            device.ensure_unique();
            device.reopen(false);
            let filesystem = FxFilesystem::open(device as DeviceHolder).await.unwrap();
            fsck(filesystem.clone()).await.expect("Fsck");
            fsck_volume(filesystem.as_ref(), volume_store_id, None).await.expect("Fsck volume");
            let volumes_directory = VolumesDirectory::new(
                root_volume(filesystem.clone()).await.unwrap(),
                Weak::new(),
                None,
            )
            .await
            .unwrap();
            let volume_and_root = volumes_directory
                .mount_volume(VOLUME_NAME, None, false)
                .await
                .expect("mount unencrypted volume failed");
            // TODO(https://fxbug.dev/378924259): Migrate to open3.
            let (volume_proxy, volume_server_end) = fidl::endpoints::create_proxy::<VolumeMarker>();
            volumes_directory.directory_node().clone().deprecated_open(
                ExecutionScope::new(),
                fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
                Path::validate_and_split(VOLUME_NAME).unwrap(),
                volume_server_end.into_channel().into(),
            );

            let project_proxy = {
                let (volume_dir_proxy, dir_server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
                volumes_directory
                    .serve_volume(&volume_and_root, dir_server_end, false)
                    .expect("serve_volume failed");

                connect_to_protocol_at_dir_svc::<ProjectIdMarker>(&volume_dir_proxy)
                    .expect("Unable to connect to project id service")
            };

            let usage_bytes_and_nodes = {
                let (
                    BytesAndNodes { bytes: limit_bytes, nodes: limit_nodes },
                    usage_bytes_and_nodes,
                ) = project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info");
                assert_eq!(limit_bytes, BYTES_LIMIT_2);
                assert_eq!(limit_nodes, NODES_LIMIT_2);
                usage_bytes_and_nodes
            };

            // Should be unable to clear the project limit, due to being in use.
            project_proxy.clear(PROJECT_ID).await.unwrap().expect("To clear limits");

            assert_eq!(
                project_proxy.get_for_node(node_id).await.unwrap().expect("Checking project"),
                PROJECT_ID
            );
            project_proxy
                .set_for_node(node_id, PROJECT_ID2)
                .await
                .unwrap()
                .expect("Changing project");
            assert_eq!(
                project_proxy.get_for_node(node_id).await.unwrap().expect("Checking project"),
                PROJECT_ID2
            );

            assert_eq!(
                project_proxy.info(PROJECT_ID).await.unwrap().expect_err("Expect missing limits"),
                Status::NOT_FOUND.into_raw()
            );
            assert_eq!(
                project_proxy.info(PROJECT_ID2).await.unwrap().expect("Fetching project info").1,
                usage_bytes_and_nodes
            );

            std::mem::drop(volume_proxy);
            volumes_directory.terminate().await;
            std::mem::drop(volumes_directory);
            filesystem.close().await.expect("close filesystem failed");
            device = filesystem.take_device().await;
        }
        device.ensure_unique();
        device.reopen(false);
        let filesystem = FxFilesystem::open(device as DeviceHolder).await.unwrap();
        fsck(filesystem.clone()).await.expect("Fsck");
        fsck_volume(filesystem.as_ref(), volume_store_id, None).await.expect("Fsck volume");
        let volumes_directory = VolumesDirectory::new(
            root_volume(filesystem.clone()).await.unwrap(),
            Weak::new(),
            None,
        )
        .await
        .unwrap();
        let volume_and_root = volumes_directory
            .mount_volume(VOLUME_NAME, None, false)
            .await
            .expect("mount unencrypted volume failed");
        let (volume_dir_proxy, dir_server_end) =
            fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        volumes_directory
            .serve_volume(&volume_and_root, dir_server_end, false)
            .expect("serve_volume failed");
        let project_proxy = connect_to_protocol_at_dir_svc::<ProjectIdMarker>(&volume_dir_proxy)
            .expect("Unable to connect to project id service");
        assert_eq!(
            project_proxy.info(PROJECT_ID).await.unwrap().expect_err("Expect missing limits"),
            Status::NOT_FOUND.into_raw()
        );
        volumes_directory.terminate().await;
        std::mem::drop(volumes_directory);
        filesystem.close().await.expect("close filesystem failed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_project_limit_accounting() {
        const BYTES_LIMIT: u64 = 123456;
        const NODES_LIMIT: u64 = 4321;
        const VOLUME_NAME: &str = "A";
        const FILE_NAME: &str = "B";
        const PROJECT_ID: u64 = 42;
        let volume_store_id;
        let mut device = DeviceHolder::new(FakeDevice::new(8192, 512));
        let first_object_id;
        let mut bytes_usage;
        {
            let filesystem = FxFilesystem::new_empty(device).await.unwrap();
            let volumes_directory = VolumesDirectory::new(
                root_volume(filesystem.clone()).await.unwrap(),
                Weak::new(),
                None,
            )
            .await
            .unwrap();

            let volume_and_root = volumes_directory
                .create_and_mount_volume(VOLUME_NAME, Some(Arc::new(InsecureCrypt::new())), false)
                .await
                .expect("create unencrypted volume failed");
            volume_store_id = volume_and_root.volume().store().store_object_id();

            // TODO(https://fxbug.dev/378924259): Migrate to open3.
            let (volume_proxy, volume_server_end) = fidl::endpoints::create_proxy::<VolumeMarker>();
            volumes_directory.directory_node().clone().deprecated_open(
                ExecutionScope::new(),
                fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
                Path::validate_and_split(VOLUME_NAME).unwrap(),
                volume_server_end.into_channel().into(),
            );

            let (volume_dir_proxy, dir_server_end) =
                fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
            volumes_directory
                .serve_volume(&volume_and_root, dir_server_end, false)
                .expect("serve_volume failed");

            let project_proxy =
                connect_to_protocol_at_dir_svc::<ProjectIdMarker>(&volume_dir_proxy)
                    .expect("Unable to connect to project id service");

            project_proxy
                .set_limit(PROJECT_ID, BYTES_LIMIT, NODES_LIMIT)
                .await
                .unwrap()
                .expect("To set limits");
            {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").0;
                assert_eq!(bytes, BYTES_LIMIT);
                assert_eq!(nodes, NODES_LIMIT);
            }

            let file_proxy = {
                let (root_proxy, root_server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
                volume_dir_proxy
                    .open(
                        "root",
                        fio::PERM_READABLE | fio::PERM_WRITABLE,
                        &Default::default(),
                        root_server_end.into_channel(),
                    )
                    .expect("Failed to open volume root");

                open_file_checked(
                    &root_proxy,
                    FILE_NAME,
                    fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
                    &Default::default(),
                )
                .await
            };

            assert_eq!(
                8192,
                file_proxy
                    .write(&vec![0xff as u8; 8192])
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::from_raw)
                    .expect("File write was successful")
            );
            file_proxy.sync().await.expect("FIDL call failed").expect("Sync failed.");

            {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                assert_eq!(bytes, 0);
                assert_eq!(nodes, 0);
            }

            let (_, immutable_attributes) =
                file_proxy.get_attributes(fio::NodeAttributesQuery::ID).await.unwrap().unwrap();
            let node_id = immutable_attributes.id.unwrap();

            first_object_id = node_id;
            project_proxy
                .set_for_node(node_id, PROJECT_ID)
                .await
                .unwrap()
                .expect("Setting project on node");

            bytes_usage = {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                assert!(bytes > 0);
                assert_eq!(nodes, 1);
                bytes
            };

            // Grow the file by a block.
            assert_eq!(
                8192,
                file_proxy
                    .write(&vec![0xff as u8; 8192])
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::from_raw)
                    .expect("File write was successful")
            );
            file_proxy.sync().await.expect("FIDL call failed").expect("Sync failed.");
            bytes_usage = {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                assert!(bytes > bytes_usage);
                assert_eq!(nodes, 1);
                bytes
            };

            std::mem::drop(volume_proxy);
            volumes_directory.terminate().await;
            std::mem::drop(volumes_directory);
            filesystem.close().await.expect("close filesystem failed");
            device = filesystem.take_device().await;
        }
        {
            device.ensure_unique();
            device.reopen(false);
            let filesystem = FxFilesystem::open(device as DeviceHolder).await.unwrap();
            fsck(filesystem.clone()).await.expect("Fsck");
            fsck_volume(filesystem.as_ref(), volume_store_id, Some(Arc::new(InsecureCrypt::new())))
                .await
                .expect("Fsck volume");
            let volumes_directory = VolumesDirectory::new(
                root_volume(filesystem.clone()).await.unwrap(),
                Weak::new(),
                None,
            )
            .await
            .unwrap();
            let volume_and_root = volumes_directory
                .mount_volume(VOLUME_NAME, Some(Arc::new(InsecureCrypt::new())), false)
                .await
                .expect("mount unencrypted volume failed");

            // TODO(https://fxbug.dev/378924259): Migrate to open3.
            let (volume_proxy, volume_server_end) = fidl::endpoints::create_proxy::<VolumeMarker>();
            volumes_directory.directory_node().clone().deprecated_open(
                ExecutionScope::new(),
                fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
                Path::validate_and_split(VOLUME_NAME).unwrap(),
                volume_server_end.into_channel().into(),
            );

            let (root_proxy, project_proxy) = {
                let (volume_dir_proxy, dir_server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
                volumes_directory
                    .serve_volume(&volume_and_root, dir_server_end, false)
                    .expect("serve_volume failed");

                let (root_proxy, root_server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
                volume_dir_proxy
                    .open(
                        "root",
                        fio::PERM_READABLE | fio::PERM_WRITABLE,
                        &Default::default(),
                        root_server_end.into_channel(),
                    )
                    .expect("Failed to open volume root");
                let project_proxy = {
                    connect_to_protocol_at_dir_svc::<ProjectIdMarker>(&volume_dir_proxy)
                        .expect("Unable to connect to project id service")
                };
                (root_proxy, project_proxy)
            };

            {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                assert_eq!(bytes, bytes_usage);
                assert_eq!(nodes, 1);
            }

            assert_eq!(
                project_proxy
                    .get_for_node(first_object_id)
                    .await
                    .unwrap()
                    .expect("Checking project"),
                PROJECT_ID
            );
            root_proxy
                .unlink(FILE_NAME, &fio::UnlinkOptions::default())
                .await
                .expect("FIDL call failed")
                .expect("unlink failed");
            filesystem.graveyard().flush().await;

            {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                assert_eq!(bytes, 0);
                assert_eq!(nodes, 0);
            }

            let file_proxy = open_file_checked(
                &root_proxy,
                FILE_NAME,
                fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
                &Default::default(),
            )
            .await;

            let (_, immutable_attributes) =
                file_proxy.get_attributes(fio::NodeAttributesQuery::ID).await.unwrap().unwrap();
            let node_id = immutable_attributes.id.unwrap();

            project_proxy
                .set_for_node(node_id, PROJECT_ID)
                .await
                .unwrap()
                .expect("Applying project");

            bytes_usage = {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                // Empty file should have less space than the non-empty file from above.
                assert!(bytes < bytes_usage);
                assert_eq!(nodes, 1);
                bytes
            };

            assert_eq!(
                8192,
                file_proxy
                    .write(&vec![0xff as u8; 8192])
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::from_raw)
                    .expect("File write was successful")
            );
            file_proxy.sync().await.expect("FIDL call failed").expect("Sync failed.");
            bytes_usage = {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                assert!(bytes > bytes_usage);
                assert_eq!(nodes, 1);
                bytes
            };

            // Trim to zero. Bytes should decrease.
            file_proxy.resize(0).await.expect("FIDL call failed").expect("Resize file");
            file_proxy.sync().await.expect("FIDL call failed").expect("Sync failed.");
            {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                assert!(bytes < bytes_usage);
                assert_eq!(nodes, 1);
            };

            // Dropping node from project. Usage should go to zero.
            project_proxy
                .clear_for_node(node_id)
                .await
                .expect("FIDL call failed")
                .expect("Clear failed.");
            {
                let BytesAndNodes { bytes, nodes } =
                    project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
                assert_eq!(bytes, 0);
                assert_eq!(nodes, 0);
            };

            std::mem::drop(volume_proxy);
            volumes_directory.terminate().await;
            std::mem::drop(volumes_directory);
            filesystem.close().await.expect("close filesystem failed");
            device = filesystem.take_device().await;
        }
        device.ensure_unique();
        device.reopen(false);
        let filesystem = FxFilesystem::open(device as DeviceHolder).await.unwrap();
        fsck(filesystem.clone()).await.expect("Fsck");
        fsck_volume(filesystem.as_ref(), volume_store_id, Some(Arc::new(InsecureCrypt::new())))
            .await
            .expect("Fsck volume");
        filesystem.close().await.expect("close filesystem failed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_project_node_inheritance() {
        const BYTES_LIMIT: u64 = 123456;
        const NODES_LIMIT: u64 = 4321;
        const VOLUME_NAME: &str = "A";
        const DIR_NAME: &str = "B";
        const SUBDIR_NAME: &str = "C";
        const FILE_NAME: &str = "D";
        const PROJECT_ID: u64 = 42;
        let volume_store_id;
        let mut device = DeviceHolder::new(FakeDevice::new(8192, 512));
        {
            let filesystem = FxFilesystem::new_empty(device).await.unwrap();
            let volumes_directory = VolumesDirectory::new(
                root_volume(filesystem.clone()).await.unwrap(),
                Weak::new(),
                None,
            )
            .await
            .unwrap();

            let volume_and_root = volumes_directory
                .create_and_mount_volume(VOLUME_NAME, Some(Arc::new(InsecureCrypt::new())), false)
                .await
                .expect("create unencrypted volume failed");
            volume_store_id = volume_and_root.volume().store().store_object_id();

            // TODO(https://fxbug.dev/378924259): Migrate to open3.
            let (volume_proxy, volume_server_end) = fidl::endpoints::create_proxy::<VolumeMarker>();
            volumes_directory.directory_node().clone().deprecated_open(
                ExecutionScope::new(),
                fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
                Path::validate_and_split(VOLUME_NAME).unwrap(),
                volume_server_end.into_channel().into(),
            );

            let (volume_dir_proxy, dir_server_end) =
                fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
            volumes_directory
                .serve_volume(&volume_and_root, dir_server_end, false)
                .expect("serve_volume failed");

            let project_proxy =
                connect_to_protocol_at_dir_svc::<ProjectIdMarker>(&volume_dir_proxy)
                    .expect("Unable to connect to project id service");

            project_proxy
                .set_limit(PROJECT_ID, BYTES_LIMIT, NODES_LIMIT)
                .await
                .unwrap()
                .expect("To set limits");

            let dir_proxy = {
                let (root_proxy, root_server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
                volume_dir_proxy
                    .open(
                        "root",
                        fio::PERM_READABLE | fio::PERM_WRITABLE,
                        &Default::default(),
                        root_server_end.into_channel(),
                    )
                    .expect("Failed to open volume root");

                open_dir_checked(
                    &root_proxy,
                    DIR_NAME,
                    fio::Flags::FLAG_MAYBE_CREATE
                        | fio::PERM_READABLE
                        | fio::PERM_WRITABLE
                        | fio::Flags::PROTOCOL_DIRECTORY,
                    Default::default(),
                )
                .await
            };
            {
                let (_, immutable_attributes) =
                    dir_proxy.get_attributes(fio::NodeAttributesQuery::ID).await.unwrap().unwrap();
                let node_id = immutable_attributes.id.unwrap();

                project_proxy
                    .set_for_node(node_id, PROJECT_ID)
                    .await
                    .unwrap()
                    .expect("Setting project on node");
            }

            let subdir_proxy = open_dir_checked(
                &dir_proxy,
                SUBDIR_NAME,
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_DIRECTORY,
                Default::default(),
            )
            .await;
            {
                let (_, immutable_attributes) = subdir_proxy
                    .get_attributes(fio::NodeAttributesQuery::ID)
                    .await
                    .unwrap()
                    .unwrap();
                let node_id = immutable_attributes.id.unwrap();

                assert_eq!(
                    project_proxy
                        .get_for_node(node_id)
                        .await
                        .unwrap()
                        .expect("Setting project on node"),
                    PROJECT_ID
                );
            }

            let file_proxy = open_file_checked(
                &subdir_proxy,
                FILE_NAME,
                fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
                &Default::default(),
            )
            .await;
            {
                let (_, immutable_attributes) =
                    file_proxy.get_attributes(fio::NodeAttributesQuery::ID).await.unwrap().unwrap();
                let node_id = immutable_attributes.id.unwrap();

                assert_eq!(
                    project_proxy
                        .get_for_node(node_id)
                        .await
                        .unwrap()
                        .expect("Setting project on node"),
                    PROJECT_ID
                );
            }

            // An unnamed temporary file is created slightly differently to a regular file object.
            // Just in case, check that it inherits project ID as well.
            let tmpfile_proxy = open_file_checked(
                &subdir_proxy,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_READABLE,
                &fio::Options::default(),
            )
            .await;
            {
                let (_, immutable_attributes) = tmpfile_proxy
                    .get_attributes(fio::NodeAttributesQuery::ID)
                    .await
                    .unwrap()
                    .unwrap();
                let node_id: u64 = immutable_attributes.id.unwrap();
                assert_eq!(
                    project_proxy
                        .get_for_node(node_id)
                        .await
                        .unwrap()
                        .expect("Setting project on node"),
                    PROJECT_ID
                );
            }

            let BytesAndNodes { nodes, .. } =
                project_proxy.info(PROJECT_ID).await.unwrap().expect("Fetching project info").1;
            assert_eq!(nodes, 3);
            std::mem::drop(volume_proxy);
            volumes_directory.terminate().await;
            std::mem::drop(volumes_directory);
            filesystem.close().await.expect("close filesystem failed");
            device = filesystem.take_device().await;
        }
        device.ensure_unique();
        device.reopen(false);
        let filesystem = FxFilesystem::open(device as DeviceHolder).await.unwrap();
        fsck(filesystem.clone()).await.expect("Fsck");
        fsck_volume(filesystem.as_ref(), volume_store_id, Some(Arc::new(InsecureCrypt::new())))
            .await
            .expect("Fsck volume");
        filesystem.close().await.expect("close filesystem failed");
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_project_listing() {
        const VOLUME_NAME: &str = "A";
        const FILE_NAME: &str = "B";
        const NON_ZERO_PROJECT_ID: u64 = 3;
        let mut device = DeviceHolder::new(FakeDevice::new(8192, 512));
        let volume_store_id;
        {
            let filesystem = FxFilesystem::new_empty(device).await.unwrap();
            let volumes_directory = VolumesDirectory::new(
                root_volume(filesystem.clone()).await.unwrap(),
                Weak::new(),
                None,
            )
            .await
            .unwrap();
            let volume_and_root = volumes_directory
                .create_and_mount_volume(VOLUME_NAME, None, false)
                .await
                .expect("create unencrypted volume failed");
            volume_store_id = volume_and_root.volume().store().store_object_id();

            // TODO(https://fxbug.dev/378924259): Migrate to open3.
            let (volume_proxy, volume_server_end) = fidl::endpoints::create_proxy::<VolumeMarker>();
            volumes_directory.directory_node().clone().deprecated_open(
                ExecutionScope::new(),
                fio::OpenFlags::RIGHT_READABLE | fio::OpenFlags::RIGHT_WRITABLE,
                Path::validate_and_split(VOLUME_NAME).unwrap(),
                volume_server_end.into_channel().into(),
            );
            let (volume_dir_proxy, dir_server_end) =
                fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
            volumes_directory
                .serve_volume(&volume_and_root, dir_server_end, false)
                .expect("serve_volume failed");
            let project_proxy =
                connect_to_protocol_at_dir_svc::<ProjectIdMarker>(&volume_dir_proxy)
                    .expect("Unable to connect to project id service");
            // This is just to ensure that the small numbers below can be used for this test.
            assert!(FxVolumeAndRoot::MAX_PROJECT_ENTRIES >= 4);
            // Create a bunch of proxies. 3 more than the limit to ensure pagination.
            let num_entries = u64::try_from(FxVolumeAndRoot::MAX_PROJECT_ENTRIES + 3).unwrap();
            for project_id in 1..=num_entries {
                project_proxy.set_limit(project_id, 1, 1).await.unwrap().expect("To set limits");
            }

            // Add one usage entry to be interspersed with the limit entries. Verifies that the
            // iterator will progress passed it with no effect.
            let file_proxy = {
                let (root_proxy, root_server_end) =
                    fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
                volume_dir_proxy
                    .open(
                        "root",
                        fio::PERM_READABLE | fio::PERM_WRITABLE,
                        &Default::default(),
                        root_server_end.into_channel(),
                    )
                    .expect("Failed to open volume root");

                open_file_checked(
                    &root_proxy,
                    FILE_NAME,
                    fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE,
                    &Default::default(),
                )
                .await
            };
            let (_, immutable_attributes) =
                file_proxy.get_attributes(fio::NodeAttributesQuery::ID).await.unwrap().unwrap();
            let node_id = immutable_attributes.id.unwrap();
            project_proxy
                .set_for_node(node_id, NON_ZERO_PROJECT_ID)
                .await
                .unwrap()
                .expect("Setting project on node");
            {
                let BytesAndNodes { nodes, .. } = project_proxy
                    .info(NON_ZERO_PROJECT_ID)
                    .await
                    .unwrap()
                    .expect("Fetching project info")
                    .1;
                assert_eq!(nodes, 1);
            }

            // If this `unwrap()` fails, it is likely the MAX_PROJECT_ENTRIES is too large for fidl.
            let (mut entries, mut next_token) =
                project_proxy.list(None).await.unwrap().expect("To get project listing");
            assert_eq!(entries.len(), FxVolumeAndRoot::MAX_PROJECT_ENTRIES);
            assert!(next_token.is_some());
            assert!(entries.contains(&1));
            assert!(entries.contains(&3));
            assert!(!entries.contains(&num_entries));
            // Page two should have a small set at the end.
            (entries, next_token) = project_proxy
                .list(next_token.as_deref())
                .await
                .unwrap()
                .expect("To get project listing");
            assert_eq!(entries.len(), 3);
            assert!(next_token.is_none());
            assert!(entries.contains(&num_entries));
            assert!(!entries.contains(&1));
            assert!(!entries.contains(&3));
            // Delete a couple and list all again, but one has usage still.
            project_proxy.clear(1).await.unwrap().expect("Clear project");
            project_proxy.clear(3).await.unwrap().expect("Clear project");
            (entries, next_token) =
                project_proxy.list(None).await.unwrap().expect("To get project listing");
            assert_eq!(entries.len(), FxVolumeAndRoot::MAX_PROJECT_ENTRIES);
            assert!(next_token.is_some());
            assert!(!entries.contains(&num_entries));
            assert!(!entries.contains(&1));
            assert!(entries.contains(&3));
            (entries, next_token) = project_proxy
                .list(next_token.as_deref())
                .await
                .unwrap()
                .expect("To get project listing");
            assert_eq!(entries.len(), 2);
            assert!(next_token.is_none());
            assert!(entries.contains(&num_entries));
            // Delete two more to hit the edge case.
            project_proxy.clear(2).await.unwrap().expect("Clear project");
            project_proxy.clear(4).await.unwrap().expect("Clear project");
            (entries, next_token) =
                project_proxy.list(None).await.unwrap().expect("To get project listing");
            assert_eq!(entries.len(), FxVolumeAndRoot::MAX_PROJECT_ENTRIES);
            assert!(next_token.is_none());
            assert!(entries.contains(&num_entries));
            std::mem::drop(volume_proxy);
            volumes_directory.terminate().await;
            std::mem::drop(volumes_directory);
            filesystem.close().await.expect("close filesystem failed");
            device = filesystem.take_device().await;
        }
        device.ensure_unique();
        device.reopen(false);
        let filesystem = FxFilesystem::open(device as DeviceHolder).await.unwrap();
        fsck(filesystem.clone()).await.expect("Fsck");
        fsck_volume(filesystem.as_ref(), volume_store_id, None).await.expect("Fsck volume");
        filesystem.close().await.expect("close filesystem failed");
    }

    #[fuchsia::test(threads = 10)]
    async fn test_profile() {
        let mut hashes = Vec::new();
        let device = {
            let fixture = blob_testing::new_blob_fixture().await;

            for i in 0..3u64 {
                let hash =
                    fixture.write_blob(i.to_le_bytes().as_slice(), CompressionMode::Never).await;
                hashes.push(hash);
            }
            fixture.close().await
        };
        device.ensure_unique();

        device.reopen(false);
        let mut device = {
            let fixture = blob_testing::open_blob_fixture(device).await;
            fixture
                .volume()
                .volume()
                .record_or_replay_profile(new_profile_state(true), "foo")
                .await
                .expect("Recording");

            // Page in the zero offsets only to avoid readahead strangeness.
            let mut writable = [0u8];
            for hash in &hashes {
                let vmo = fixture.get_blob_vmo(*hash).await;
                vmo.read(&mut writable, 0).expect("Vmo read");
            }
            fixture.volume().volume().stop_profile_tasks().await;
            fixture.close().await
        };

        // Do this multiple times to ensure that the re-recording doesn't drop anything.
        for i in 0..3 {
            device.ensure_unique();
            device.reopen(false);
            let fixture = blob_testing::open_blob_fixture(device).await;
            {
                // Need to get the root vmo to check committed bytes.
                let dir = fixture
                    .volume()
                    .root()
                    .clone()
                    .into_any()
                    .downcast::<BlobDirectory>()
                    .expect("Root should be BlobDirectory");

                // Ensure that nothing is paged in right now.
                for hash in &hashes {
                    let blob = dir.lookup_blob(*hash).await.expect("Opening blob");
                    assert_eq!(blob.vmo().info().unwrap().committed_bytes, 0);
                }

                fixture
                    .volume()
                    .volume()
                    .record_or_replay_profile(new_profile_state(true), "foo")
                    .await
                    .expect("Replaying");

                // Move the file in flight to ensure a new version lands to be used next time.
                {
                    let store_id = fixture.volume().volume().store().store_object_id();
                    let dir = fixture.volume().volume().get_profile_directory().await.unwrap();
                    let old_file = dir.lookup("foo").await.unwrap().unwrap().0;
                    let mut transaction = fixture
                        .fs()
                        .clone()
                        .new_transaction(
                            lock_keys!(
                                LockKey::object(store_id, dir.object_id()),
                                LockKey::object(store_id, old_file),
                            ),
                            Options::default(),
                        )
                        .await
                        .unwrap();
                    replace_child(&mut transaction, Some((&dir, "foo")), (&dir, &i.to_string()))
                        .await
                        .expect("Replace old profile.");
                    transaction.commit().await.unwrap();
                    assert!(
                        dir.lookup("foo").await.unwrap().is_none(),
                        "Old profile should be moved"
                    );
                }

                // Await all data being played back by checking that things have paged in.
                for hash in &hashes {
                    // Fetch vmo this way as well to ensure that the open is counting the file as
                    // used in the current recording.
                    let _vmo = fixture.get_blob_vmo(*hash).await;
                    let blob = dir.lookup_blob(*hash).await.expect("Opening blob");
                    while blob.vmo().info().unwrap().committed_bytes == 0 {
                        fasync::Timer::new(Duration::from_millis(25)).await;
                    }
                }

                // Complete the recording.
                fixture.volume().volume().stop_profile_tasks().await;
            }
            device = fixture.close().await;
        }
    }

    #[fuchsia::test(threads = 10)]
    async fn test_profile_update() {
        let mut hashes = Vec::new();
        let device = {
            let fixture = blob_testing::new_blob_fixture().await;
            for i in 0..2u64 {
                let hash =
                    fixture.write_blob(i.to_le_bytes().as_slice(), CompressionMode::Never).await;
                hashes.push(hash);
            }
            fixture.close().await
        };
        device.ensure_unique();

        device.reopen(false);
        let device = {
            let fixture = blob_testing::open_blob_fixture(device).await;

            {
                let volume = fixture.volume().volume();
                volume
                    .record_or_replay_profile(new_profile_state(true), "foo")
                    .await
                    .expect("Recording");

                let original_recorded = RECORDED.load(Ordering::Relaxed);

                // Page in the zero offsets only to avoid readahead strangeness.
                {
                    let mut writable = [0u8];
                    let hash = &hashes[0];
                    let vmo = fixture.get_blob_vmo(*hash).await;
                    vmo.read(&mut writable, 0).expect("Vmo read");
                }

                // The recording happens asynchronously, so we must wait.  This is crude, but it's
                // only for testing and it's simple.
                while RECORDED.load(Ordering::Relaxed) == original_recorded {
                    fasync::Timer::new(std::time::Duration::from_millis(10)).await;
                }

                volume.stop_profile_tasks().await;
            }
            fixture.close().await
        };

        device.ensure_unique();
        device.reopen(false);
        let fixture = blob_testing::open_blob_fixture(device).await;
        {
            // Need to get the root vmo to check committed bytes.
            let dir = fixture
                .volume()
                .root()
                .clone()
                .into_any()
                .downcast::<BlobDirectory>()
                .expect("Root should be BlobDirectory");

            // Ensure that nothing is paged in right now.
            for hash in &hashes {
                let blob = dir.lookup_blob(*hash).await.expect("Opening blob");
                assert_eq!(blob.vmo().info().unwrap().committed_bytes, 0);
            }

            let volume = fixture.volume().volume();

            volume
                .record_or_replay_profile(new_profile_state(true), "foo")
                .await
                .expect("Replaying");

            // Await all data being played back by checking that things have paged in.
            {
                let hash = &hashes[0];
                let blob = dir.lookup_blob(*hash).await.expect("Opening blob");
                while blob.vmo().info().unwrap().committed_bytes == 0 {
                    fasync::Timer::new(Duration::from_millis(25)).await;
                }
            }

            let original_recorded = RECORDED.load(Ordering::Relaxed);

            // Record the new profile that will overwrite it.
            {
                let mut writable = [0u8];
                let hash = &hashes[1];
                let vmo = fixture.get_blob_vmo(*hash).await;
                vmo.read(&mut writable, 0).expect("Vmo read");
            }

            // The recording happens asynchronously, so we must wait.  This is crude, but it's only
            // for testing and it's simple.
            while RECORDED.load(Ordering::Relaxed) == original_recorded {
                fasync::Timer::new(std::time::Duration::from_millis(10)).await;
            }

            // Complete the recording.
            volume.stop_profile_tasks().await;
        }
        let device = fixture.close().await;

        device.ensure_unique();
        device.reopen(false);
        let fixture = blob_testing::open_blob_fixture(device).await;
        {
            // Need to get the root vmo to check committed bytes.
            let dir = fixture
                .volume()
                .root()
                .clone()
                .into_any()
                .downcast::<BlobDirectory>()
                .expect("Root should be BlobDirectory");

            // Ensure that nothing is paged in right now.
            for hash in &hashes {
                let blob = dir.lookup_blob(*hash).await.expect("Opening blob");
                assert_eq!(blob.vmo().info().unwrap().committed_bytes, 0);
            }

            fixture
                .volume()
                .volume()
                .record_or_replay_profile(new_profile_state(true), "foo")
                .await
                .expect("Replaying");

            // Await all data being played back by checking that things have paged in.
            {
                let hash = &hashes[1];
                let blob = dir.lookup_blob(*hash).await.expect("Opening blob");
                while blob.vmo().info().unwrap().committed_bytes == 0 {
                    fasync::Timer::new(Duration::from_millis(25)).await;
                }
            }

            // Complete the recording.
            fixture.volume().volume().stop_profile_tasks().await;

            // Verify that first blob was not paged in as the it should be dropped from the profile.
            {
                let hash = &hashes[0];
                let blob = dir.lookup_blob(*hash).await.expect("Opening blob");
                assert_eq!(blob.vmo().info().unwrap().committed_bytes, 0);
            }
        }
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_unencrypted_volume() {
        let fixture = TestFixture::new_unencrypted().await;
        let root = fixture.root();

        let f = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        close_file_checked(f).await;

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_read_only_unencrypted_volume() {
        // Make a new Fxfs filesystem with an unencrypted volume named "vol".
        let fs = {
            let device = fxfs::filesystem::mkfs_with_volume(
                DeviceHolder::new(FakeDevice::new(8192, 512)),
                "vol",
                None,
            )
            .await
            .unwrap();
            // Re-open the device as read-only and mount the filesystem as read-only.
            device.reopen(true);
            FxFilesystemBuilder::new().read_only(true).open(device).await.unwrap()
        };
        // Ensure we can access the volume and gracefully terminate any tasks.
        {
            let root_volume = root_volume(fs.clone()).await.unwrap();
            let store = root_volume.volume("vol", NO_OWNER, None).await.unwrap();
            let unique_id = store.store_object_id();
            let volume = FxVolume::new(Weak::new(), store, unique_id).unwrap();
            volume.terminate().await;
        }
        // Close the filesystem, and make sure we don't have any dangling references.
        fs.close().await.unwrap();
        let device = fs.take_device().await;
        device.ensure_unique();
    }

    #[fuchsia::test]
    async fn test_read_only_encrypted_volume() {
        let crypt: Arc<InsecureCrypt> = Arc::new(InsecureCrypt::new());
        // Make a new Fxfs filesystem with an encrypted volume named "vol".
        let fs = {
            let device = fxfs::filesystem::mkfs_with_volume(
                DeviceHolder::new(FakeDevice::new(8192, 512)),
                "vol",
                Some(crypt.clone()),
            )
            .await
            .unwrap();
            // Re-open the device as read-only and mount the filesystem as read-only.
            device.reopen(true);
            FxFilesystemBuilder::new().read_only(true).open(device).await.unwrap()
        };
        // Ensure we can access the volume and gracefully terminate any tasks.
        {
            let root_volume = root_volume(fs.clone()).await.unwrap();
            let store = root_volume.volume("vol", NO_OWNER, Some(crypt)).await.unwrap();
            let unique_id = store.store_object_id();
            let volume = FxVolume::new(Weak::new(), store, unique_id).unwrap();
            volume.terminate().await;
        }
        // Close the filesystem, and make sure we don't have any dangling references.
        fs.close().await.unwrap();
        let device = fs.take_device().await;
        device.ensure_unique();
    }

    #[fuchsia::test]
    fn test_read_ahead_sizes() {
        let config = MemoryPressureConfig::default();
        assert!(config.mem_normal.read_ahead_size % BASE_READ_AHEAD_SIZE == 0);
        assert_eq!(config.mem_normal.read_ahead_size, MAX_READ_AHEAD_SIZE);

        assert!(config.mem_warning.read_ahead_size % BASE_READ_AHEAD_SIZE == 0);

        assert_eq!(config.mem_critical.read_ahead_size, BASE_READ_AHEAD_SIZE);
    }

    #[fuchsia::test]
    fn test_refault_tracker() {
        fn get_stats(tracker: &RefaultTracker) -> (u64, u64, [u64; 8]) {
            let tracker = tracker.0.lock();
            (tracker.count, tracker.bytes, tracker.histogram)
        }

        let refault_tracker = RefaultTracker::default();
        refault_tracker.record_refault(10, 1);
        assert_eq!(get_stats(&refault_tracker), (1, 10, [1, 0, 0, 0, 0, 0, 0, 0]));
        refault_tracker.record_refault(10, 1);
        assert_eq!(get_stats(&refault_tracker), (2, 20, [2, 0, 0, 0, 0, 0, 0, 0]));
        refault_tracker.record_refault(10, 2);
        assert_eq!(get_stats(&refault_tracker), (3, 30, [1, 1, 0, 0, 0, 0, 0, 0]));
        refault_tracker.record_refault(10, 3);
        assert_eq!(get_stats(&refault_tracker), (4, 40, [1, 1, 0, 0, 0, 0, 0, 0]));
        refault_tracker.record_refault(10, 4);
        assert_eq!(get_stats(&refault_tracker), (5, 50, [1, 0, 1, 0, 0, 0, 0, 0]));

        let refault_tracker = RefaultTracker::default();
        // The refault-tracker should see every chunk counter increase but it's possible they could
        // arrive out of order. The operations wrap on overflow so it should eventually become
        // consistent.
        refault_tracker.record_refault(10, 2);
        assert_eq!(get_stats(&refault_tracker), (1, 10, [u64::MAX, 1, 0, 0, 0, 0, 0, 0]));
        refault_tracker.record_refault(10, 1);
        assert_eq!(get_stats(&refault_tracker), (2, 20, [0, 1, 0, 0, 0, 0, 0, 0]));
        refault_tracker.record_refault(10, 4);
        assert_eq!(get_stats(&refault_tracker), (3, 30, [0, 0, 1, 0, 0, 0, 0, 0]));
        refault_tracker.record_refault(10, 3);
        assert_eq!(get_stats(&refault_tracker), (4, 40, [0, 0, 1, 0, 0, 0, 0, 0]));
    }
}
