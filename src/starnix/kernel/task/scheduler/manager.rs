// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{RoleOverrides, Task};
use fidl::HandleBased;
use fidl_fuchsia_scheduler::{
    RoleManagerMarker, RoleManagerSetRoleRequest, RoleManagerSynchronousProxy, RoleName, RoleTarget,
};
use fuchsia_component::client::connect_to_protocol_sync;
use starnix_logging::{impossible_error, log_debug, log_warn, track_stub};
use starnix_uapi::errors::Errno;
use starnix_uapi::{
    errno, error, sched_param, SCHED_BATCH, SCHED_DEADLINE, SCHED_FIFO, SCHED_IDLE, SCHED_NORMAL,
    SCHED_RESET_ON_FORK, SCHED_RR,
};
use std::cmp::Ordering;

pub struct SchedulerManager {
    role_manager: Option<RoleManagerSynchronousProxy>,
    role_overrides: RoleOverrides,
}

impl SchedulerManager {
    /// Create a new SchedulerManager which will apply any provided `role_overrides` before
    /// computing a role name based on a Task's scheduler policy.
    pub fn new(role_overrides: RoleOverrides) -> SchedulerManager {
        let role_manager = connect_to_protocol_sync::<RoleManagerMarker>().unwrap();
        let role_manager = if let Err(e) = Self::set_thread_role_inner(
            &role_manager,
            &*fuchsia_runtime::thread_self(),
            SchedulerPolicyKind::default().role_name(),
        ) {
            log_debug!("Setting thread role failed ({e:?}), will not set thread priority.");
            None
        } else {
            log_debug!("Thread role set successfully, scheduler manager initialized.");
            Some(role_manager)
        };

        SchedulerManager { role_manager, role_overrides }
    }

    /// Create a new empty SchedulerManager for testing.
    pub fn empty_for_tests() -> Self {
        Self { role_manager: None, role_overrides: RoleOverrides::new().build().unwrap() }
    }

    /// Return the currently active role name for this task. Requires read access to `task`'s state,
    /// should only be called by code which is not already modifying the provided `task`.
    pub fn role_name(&self, task: &Task) -> Result<&str, Errno> {
        let policy = task.read().scheduler_policy;
        self.role_name_inner(task, policy)
    }

    fn role_name_inner(&self, task: &Task, policy: SchedulerPolicy) -> Result<&str, Errno> {
        Ok(if policy.kind.is_realtime() {
            let process_name = task
                .thread_group()
                .read()
                .get_task(task.thread_group().leader)
                .ok_or_else(|| errno!(EINVAL))?
                .command();
            let thread_name = task.command();
            if let Some(name) =
                self.role_overrides.get_role_name(process_name.as_bytes(), thread_name.as_bytes())
            {
                name
            } else {
                policy.kind.role_name()
            }
        } else {
            policy.kind.role_name()
        })
    }

    /// Give the provided `task`'s Zircon thread a role.
    ///
    /// Requires passing the current `policy` so that this can be performed without touching
    /// `task`'s state lock.
    pub fn set_thread_role(&self, task: &Task, policy: SchedulerPolicy) -> Result<(), Errno> {
        let Some(role_manager) = self.role_manager.as_ref() else {
            log_debug!("no role manager for setting role");
            return Ok(());
        };

        let role_name = self.role_name_inner(task, policy)?;
        let thread = task.thread.read();
        let Some(thread) = thread.as_ref() else {
            log_debug!("thread role update requested for task without thread, skipping");
            return Ok(());
        };
        Self::set_thread_role_inner(role_manager, thread, role_name)
    }

    fn set_thread_role_inner(
        role_manager: &RoleManagerSynchronousProxy,
        thread: &zx::Thread,
        role_name: &str,
    ) -> Result<(), Errno> {
        log_debug!(role_name; "setting thread role");

        let thread = thread.duplicate_handle(zx::Rights::SAME_RIGHTS).map_err(impossible_error)?;
        let request = RoleManagerSetRoleRequest {
            target: Some(RoleTarget::Thread(thread)),
            role: Some(RoleName { role: role_name.to_string() }),
            ..Default::default()
        };
        let _ = role_manager.set_role(request, zx::MonotonicInstant::INFINITE).map_err(|err| {
            log_warn!(err:%; "Unable to set thread role.");
            errno!(EINVAL)
        })?;
        Ok(())
    }
}

