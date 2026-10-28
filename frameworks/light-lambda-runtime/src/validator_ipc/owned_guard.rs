//! Resource-bearing guard. No detached reaper or runtime requirement on drop.
use std::ops::{Deref, DerefMut};
pub(super) trait Abandon {
    fn abandon(self);
}
pub(super) struct Owner<T: Abandon>(Option<T>);
impl<T: Abandon> Owner<T> {
    pub(super) fn new(value: T) -> Self {
        Self(Some(value))
    }
    pub(super) fn into_inner(mut self) -> T {
        self.0.take().expect("owned resource present")
    }
}
impl<T: Abandon> Deref for Owner<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0.as_ref().expect("owned resource present")
    }
}
impl<T: Abandon> DerefMut for Owner<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.0.as_mut().expect("owned resource present")
    }
}
impl<T: Abandon> Drop for Owner<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            value.abandon();
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::SeqCst},
    };
    struct Value {
        drops: Arc<AtomicUsize>,
        retained: Arc<Mutex<Option<Value>>>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.fetch_add(1, SeqCst);
        }
    }
    impl Abandon for Value {
        fn abandon(self) {
            let retained = self.retained.clone();
            *retained.lock().unwrap() = Some(self);
        }
    }
    fn value() -> (Value, Arc<AtomicUsize>, Arc<Mutex<Option<Value>>>) {
        let drops = Arc::new(AtomicUsize::new(0));
        let retained = Arc::new(Mutex::new(None));
        (
            Value {
                drops: drops.clone(),
                retained: retained.clone(),
            },
            drops,
            retained,
        )
    }
    #[test]
    fn ordinary_drop_retains_instead_of_refunding() {
        let (v, drops, retained) = value();
        drop(Owner::new(v));
        assert_eq!(drops.load(SeqCst), 0);
        drop(retained.lock().unwrap().take());
        assert_eq!(drops.load(SeqCst), 1);
    }
    #[test]
    fn confirmed_completion_disarms_guard_exactly_once() {
        let (v, drops, retained) = value();
        drop(Owner::new(v).into_inner());
        assert_eq!(drops.load(SeqCst), 1);
        assert!(retained.lock().unwrap().is_none());
    }
    #[test]
    fn unwind_retains_without_claiming_completion() {
        let (v, drops, retained) = value();
        assert!(
            std::panic::catch_unwind(move || {
                let _owner = Owner::new(v);
                panic!("synthetic");
            })
            .is_err()
        );
        assert_eq!(drops.load(SeqCst), 0);
        drop(retained.lock().unwrap().take());
    }
    #[tokio::test]
    async fn aborted_task_retains_its_owned_value() {
        let (v, drops, retained) = value();
        let entered = Arc::new(tokio::sync::Notify::new());
        let signal = entered.clone();
        let task = tokio::spawn(async move {
            let _owner = Owner::new(v);
            signal.notify_one();
            std::future::pending::<()>().await;
        });
        entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(drops.load(SeqCst), 0);
        drop(retained.lock().unwrap().take());
    }
}
