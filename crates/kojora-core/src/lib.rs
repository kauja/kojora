//! Core concepts for Kojora, a modular, resource-aware software factory.
//!
//! These definitions are an experimental internal starting point, not a
//! stable plugin ABI. The external plugin contract will be versioned separately.

/// Long-lived identity of a bot. A bot is not an agent process or session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BotId(pub u64);

/// Identity of a discrete, durable unit of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub u64);

/// Identity of one attempt to execute a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(pub u64);

/// Identity of a conversation or agent runtime session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(pub u64);

/// Lifecycle of one execution attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// Lightweight scheduling metadata; full task metadata belongs in storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadyTask {
    pub id: TaskId,
    pub bot_id: BotId,
    pub priority: u8,
    pub requested_memory_bytes: u64,
}

/// Resources measured on one host. All quantities are in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceSnapshot {
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
}

/// Policy interface. Decisions must still pass trusted runtime admission checks.
pub trait Scheduler {
    fn select_next(
        &self,
        tasks: &[ReadyTask],
        snapshot: ResourceSnapshot,
    ) -> Option<TaskId>;
}

/// A simple baseline policy: highest priority that fits current available RAM.
/// Production scheduling must use memory headroom, reservations and pressure.
#[derive(Debug, Default)]
pub struct PriorityScheduler;

impl Scheduler for PriorityScheduler {
    fn select_next(
        &self,
        tasks: &[ReadyTask],
        snapshot: ResourceSnapshot,
    ) -> Option<TaskId> {
        tasks
            .iter()
            .filter(|task| task.requested_memory_bytes <= snapshot.available_memory_bytes)
            .max_by_key(|task| (task.priority, std::cmp::Reverse(task.id)))
            .map(|task| task.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_selects_highest_priority_that_fits() {
        let tasks = [
            ReadyTask {
                id: TaskId(1),
                bot_id: BotId(1),
                priority: 100,
                requested_memory_bytes: 200,
            },
            ReadyTask {
                id: TaskId(2),
                bot_id: BotId(1),
                priority: 50,
                requested_memory_bytes: 40,
            },
            ReadyTask {
                id: TaskId(3),
                bot_id: BotId(2),
                priority: 20,
                requested_memory_bytes: 20,
            },
        ];
        let host = ResourceSnapshot {
            total_memory_bytes: 512,
            available_memory_bytes: 80,
        };
        assert_eq!(PriorityScheduler.select_next(&tasks, host), Some(TaskId(2)));
    }

    #[test]
    fn scheduler_returns_none_when_nothing_fits() {
        let tasks = [ReadyTask {
            id: TaskId(1),
            bot_id: BotId(1),
            priority: 1,
            requested_memory_bytes: 128,
        }];
        let host = ResourceSnapshot {
            total_memory_bytes: 256,
            available_memory_bytes: 32,
        };
        assert_eq!(PriorityScheduler.select_next(&tasks, host), None);
    }

    #[test]
    fn scheduler_ties_are_deterministic() {
        let tasks = [
            ReadyTask {
                id: TaskId(9),
                bot_id: BotId(1),
                priority: 10,
                requested_memory_bytes: 1,
            },
            ReadyTask {
                id: TaskId(3),
                bot_id: BotId(1),
                priority: 10,
                requested_memory_bytes: 1,
            },
        ];
        let host = ResourceSnapshot {
            total_memory_bytes: 256,
            available_memory_bytes: 32,
        };
        assert_eq!(PriorityScheduler.select_next(&tasks, host), Some(TaskId(3)));
    }
}