// In user space, priority (niceness) is an integer from -20..19 (inclusive)
// with the default being 0.
//
// In the kernel it is represented as a range from 1..40 (inclusive).
// The conversion is done by the formula: user_nice = 20 - kernel_nice.
//
// In POSIX, priority is a per-process setting, but in Linux it is per-thread.
// See https://man7.org/linux/man-pages/man2/setpriority.2.html#BUGS and
// https://man7.org/linux/man-pages/man2/setpriority.2.html#NOTES
const DEFAULT_TASK_PRIORITY: u8 = 20;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, PartialOrd)]
pub struct SchedulerPolicy {
    kind: SchedulerPolicyKind,
    reset_on_fork: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerPolicyKind {
    Normal {
        // 1-40, from setpriority()
        priority: u8,
    },
    Batch {
        // 1-40, from setpriority()
        priority: u8,
    },
    Idle {
        // 1-40, from setpriority()
        priority: u8,
    },
    Fifo {
        /// 0-99, from sched_setscheduler()
        priority: u8,
    },
    RoundRobin {
        /// 0-99, from sched_setscheduler()
        priority: u8,
    },
}

impl PartialOrd for SchedulerPolicyKind {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let o1 = self.ordering();
        let o2 = other.ordering();
        if o1 != o2 {
            o1.partial_cmp(&o2)
        } else if self == other {
            Some(Ordering::Equal)
        } else {
            // FIFO and RR are not comparable
            None
        }
    }
}

impl std::default::Default for SchedulerPolicyKind {
    fn default() -> Self {
        Self::Normal { priority: DEFAULT_TASK_PRIORITY }
    }
}

