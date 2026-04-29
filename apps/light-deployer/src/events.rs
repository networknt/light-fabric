use crate::model::DeploymentEvent;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

#[derive(Clone)]
pub struct EventHub {
    sender: broadcast::Sender<DeploymentEvent>,
    history: Arc<RwLock<VecDeque<DeploymentEvent>>>,
    max_history: usize,
}

impl EventHub {
    pub fn new(max_history: usize) -> Self {
        let (sender, _) = broadcast::channel(256);
        Self {
            sender,
            history: Arc::new(RwLock::new(VecDeque::with_capacity(max_history))),
            max_history,
        }
    }

    pub async fn publish(&self, event: DeploymentEvent) {
        {
            let mut history = self.history.write().await;
            if history.len() == self.max_history {
                history.pop_front();
            }
            history.push_back(event.clone());
        }
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DeploymentEvent> {
        self.sender.subscribe()
    }

    pub async fn history_for(&self, request_id: &str) -> Vec<DeploymentEvent> {
        self.history
            .read()
            .await
            .iter()
            .filter(|event| event.request_id == request_id)
            .cloned()
            .collect()
    }
}
