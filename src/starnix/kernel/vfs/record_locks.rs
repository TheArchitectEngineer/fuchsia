// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{CurrentTask, WaitQueue, Waiter};
use crate::vfs::{FdTableId, FileObject, FileObjectId};
use starnix_sync::{Locked, Mutex, Unlocked};
use starnix_uapi::errors::{Errno, EAGAIN};
use starnix_uapi::{
    __kernel_off_t, c_short, errno, error, pid_t, uapi, F_GETLK, F_GETLK64, F_OFD_GETLK,
    F_OFD_SETLK, F_OFD_SETLKW, F_RDLCK, F_SETLK, F_SETLK64, F_SETLKW, F_SETLKW64, F_UNLCK, F_WRLCK,
    SEEK_CUR, SEEK_END, SEEK_SET,
};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordLength {
    Value(usize),
    Infinite,
}

impl RecordLength {
    fn new(value: usize) -> Self {
        if value == 0 {
            Self::Infinite
        } else {
            Self::Value(value)
        }
    }
    fn value(&self) -> __kernel_off_t {
        match self {
            Self::Value(e) => *e as __kernel_off_t,
            Self::Infinite => 0,
        }
    }
}

impl std::ops::Add<usize> for RecordLength {
    type Output = Self;

    fn add(self, element: usize) -> Self {
        match self {
            Self::Value(e) => Self::Value(e.saturating_add(element)),
            Self::Infinite => Self::Infinite,
        }
    }
}

impl std::ops::Sub<usize> for RecordLength {
    type Output = Option<Self>;

    fn sub(self, element: usize) -> Option<Self> {
        match self {
            Self::Value(e) if e > element => Some(Self::Value(e - element)),
            Self::Infinite => Some(Self::Infinite),
            _ => None,
        }
    }
}

impl std::cmp::PartialEq<usize> for RecordLength {
    fn eq(&self, other: &usize) -> bool {
        match self {
            Self::Value(e) => e == other,
            Self::Infinite => false,
        }
    }
}

impl std::cmp::PartialOrd<usize> for RecordLength {
    fn partial_cmp(&self, other: &usize) -> Option<std::cmp::Ordering> {
        match self {
            Self::Value(e) => e.partial_cmp(other),
            Self::Infinite => Some(std::cmp::Ordering::Greater),
        }
    }
}

impl Ord for RecordLength {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        if self == other {
            std::cmp::Ordering::Equal
        } else {
            match self {
                Self::Value(e1) => match other {
                    Self::Value(e2) => e1.cmp(e2),
                    Self::Infinite => std::cmp::Ordering::Less,
                },
                Self::Infinite => std::cmp::Ordering::Greater,
            }
        }
    }
}

impl PartialOrd for RecordLength {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RecordRange {
    start: usize,
    length: RecordLength,
}

impl RecordRange {
    fn new(start: usize, length: usize) -> Self {
        Self { start, length: RecordLength::new(length) }
    }

    /// Build a new `RecordRange` from the whence, start and length information in the flock
    /// struct. The opened file is used when the position needs to be considered from the local
    /// position of the file or the end of the file.
    fn build(flock: &uapi::flock, file: &FileObject) -> Result<RecordRange, Errno> {
        let origin: __kernel_off_t = match flock.l_whence as u32 {
            SEEK_SET => 0,
            SEEK_CUR => *file.offset.lock(),
            SEEK_END => file.node().info().size.try_into().map_err(|_| errno!(EINVAL))?,
            _ => {
                return error!(EINVAL);
            }
        };
        let mut start = origin.checked_add(flock.l_start).ok_or_else(|| errno!(EOVERFLOW))?;
        let mut length = flock.l_len;
        if length < 0 {
            start = start.checked_add(length).ok_or_else(|| errno!(EINVAL))?;
            length = length.checked_neg().ok_or_else(|| errno!(EINVAL))?;
        }
        if start < 0 {
            return error!(EINVAL);
        }
        Ok(Self::new(start as usize, length as usize))
    }

    fn end(&self) -> RecordLength {
        self.length + self.start
    }

    fn intersects(&self, other: &RecordRange) -> bool {
        let r1 = std::cmp::min(self, other);
        let r2 = std::cmp::max(self, other);
        r1.end() > r2.start
    }
}

impl std::ops::Sub<RecordRange> for RecordRange {
    type Output = Vec<Self>;

