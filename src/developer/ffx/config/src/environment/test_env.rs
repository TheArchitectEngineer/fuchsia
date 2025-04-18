// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::logging::LogDestination;
use crate::{ConfigMap, Environment, EnvironmentContext};
use anyhow::{Context, Result};
use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::{NamedTempFile, TempDir};
use tracing::level_filters::LevelFilter;

use super::{EnvVars, EnvironmentKind, ExecutableKind};

/// A structure that holds information about the test config environment for the duration
/// of a test. This object must continue to exist for the duration of the test, or the test
/// may fail.
#[must_use = "This object must be held for the duration of a test (ie. `let _env = ffx_config::test_init()`) for it to operate correctly."]
pub struct TestEnv {
    pub env_file: NamedTempFile,
    pub context: EnvironmentContext,
    pub isolate_root: TempDir,
    pub user_file: NamedTempFile,
    pub build_file: Option<NamedTempFile>,
    pub global_file: NamedTempFile,
    pub log_subscriber: Arc<dyn tracing::Subscriber + Send + Sync>,
    _guard: async_lock::MutexGuardArc<()>,
}

impl TestEnv {
    async fn new(guard: async_lock::MutexGuardArc<()>, env_vars: EnvVars) -> Result<Self> {
        let env_file = NamedTempFile::new().context("tmp access failed")?;
        let isolate_root = tempfile::tempdir()?;

        let context = EnvironmentContext::isolated(
            ExecutableKind::Test,
            isolate_root.path().to_owned(),
            env_vars,
            ConfigMap::default(),
            Some(env_file.path().to_owned()),
            None,
            false,
        )?;
        Self::build_test_env(context, env_file, isolate_root, guard).await
    }

    async fn new_intree(
        build_dir: &Path,
        guard: async_lock::MutexGuardArc<()>,
        env_vars: Option<EnvVars>,
    ) -> Result<Self> {
        let env_file = NamedTempFile::new().context("tmp access failed")?;
        let isolate_root = tempfile::tempdir()?;

        let context = EnvironmentContext::new(
            EnvironmentKind::InTree {
                tree_root: isolate_root.path().to_owned(),
                build_dir: Some(PathBuf::from(build_dir)),
            },
            ExecutableKind::Test,
            env_vars,
            ConfigMap::default(),
            Some(env_file.path().to_owned()),
            false,
        );
        Self::build_test_env(context, env_file, isolate_root, guard).await
    }

    async fn build_test_env(
        context: EnvironmentContext,
        env_file: NamedTempFile,
        isolate_root: TempDir,
        guard: async_lock::MutexGuardArc<()>,
    ) -> Result<Self> {
        let global_file = NamedTempFile::new().context("tmp access failed")?;
        let global_file_path = global_file.path().to_owned();
        let user_file = NamedTempFile::new().context("tmp access failed")?;
        let user_file_path = user_file.path().to_owned();
        let build_file =
            context.build_dir().and(Some(NamedTempFile::new().context("tmp access failed")?));

        let log_subscriber: Arc<dyn tracing::Subscriber + Send + Sync> =
            Arc::new(crate::logging::configure_subscribers(
                &context,
                vec![LogDestination::TestWriter],
                LevelFilter::DEBUG,
            ));

        // Dropping the subscriber guard causes test flakes as the tracing library panics when
        // closing an instrumentation span on a different subscriber.
        // To mitigate this, only drop the guards at thread exit.
        // See https://github.com/tokio-rs/tracing/issues/1656 for more details.
        let log_guard = tracing::subscriber::set_default(Arc::clone(&log_subscriber));

        thread_local! {
            static GUARD_STASH: Cell<Option<tracing::subscriber::DefaultGuard>> =
                const { Cell::new(None) };
        }

        GUARD_STASH.with(move |guard| guard.set(Some(log_guard)));

        let test_env = TestEnv {
            env_file,
            context,
            user_file,
            build_file,
            global_file,
            isolate_root,
            log_subscriber,
            _guard: guard,
        };

        let mut env = Environment::new_empty(test_env.context.clone());

        env.set_user(Some(&user_file_path));
        if let Some(ref build_file) = test_env.build_file {
            let build_file_path = build_file.path().to_owned();
            env.set_build(&build_file_path)?;
        }
        env.set_global(Some(&global_file_path));
        env.save().await.context("saving env file")?;

        Ok(test_env)
    }

    pub fn load(&self) -> Environment {
        self.context.load().expect("opening test env file")
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // after the test, wipe out all the test configuration we set up. Explode if things aren't as we
        // expect them.
        let mut env = crate::ENV.lock().expect("Poisoned lock");
        let env_prev = env.clone();
        *env = None;
        drop(env);

        if let Some(env_prev) = env_prev {
            assert_eq!(
                env_prev,
                self.context,
                "environment context changed from isolated environment to {other:?} during test run somehow.",
                other = env_prev
            );
        }

        // since we're not running in async context during drop, we can't clear the cache unfortunately.
    }
}

lazy_static::lazy_static! {
    static ref TEST_LOCK: Arc<async_lock::Mutex<()>> = Arc::default();
}
/// When running tests we usually just want to initialize a blank slate configuration, so
/// use this for tests. You must hold the returned object object for the duration of the test, not doing so
/// will result in strange behaviour.
pub async fn test_init() -> Result<TestEnv> {
    let env =
        TestEnv::new(TEST_LOCK.lock_arc().await, HashMap::from_iter(std::env::vars())).await?;

    // force an overwrite of the configuration setup
    crate::init(&env.context)?;

    Ok(env)
}

/// Creates a blank slate configuration which models the context when running in-tree. Specifically, it exposes
/// the build_dir() property.
/// You must hold the returned object object for the duration of the test, not doing so
/// will result in strange behaviour.
pub async fn test_init_in_tree(build_dir: &Path) -> Result<TestEnv> {
    let env = TestEnv::new_intree(build_dir, TEST_LOCK.lock_arc().await, None).await?;

    // force an overwrite of the configuration setup
    crate::init(&env.context)?;

    Ok(env)
}

/// Creates a test environment with custom environment variables which is either
/// `EnvironmentKind::InTree` if `build_dir.is_some()`, otherwise
/// `EnvironmentKind::Isolated`.
/// You must hold the returned object object for the duration of the test, not doing so
/// will result in strange behaviour.
pub async fn test_init_with_env(env_vars: EnvVars, build_dir: Option<&Path>) -> Result<TestEnv> {
    let env = match build_dir {
        Some(build_dir) => {
            TestEnv::new_intree(build_dir, TEST_LOCK.lock_arc().await, Some(env_vars)).await
        }
        None => TestEnv::new(TEST_LOCK.lock_arc().await, env_vars).await,
    }?;

    // force an overwrite of the configuration setup
    crate::init(&env.context)?;

    Ok(env)
}