impl SchedulerPolicy {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }

    fn from_raw(mut policy: u32, priority: u8) -> Result<Self, Errno> {
        let reset_on_fork = (policy & SCHED_RESET_ON_FORK) != 0;
        if reset_on_fork {
            track_stub!(
                TODO("https://fxbug.dev/297961833"),
                "SCHED_RESET_ON_FORK check CAP_SYS_NICE"
            );
            policy -= SCHED_RESET_ON_FORK;
        }
        let kind = match policy {
            SCHED_FIFO => SchedulerPolicyKind::Fifo { priority },
            SCHED_RR => SchedulerPolicyKind::RoundRobin { priority },
            SCHED_NORMAL => SchedulerPolicyKind::Normal { priority },
            SCHED_BATCH => SchedulerPolicyKind::Batch { priority },
            SCHED_IDLE => SchedulerPolicyKind::Idle { priority },
            _ => return error!(EINVAL),
        };

        Ok(Self { kind, reset_on_fork })
    }

    pub fn from_binder(policy: u32, priority: u32) -> Result<Option<Self>, Errno> {
        if priority == 0 && policy == 0 {
            Ok(None)
        } else {
            if policy != SCHED_NORMAL && policy != SCHED_RR && policy != SCHED_FIFO {
                return error!(EINVAL);
            }
            let priority: u8 = if policy == SCHED_NORMAL {
                let signed_priority = (priority as u8) as i8;
                if signed_priority < -20 || signed_priority > 19 {
                    return error!(EINVAL);
                }
                (20 - signed_priority) as u8
            } else {
                if priority < 1 || priority > 99 {
                    return error!(EINVAL);
                }
                priority as u8
            };
            Self::from_raw(policy, priority).map(Some)
        }
    }

    pub fn from_sched_params(policy: u32, params: sched_param, rlimit: u64) -> Result<Self, Errno> {
        let mut priority = u8::try_from(params.sched_priority).map_err(|_| errno!(EINVAL))?;
        let raw_policy = policy & !SCHED_RESET_ON_FORK;
        let valid_priorities =
            min_priority_for_sched_policy(raw_policy)?..=max_priority_for_sched_policy(raw_policy)?;
        if !valid_priorities.contains(&priority) {
            return error!(EINVAL);
        }
        priority = std::cmp::min(priority as u64, rlimit) as u8;
        Self::from_raw(policy, priority)
    }

    pub fn fork(self) -> Self {
        if self.reset_on_fork {
            Self {
                kind: match self.kind {
                    // If the calling thread has a scheduling policy of SCHED_FIFO or
                    // SCHED_RR, the policy is reset to SCHED_OTHER in child processes.
                    SchedulerPolicyKind::Fifo { .. } | SchedulerPolicyKind::RoundRobin { .. } => {
                        SchedulerPolicyKind::default()
                    }

                    // If the calling process has a negative nice value, the nice
                    // value is reset to zero in child processes.
                    SchedulerPolicyKind::Normal { .. } => {
                        SchedulerPolicyKind::Normal { priority: DEFAULT_TASK_PRIORITY }
                    }
                    SchedulerPolicyKind::Batch { .. } => {
                        SchedulerPolicyKind::Batch { priority: DEFAULT_TASK_PRIORITY }
                    }
                    SchedulerPolicyKind::Idle { .. } => {
                        SchedulerPolicyKind::Idle { priority: DEFAULT_TASK_PRIORITY }
                    }
                },
                // This flag is disabled in child processes created by fork(2).
                reset_on_fork: false,
            }
        } else {
            self
        }
    }

    pub fn raw_policy(&self) -> u32 {
        let mut base = match self.kind {
            SchedulerPolicyKind::Normal { .. } => SCHED_NORMAL,
            SchedulerPolicyKind::Batch { .. } => SCHED_BATCH,
            SchedulerPolicyKind::Idle { .. } => SCHED_IDLE,
            SchedulerPolicyKind::Fifo { .. } => SCHED_FIFO,
            SchedulerPolicyKind::RoundRobin { .. } => SCHED_RR,
        };
        if self.reset_on_fork {
            base |= SCHED_RESET_ON_FORK;
        }
        base
    }

    /// Return the raw "normal priority" for a process, in the range 1-40. This is the value used to
    /// compute nice, and does not apply to real-time scheduler policies.
    pub fn raw_priority(&self) -> u8 {
        match self.kind {
            SchedulerPolicyKind::Normal { priority }
            | SchedulerPolicyKind::Batch { priority }
            | SchedulerPolicyKind::Idle { priority } => priority,
            _ => DEFAULT_TASK_PRIORITY,
        }
    }

    /// Set the "normal priority" for a process, in the range 1-40. This is the value used to
    /// compute nice, and does not apply to real-time scheduler policies.
    pub fn set_raw_nice(&mut self, new_priority: u8) {
        match &mut self.kind {
            SchedulerPolicyKind::Normal { priority }
            | SchedulerPolicyKind::Batch { priority }
            | SchedulerPolicyKind::Idle { priority } => *priority = new_priority,
            _ => (),
        }
    }

    pub fn raw_params(&self) -> sched_param {
        match self.kind {
            SchedulerPolicyKind::Normal { .. }
            | SchedulerPolicyKind::Batch { .. }
            | SchedulerPolicyKind::Idle { .. } => sched_param { sched_priority: 0 },
            SchedulerPolicyKind::Fifo { priority }
            | SchedulerPolicyKind::RoundRobin { priority } => {
                sched_param { sched_priority: priority as i32 }
            }
        }
    }
}

impl SchedulerPolicyKind {
    fn is_realtime(&self) -> bool {
        matches!(self, Self::Fifo { .. } | Self::RoundRobin { .. })
    }

    /// Returns a tuple allowing to compare 2 policies.
    fn ordering(&self) -> (u8, u8) {
        match self {
            Self::RoundRobin { priority } | Self::Fifo { priority } => (3, *priority),
            Self::Normal { priority } => (2, *priority),
            Self::Batch { priority } => (1, *priority),
            Self::Idle { priority } => (0, *priority),
        }
    }

