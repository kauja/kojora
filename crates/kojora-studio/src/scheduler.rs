//! Fail-closed in-memory admission control for independent task runners.
//! Reservations are taken before durable Running records are published.
//! This is an *admission* budget; enforcement requires a cgroup-backed runner.

use kojora_runtime::CancellationToken;
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionPolicy {
    pub max_parallel: usize,
    pub memory_mib_per_task: u64,
    pub total_memory_mib: u64,
}

impl AdmissionPolicy {
    pub const fn single() -> Self {
        Self {
            max_parallel: 1,
            memory_mib_per_task: 0,
            total_memory_mib: 0,
        }
    }

    pub fn validate(self) -> io::Result<()> {
        if self.max_parallel == 0 || self.max_parallel > 32 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "max_parallel must be between 1 and 32"));
        }
        if self.max_parallel > 1
            && (self.memory_mib_per_task == 0 || self.total_memory_mib == 0)
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "parallel execution requires an explicit memory budget"));
        }
        if self.memory_mib_per_task > 0
            && (self.total_memory_mib < self.memory_mib_per_task
                || self.memory_mib_per_task.checked_mul(1024 * 1024).is_none())
        {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,
                "invalid per-task memory reservation"));
        }
        Ok(())
    }
}

#[derive(Clone)]
struct ActiveTask {
    profile_dir: PathBuf,
    token: CancellationToken,
}

pub struct Scheduler {
    policy: AdmissionPolicy,
    tasks: BTreeMap<u64, ActiveTask>,
}

impl Scheduler {
    pub fn new(policy: AdmissionPolicy) -> io::Result<Self> {
        policy.validate()?;
        Ok(Self { policy, tasks: BTreeMap::new() })
    }

    pub fn max_parallel(&self) -> usize {
        self.policy.max_parallel
    }

    pub fn active_ids(&self) -> Vec<u64> {
        self.tasks.keys().copied().collect()
    }

    pub fn active_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn reserved_mib(&self) -> u64 {
        (self.tasks.len() as u64).saturating_mul(self.policy.memory_mib_per_task)
    }

    pub fn token(&self, id: u64) -> Option<CancellationToken> {
        self.tasks.get(&id).map(|slot| slot.token.clone())
    }

    pub fn admit(&mut self, id: u64, profile_dir: PathBuf) -> io::Result<CancellationToken> {
        if self.tasks.len() >= self.policy.max_parallel {
            return Err(io::Error::new(io::ErrorKind::WouldBlock,
                "worker reached maximum concurrent task count"));
        }
        let next = (self.tasks.len() as u64 + 1)
            .checked_mul(self.policy.memory_mib_per_task)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "budget overflow"))?;
        if self.policy.memory_mib_per_task != 0 && next > self.policy.total_memory_mib {
            return Err(io::Error::new(io::ErrorKind::WouldBlock,
                "worker memory reservations exhausted"));
        }
        if self.tasks.values().any(|active| active.profile_dir == profile_dir) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock,
                "another run already holds this provider profile"));
        }
        if self.tasks.contains_key(&id) {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "task ID is already active"));
        }
        let token = CancellationToken::default();
        self.tasks.insert(id, ActiveTask { profile_dir, token: token.clone() });
        Ok(token)
    }

    pub fn release(&mut self, id: u64) {
        self.tasks.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admits_two_distinct_profiles_then_releases_reservation() {
        let mut s = Scheduler::new(AdmissionPolicy {
            max_parallel: 3, memory_mib_per_task: 512, total_memory_mib: 1024
        }).unwrap();
        let a = s.admit(1, "/profiles/a".into()).unwrap();
        let b = s.admit(2, "/profiles/b".into()).unwrap();
        assert_eq!(s.active_count(), 2);
        assert_eq!(s.reserved_mib(), 1024);
        assert_eq!(s.admit(3, "/profiles/c".into()).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        a.cancel();
        assert!(s.token(1).unwrap().is_cancelled());
        assert!(!b.is_cancelled());
        s.release(1);
        assert_eq!(s.reserved_mib(), 512);
        s.admit(3, "/profiles/c".into()).unwrap();
    }

    #[test]
    fn rejects_profile_reuse_and_unbounded_parallel_config() {
        let mut s = Scheduler::new(AdmissionPolicy {
            max_parallel: 4, memory_mib_per_task: 256, total_memory_mib: 4096
        }).unwrap();
        s.admit(1, "/profiles/a".into()).unwrap();
        assert_eq!(s.admit(2, "/profiles/a".into()).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(Scheduler::new(AdmissionPolicy {
            max_parallel: 4, memory_mib_per_task: 0, total_memory_mib: 0
        }).is_err());
    }
}
