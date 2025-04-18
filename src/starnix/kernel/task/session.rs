// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use macro_rules_attribute::apply;
use starnix_sync::RwLock;
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use crate::device::terminal::Terminal;
use crate::mutable_state::{state_accessor, state_implementation};
use crate::task::ProcessGroup;
use starnix_uapi::pid_t;

#[derive(Debug)]
pub struct SessionMutableState {
    /// The process groups in the session
    ///
    /// The references to ProcessGroup is weak to prevent cycles as ProcessGroup have a Arc reference to their
    /// session.
    /// It is still expected that these weak references are always valid, as process groups must unregister
    /// themselves before they are deleted.
    process_groups: BTreeMap<pid_t, Weak<ProcessGroup>>,

    /// The leader of the foreground process group. This is necessary because the leader must
    /// be returned even if the process group has already been deleted.
    foreground_process_group: pid_t,

    /// The controlling terminal of the session.
    pub controlling_terminal: Option<ControllingTerminal>,
}

/// A session is a collection of `ProcessGroup` objects that are related to each other. Each
/// session has a session ID (`sid`), which is a unique identifier for the session.
///
/// The session leader is the first `ProcessGroup` in a session. It is responsible for managing the
/// session, including sending signals to all processes in the session and controlling the
/// foreground and background process groups.
///
/// When a `ProcessGroup` is created, it is automatically added to the session of its parent.
/// See `setsid(2)` for information about creating sessions.
///
/// A session can be destroyed when the session leader exits or when all process groups in the
/// session are destroyed.
#[derive(Debug)]
pub struct Session {
    /// The leader of the session
    pub leader: pid_t,

    /// The mutable state of the Session.
    mutable_state: RwLock<SessionMutableState>,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.leader == other.leader
    }
}

impl Session {
    pub fn new(leader: pid_t) -> Arc<Session> {
        Arc::new(Session {
            leader,
            mutable_state: RwLock::new(SessionMutableState {
                process_groups: BTreeMap::new(),
                foreground_process_group: leader,
                controlling_terminal: None,
            }),
        })
    }

    state_accessor!(Session, mutable_state);
}

#[apply(state_implementation!)]
impl SessionMutableState<Base = Session> {
    /// Removes the process group from the session. Returns whether the session is empty.
    pub fn remove(&mut self, pid: pid_t) {
        self.process_groups.remove(&pid);
    }

    pub fn insert(&mut self, process_group: &Arc<ProcessGroup>) {
        self.process_groups.insert(process_group.leader, Arc::downgrade(process_group));
    }

    pub fn get_foreground_process_group_leader(&self) -> pid_t {
        self.foreground_process_group
    }

    pub fn get_foreground_process_group(&self) -> Option<Arc<ProcessGroup>> {
        self.process_groups.get(&self.foreground_process_group).and_then(Weak::upgrade)
    }

    pub fn set_foreground_process_group(&mut self, process_group: &Arc<ProcessGroup>) {
        self.foreground_process_group = process_group.leader;
    }
}

/// The controlling terminal of a session.
#[derive(Clone, Debug)]
pub struct ControllingTerminal {
    /// The controlling terminal.
    pub terminal: Arc<Terminal>,
    /// Whether the session is associated to the main or replica side of the terminal.
    pub is_main: bool,
}

impl ControllingTerminal {
    pub fn new(terminal: Arc<Terminal>, is_main: bool) -> Self {
        Self { terminal, is_main }
    }

    pub fn matches(&self, terminal: &Arc<Terminal>, is_main: bool) -> bool {
        Arc::ptr_eq(terminal, &self.terminal) && is_main == self.is_main
    }
}