    /// Returns a number 0-31 (inclusive) mapping Linux scheduler priority to a Zircon priority
    /// level for the fair scheduler.
    ///
    /// The range of 32 Zircon priorities is divided into a region for each flavor of Linux
    /// scheduling:
    ///
    /// 1. 0-3 (inclusive) is used for SCHED_IDLE, the lowest priority Linux tasks.
    /// 2. 6-15 (inclusive) is used for lower-than-default-priority SCHED_OTHER/SCHED_BATCH tasks.
    /// 3. 16 is used for the default priority SCHED_OTHER/SCHED_BATCH, the same as Zircon's
    ///    default for Fuchsia processes.
    /// 4. 17-26 (inclusive) is used for higher-than-default-priority SCHED_OTHER/SCHED_BATCH tasks.
    /// 5. Realtime tasks receive their own profile name.
    fn role_name(&self) -> &'static str {
        match self {
            // Configured with nice 0-40, mapped to 0-3.
            Self::Idle { priority } => FAIR_PRIORITY_ROLE_NAMES[*priority as usize / 11],

            // Configured with nice 0-40 and mapped to 6-26. 20 is the default nice which we want to
            // map to 16.
            Self::Normal { priority } => FAIR_PRIORITY_ROLE_NAMES[(*priority as usize / 2) + 6],
            Self::Batch { priority } => {
                track_stub!(TODO("https://fxbug.dev/308055542"), "SCHED_BATCH hinting");
                FAIR_PRIORITY_ROLE_NAMES[(*priority as usize / 2) + 6]
            }

            // Configured with priority 1-99, mapped to a constant bandwidth profile. Priority
            // between realtime tasks is ignored because we don't currently have a way to tell the
            // scheduler that a given realtime task is more important than another without
            // specifying an earlier deadline for the higher priority task. We can't specify
            // deadlines at runtime, so we'll treat their priorities all the same.
            Self::Fifo { .. } | Self::RoundRobin { .. } => REALTIME_ROLE_NAME,
        }
    }
}

pub fn min_priority_for_sched_policy(policy: u32) -> Result<u8, Errno> {
    match policy {
        SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Ok(0),
        SCHED_FIFO | SCHED_RR => Ok(1),
        _ => error!(EINVAL),
    }
}

pub fn max_priority_for_sched_policy(policy: u32) -> Result<u8, Errno> {
    match policy {
        SCHED_NORMAL | SCHED_BATCH | SCHED_IDLE | SCHED_DEADLINE => Ok(0),
        SCHED_FIFO | SCHED_RR => Ok(99),
        _ => error!(EINVAL),
    }
}

