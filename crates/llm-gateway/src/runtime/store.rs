use super::LlmPublishedSnapshot;
use arc_swap::ArcSwap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub enum PublishOutcome {
    Published,
    Unchanged,
}

pub struct LlmSnapshotStore {
    current: ArcSwap<LlmPublishedSnapshot>,
    retired: Mutex<VecDeque<Arc<LlmPublishedSnapshot>>>,
    max_retained_generations: usize,
}

impl LlmSnapshotStore {
    pub fn new(initial: LlmPublishedSnapshot, max_retained_generations: usize) -> Self {
        Self {
            current: ArcSwap::from_pointee(initial),
            retired: Mutex::new(VecDeque::new()),
            max_retained_generations,
        }
    }

    pub fn load(&self) -> Arc<LlmPublishedSnapshot> {
        self.current.load_full()
    }

    pub fn publish(&self, candidate: LlmPublishedSnapshot) -> PublishOutcome {
        let current = self.load();
        if current.digest == candidate.digest {
            return PublishOutcome::Unchanged;
        }
        let previous = self.current.swap(Arc::new(candidate));
        let mut retired = self
            .retired
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        retired.push_back(previous);
        while retired.len() > self.max_retained_generations {
            retired.pop_front();
        }
        PublishOutcome::Published
    }

    pub fn retained_generations(&self) -> usize {
        self.retired
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}