    fn sub(self, other: RecordRange) -> Vec<RecordRange> {
        if !self.intersects(&other) {
            return vec![self];
        }
        let mut vec = Vec::with_capacity(2);
        if self.start < other.start {
            let length = std::cmp::min(RecordLength::Value(other.start - self.start), self.length);
            vec.push(RecordRange { start: self.start, length });
        }
        if let RecordLength::Value(start) = other.end() {
            let end = self.end();
            if let Some(length) = end - start {
                vec.push(RecordRange { start, length });
            }
        }
        vec
    }
}

impl std::ops::Add<RecordRange> for RecordRange {
    type Output = Vec<Self>;

    fn add(self, other: RecordRange) -> Vec<RecordRange> {
        let r1 = std::cmp::min(self, other);
        let r2 = std::cmp::max(self, other);
        let r1_end = r1.end();
        if r1_end < r2.start {
            vec![r1, r2]
        } else {
            let end = std::cmp::max(r1_end, r2.end());
            vec![RecordRange {
                start: r1.start,
                length: (end - r1.start).expect("Length is guaranteed to exist"),
            }]
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RecordLockType {
    Read,
    Write,
}

impl RecordLockType {
    fn build(flock: &uapi::flock) -> Result<Option<Self>, Errno> {
        match flock.l_type as u32 {
            F_UNLCK => Ok(None),
            F_RDLCK => Ok(Some(Self::Read)),
            F_WRLCK => Ok(Some(Self::Write)),
            _ => error!(EINVAL),
        }
    }

    /// Returns whether the current lock type is compatible with the other lock type. This only
    /// happends when both locks are read locks.
    fn is_compatible(&self, other: RecordLockType) -> bool {
        *self == Self::Read && other == Self::Read
    }

    fn has_permission(&self, file: &FileObject) -> bool {
        match self {
            Self::Read => file.can_read(),
            Self::Write => file.can_write(),
        }
    }

    fn value(&self) -> c_short {
        match self {
            Self::Read => F_RDLCK as c_short,
            Self::Write => F_WRLCK as c_short,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecordLockOwner {
    FdTable(FdTableId),
    FileObject(FileObjectId),
}

impl RecordLockOwner {
    fn new(current_task: &CurrentTask, cmd: RecordLockCommand, file: &FileObject) -> Self {
        if cmd.is_ofd() {
            RecordLockOwner::FileObject(file.id)
        } else {
            RecordLockOwner::FdTable(current_task.files.id())
        }
    }
}

#[derive(Debug, Clone)]
struct RecordLock {
    pub owner: RecordLockOwner,
    pub range: RecordRange,
    pub lock_type: RecordLockType,
    pub process_id: pid_t,
}

impl RecordLock {
    fn as_tuple(&self) -> (RecordLockOwner, &RecordRange, RecordLockType) {
        (self.owner, &self.range, self.lock_type)
    }
}

impl PartialEq for RecordLock {
    fn eq(&self, other: &Self) -> bool {
        self.as_tuple() == other.as_tuple()
    }
}

impl Eq for RecordLock {}

impl Ord for RecordLock {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_tuple().cmp(&other.as_tuple())
    }
}

impl PartialOrd for RecordLock {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
#[derive(Default, Debug)]
pub struct RecordLocksState {
    locks: BTreeSet<RecordLock>,
    queue: WaitQueue,
}

impl RecordLocksState {
    /// Returns any lock that would conflict with a lock of type `lock_type` over `range` by
    /// `fd_table`.
    fn get_conflicting_lock(
        &self,
        owner: RecordLockOwner,
        lock_type: RecordLockType,
        range: &RecordRange,
    ) -> Option<uapi::flock> {
        for record in &self.locks {
            if owner == record.owner {
                continue;
            }
            if lock_type.is_compatible(record.lock_type) {
                continue;
            }
            if range.intersects(&record.range) {
                return Some(uapi::flock {
                    l_type: record.lock_type.value(),
                    l_whence: SEEK_SET as c_short,
                    l_start: record.range.start as __kernel_off_t,
                    l_len: record.range.length.value(),
                    l_pid: record.process_id,
                    ..Default::default()
                });
            }
        }
        None
    }

    fn apply_lock(
        &mut self,
        process_id: pid_t,
        owner: RecordLockOwner,
        lock_type: RecordLockType,
        range: RecordRange,
    ) -> Result<(), Errno> {
        let mut owned_locks_in_range = Vec::new();
        for lock in self.locks.iter().filter(|record| range.intersects(&record.range)) {
            if owner == lock.owner {
                owned_locks_in_range.push(lock.clone());
            } else if !lock_type.is_compatible(lock.lock_type) {
                // conflict
                return error!(EAGAIN);
            }
        }
        let mut new_lock = RecordLock { owner, range, lock_type, process_id };
        for lock in owned_locks_in_range {
            self.locks.remove(&lock);
            if lock.lock_type == lock_type {
                let new_ranges = new_lock.range + lock.range;
                assert!(new_ranges.len() == 1);
                new_lock.range = new_ranges[0];
            } else {
                for range in lock.range - new_lock.range {
                    let mut remaining_lock = lock.clone();
                    remaining_lock.range = range;
                    self.locks.insert(remaining_lock);
                }
            }
        }
        self.locks.insert(new_lock);
        self.queue.notify_all();
        Ok(())
    }

    fn unlock(&mut self, owner: RecordLockOwner, range: RecordRange) -> Result<(), Errno> {
        let intersection_locks: Vec<_> = self
            .locks
            .iter()
            .filter(|record| owner == record.owner && range.intersects(&record.range))
            .cloned()
            .collect();
        for lock in intersection_locks {
            self.locks.remove(&lock);
            for new_range in lock.range - range {
                let mut new_lock = lock.clone();
                new_lock.range = new_range;
                self.locks.insert(new_lock);
            }
        }
        self.queue.notify_all();
        Ok(())
    }

    fn release_locks(&mut self, owner: RecordLockOwner) {
        self.locks.retain(|lock| lock.owner != owner);
        self.queue.notify_all();
    }
}

#[allow(non_camel_case_types)]
#[allow(clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordLockCommand {
    SETLK,
    SETLKW,
    GETLK,
    OFD_GETLK,
    OFD_SETLK,
    OFD_SETLKW,
}

impl RecordLockCommand {
    pub fn from_raw(cmd: u32) -> Option<Self> {
        match cmd {
            F_SETLK | F_SETLK64 => Some(RecordLockCommand::SETLK),
            F_SETLKW | F_SETLKW64 => Some(RecordLockCommand::SETLKW),
            F_GETLK | F_GETLK64 => Some(RecordLockCommand::GETLK),
            F_OFD_GETLK => Some(RecordLockCommand::OFD_GETLK),
            F_OFD_SETLK => Some(RecordLockCommand::OFD_SETLK),
            F_OFD_SETLKW => Some(RecordLockCommand::OFD_SETLKW),
            _ => None,
        }
    }

    fn is_ofd(&self) -> bool {
        match self {
            RecordLockCommand::SETLK | RecordLockCommand::SETLKW | RecordLockCommand::GETLK => {
                false
            }
            RecordLockCommand::OFD_GETLK
            | RecordLockCommand::OFD_SETLK
            | RecordLockCommand::OFD_SETLKW => true,
        }
    }

    fn is_get(&self) -> bool {
        *self == RecordLockCommand::GETLK || *self == RecordLockCommand::OFD_GETLK
    }

    fn is_blocking(&self) -> bool {
        *self == RecordLockCommand::SETLKW || *self == RecordLockCommand::OFD_SETLKW
    }
}

#[derive(Default, Debug)]
pub struct RecordLocks {
    state: Mutex<RecordLocksState>,
}

impl RecordLocks {
    /// Apply the fcntl lock operation by the given `current_task`, on the given `file`.
    ///
    /// If this method succeed, and doesn't return None, the returned flock struct must be used to
    /// overwrite the content of the flock struct passed by the user.
    pub fn lock(
        &self,
        locked: &mut Locked<Unlocked>,
        current_task: &CurrentTask,
        file: &FileObject,
        cmd: RecordLockCommand,
        mut flock: uapi::flock,
    ) -> Result<Option<uapi::flock>, Errno> {
        if cmd.is_ofd() && flock.l_pid != 0 {
            return error!(EINVAL);
        }
        let owner: RecordLockOwner = RecordLockOwner::new(current_task, cmd, file);
        let lock_type = RecordLockType::build(&flock)?;
        let range = RecordRange::build(&flock, file)?;
        if cmd.is_get() {
            let lock_type = lock_type.ok_or_else(|| errno!(EINVAL))?;
            Ok(self.state.lock().get_conflicting_lock(owner, lock_type, &range).or_else(|| {
                flock.l_type = F_UNLCK as c_short;
                Some(flock)
            }))
        } else {
            match lock_type {
                Some(lock_type) => {
                    if !lock_type.has_permission(file) {
                        return error!(EBADF);
                    }
                    let blocking = cmd.is_blocking();
                    loop {
                        let mut state = self.state.lock();
                        let waiter = blocking.then(|| {
                            let waiter = Waiter::new();
                            state.queue.wait_async(&waiter);
                            waiter
                        });
                        let process_id =
                            if cmd.is_ofd() { -1 } else { current_task.thread_group().leader };
                        match state.apply_lock(process_id, owner, lock_type, range) {
                            Err(errno) if blocking && errno == EAGAIN => {
                                // TODO(qsr): Check deadlocks.
                                if let Some(waiter) = waiter {
                                    std::mem::drop(state);
                                    waiter.wait(locked, current_task)?;
                                }
                            }
                            result => return result.map(|_| None),
                        }
                    }
                }
                None => {
                    self.state.lock().unlock(owner, range)?;
                }
            }
            Ok(None)
        }
    }

    pub fn release_locks(&self, owner: RecordLockOwner) {
        self.state.lock().release_locks(owner);
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[::fuchsia::test]
    fn test_range_intersects() {
        let r1 = RecordRange::new(25, 3);
        assert!(r1.intersects(&RecordRange::new(25, 1)));
        assert!(r1.intersects(&RecordRange::new(0, 0)));
        assert!(r1.intersects(&RecordRange::new(0, 60)));
        assert!(r1.intersects(&RecordRange::new(27, 8)));
        assert!(!r1.intersects(&RecordRange::new(28, 1)));
        assert!(!r1.intersects(&RecordRange::new(29, 8)));
        assert!(!r1.intersects(&RecordRange::new(29, 0)));
        assert!(!r1.intersects(&RecordRange::new(0, 8)));
    }

    #[::fuchsia::test]
    fn test_range_sub() {
        let r1 = RecordRange::new(25, 3);
        assert_eq!(r1 - RecordRange::new(0, 2), vec!(r1));
        assert_eq!(r1 - RecordRange::new(29, 2), vec!(r1));
        assert_eq!(r1 - RecordRange::new(29, 0), vec!(r1));
        assert_eq!(r1 - RecordRange::new(20, 0), vec!());
        assert_eq!(r1 - RecordRange::new(20, 12), vec!());
        assert_eq!(r1 - RecordRange::new(20, 6), vec!(RecordRange::new(26, 2)));
        assert_eq!(r1 - RecordRange::new(26, 3), vec!(RecordRange::new(25, 1)));
        assert_eq!(r1 - RecordRange::new(26, 0), vec!(RecordRange::new(25, 1)));
        assert_eq!(
            r1 - RecordRange::new(26, 1),
            vec!(RecordRange::new(25, 1), RecordRange::new(27, 1))
        );

        let r2 = RecordRange::new(25, 0);
        assert_eq!(r2 - RecordRange::new(0, 2), vec!(r2));
        assert_eq!(r2 - RecordRange::new(20, 0), vec!());
        assert_eq!(r2 - RecordRange::new(20, 6), vec!(RecordRange::new(26, 0)));
        assert_eq!(r2 - RecordRange::new(26, 0), vec!(RecordRange::new(25, 1)));
        assert_eq!(
            r2 - RecordRange::new(26, 1),
            vec!(RecordRange::new(25, 1), RecordRange::new(27, 0))
        );
    }

    #[::fuchsia::test]
    fn test_range_add() {
        let r1 = RecordRange::new(25, 3);
        assert_eq!(r1 + RecordRange::new(0, 2), vec!(RecordRange::new(0, 2), r1));
        assert_eq!(r1 + RecordRange::new(30, 2), vec!(r1, RecordRange::new(30, 2)));
        assert_eq!(r1 + RecordRange::new(30, 0), vec!(r1, RecordRange::new(30, 0)));
        assert_eq!(r1 + RecordRange::new(22, 3), vec!(RecordRange::new(22, 6)));
        assert_eq!(r1 + RecordRange::new(22, 4), vec!(RecordRange::new(22, 6)));
        assert_eq!(r1 + RecordRange::new(22, 8), vec!(RecordRange::new(22, 8)));
        assert_eq!(r1 + RecordRange::new(22, 0), vec!(RecordRange::new(22, 0)));
        assert_eq!(r1 + RecordRange::new(26, 1), vec!(RecordRange::new(25, 3)));
        assert_eq!(r1 + RecordRange::new(26, 2), vec!(RecordRange::new(25, 3)));
        assert_eq!(r1 + RecordRange::new(26, 8), vec!(RecordRange::new(25, 9)));
        assert_eq!(r1 + RecordRange::new(26, 0), vec!(RecordRange::new(25, 0)));
    }
}