/// Names of RoleManager roles for each static Zircon priority in the fair scheduler.
/// The index in the array is equal to the static priority.
// LINT.IfChange
const FAIR_PRIORITY_ROLE_NAMES: [&str; 32] = [
    "fuchsia.starnix.fair.0",
    "fuchsia.starnix.fair.1",
    "fuchsia.starnix.fair.2",
    "fuchsia.starnix.fair.3",
    "fuchsia.starnix.fair.4",
    "fuchsia.starnix.fair.5",
    "fuchsia.starnix.fair.6",
    "fuchsia.starnix.fair.7",
    "fuchsia.starnix.fair.8",
    "fuchsia.starnix.fair.9",
    "fuchsia.starnix.fair.10",
    "fuchsia.starnix.fair.11",
    "fuchsia.starnix.fair.12",
    "fuchsia.starnix.fair.13",
    "fuchsia.starnix.fair.14",
    "fuchsia.starnix.fair.15",
    "fuchsia.starnix.fair.16",
    "fuchsia.starnix.fair.17",
    "fuchsia.starnix.fair.18",
    "fuchsia.starnix.fair.19",
    "fuchsia.starnix.fair.20",
    "fuchsia.starnix.fair.21",
    "fuchsia.starnix.fair.22",
    "fuchsia.starnix.fair.23",
    "fuchsia.starnix.fair.24",
    "fuchsia.starnix.fair.25",
    "fuchsia.starnix.fair.26",
    "fuchsia.starnix.fair.27",
    "fuchsia.starnix.fair.28",
    "fuchsia.starnix.fair.29",
    "fuchsia.starnix.fair.30",
    "fuchsia.starnix.fair.31",
];
const REALTIME_ROLE_NAME: &str = "fuchsia.starnix.realtime";
// LINT.ThenChange(src/starnix/config/starnix.profiles)

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use starnix_uapi::errors::EINVAL;

    #[fuchsia::test]
    fn default_role_name() {
        assert_eq!(SchedulerPolicyKind::default().role_name(), "fuchsia.starnix.fair.16");
    }

    #[fuchsia::test]
    fn normal_with_non_default_nice_role_name() {
        assert_eq!(
            SchedulerPolicyKind::Normal { priority: 10 }.role_name(),
            "fuchsia.starnix.fair.11"
        );
        assert_eq!(
            SchedulerPolicyKind::Normal { priority: 27 }.role_name(),
            "fuchsia.starnix.fair.19"
        );
    }

    #[fuchsia::test]
    fn fifo_role_name() {
        assert_eq!(
            SchedulerPolicyKind::Fifo { priority: 1 }.role_name(),
            "fuchsia.starnix.realtime",
        );
        assert_eq!(
            SchedulerPolicyKind::Fifo { priority: 2 }.role_name(),
            "fuchsia.starnix.realtime",
        );
        assert_eq!(
            SchedulerPolicyKind::Fifo { priority: 99 }.role_name(),
            "fuchsia.starnix.realtime",
        );
    }

    #[fuchsia::test]
    fn build_policy_from_sched_params() {
        assert_matches!(
            SchedulerPolicy::from_sched_params(SCHED_NORMAL, sched_param { sched_priority: 0 }, 20),
            Ok(_)
        );
        assert_matches!(
            SchedulerPolicy::from_sched_params(
                SCHED_NORMAL | SCHED_RESET_ON_FORK,
                sched_param { sched_priority: 0 },
                20
            ),
            Ok(_)
        );
        assert_matches!(
            SchedulerPolicy::from_sched_params(
                SCHED_NORMAL,
                sched_param { sched_priority: 1 },
                20
            ),
            Err(e) if e == EINVAL
        );
        assert_matches!(
            SchedulerPolicy::from_sched_params(SCHED_FIFO, sched_param { sched_priority: 1 }, 20),
            Ok(_)
        );
        assert_matches!(
            SchedulerPolicy::from_sched_params(SCHED_FIFO, sched_param { sched_priority: 0 }, 20),
            Err(e) if e == EINVAL
        );
    }

    #[fuchsia::test]
    fn build_policy_from_binder() {
        assert_matches!(SchedulerPolicy::from_binder(SCHED_NORMAL, 0), Ok(None));
        assert_matches!(
            SchedulerPolicy::from_binder(SCHED_NORMAL, (((-21) as i8) as u8).into()),
            Err(_)
        );
        assert_matches!(
            SchedulerPolicy::from_binder(SCHED_NORMAL, (((-20) as i8) as u8).into()),
            Ok(Some(_))
        );
        assert_matches!(SchedulerPolicy::from_binder(SCHED_NORMAL, 1), Ok(Some(_)));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_NORMAL, 19), Ok(Some(_)));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_NORMAL, 20), Err(_));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_FIFO, 0), Err(_));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_FIFO, 1), Ok(_));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_FIFO, 99), Ok(_));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_FIFO, 100), Err(_));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_RR, 0), Err(_));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_RR, 1), Ok(_));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_RR, 99), Ok(_));
        assert_matches!(SchedulerPolicy::from_binder(SCHED_RR, 100), Err(_));
        assert_matches!(SchedulerPolicy::from_binder(42, 0), Err(_));
        assert_matches!(SchedulerPolicy::from_binder(42, 0), Err(_));
    }
}
