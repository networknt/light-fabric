use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    task::Poll,
    time::{Duration, Instant},
};
use uuid::Uuid;
use workflow_action::guard::*;
use workflow_action::*;
fn decision() -> Decision {
    let digest = request_digest("POST", "https://target/tool", "tool", b"{}");
    Decision {
        binding: Binding {
            host_id: Uuid::now_v7(),
            user_id: Uuid::now_v7(),
            grant_id: Uuid::now_v7(),
            run_id: Uuid::now_v7(),
            action_id: Uuid::now_v7(),
            attempt_id: Uuid::now_v7(),
            calling_app: "workflow".into(),
            request_digest: digest.clone(),
            request_bytes: 2,
            response_byte_limit: 1024,
            cost_unit_limit: 1,
            tool_ref: Uuid::now_v7(),
            target: "https://target/tool".into(),
            contract_digest: digest.clone(),
            policy_digest: digest.clone(),
            disclosure_digest: digest.clone(),
            claims_digest: digest,
            grant_generation: 1,
            run_generation: 1,
            budget_generation: 1,
            action_generation: 1,
            execution_class: ExecutionClass::Standard,
            depth: 0,
            maximum_depth: 4,
            parent_action_id: None,
            deadline: chrono::Utc::now() + chrono::Duration::minutes(5),
        },
        decision_id: Uuid::now_v7(),
        owner: Owner {
            gateway_service: "gateway".into(),
            replica: Uuid::now_v7(),
            boot: Uuid::now_v7(),
            fencing_generation: 1,
        },
        generation: 1,
        lease_ms: 5000,
    }
}
#[test]
fn request_digest_binds_exact_bytes_and_framed_route() {
    assert_ne!(
        request_digest("POST", "ab", "c", b"{}"),
        request_digest("POST", "a", "bc", b"{}")
    );
    assert_ne!(
        request_digest("POST", "a", "b", b"{}"),
        request_digest("POST", "a", "b", b"{ }")
    );
}
#[test]
fn preparation_or_late_ack_never_initiates_after_deadline() {
    let d = decision();
    let g = SendGuard::new(Instant::now() - Duration::from_secs(6), d.clone()).unwrap();
    g.acknowledge(&d, true).unwrap();
    assert!(matches!(
        g.poll_first_write::<()>(|| panic!("expired write")),
        Poll::Ready(Err(_))
    ));
    assert_eq!(g.state(), SendState::AbortedNotInitiated);
    assert!(g.abort_not_initiated());
}
#[test]
fn no_duplicate_ack_or_transport_retry_even_after_pending() {
    let d = decision();
    let g = SendGuard::new(Instant::now(), d.clone()).unwrap();
    assert!(g.acknowledge(&d, false).is_err());
    assert!(matches!(
        g.poll_first_write::<()>(|| panic!("unacknowledged write")),
        Poll::Ready(Err(_))
    ));
    g.acknowledge(&d, true).unwrap();
    assert!(g.acknowledge(&d, true).is_err());
    assert!(g.poll_first_write::<()>(|| Poll::Pending).is_pending());
    assert!(!g.abort_not_initiated());
    assert!(matches!(
        g.poll_first_write::<()>(|| panic!("second initiation")),
        Poll::Ready(Err(_))
    ));
}
#[test]
fn abort_and_first_write_are_mutually_exclusive() {
    for _ in 0..100 {
        let d = decision();
        let g = Arc::new(SendGuard::new(Instant::now(), d.clone()).unwrap());
        g.acknowledge(&d, true).unwrap();
        let b = Arc::new(Barrier::new(2));
        let writes = Arc::new(AtomicUsize::new(0));
        let (g1, b1, w) = (g.clone(), b.clone(), writes.clone());
        let h = std::thread::spawn(move || {
            b1.wait();
            let _ = g1.poll_first_write(|| {
                w.fetch_add(1, Ordering::SeqCst);
                Poll::Ready(Ok(()))
            });
        });
        b.wait();
        let aborted = g.abort_not_initiated();
        h.join().unwrap();
        assert_eq!(writes.load(Ordering::SeqCst), usize::from(!aborted));
    }
}
#[test]
fn lineage_cannot_reset_depth_or_expand_authority() {
    let p = decision().binding;
    let mut c = p.clone();
    c.action_id = Uuid::now_v7();
    c.attempt_id = Uuid::now_v7();
    c.parent_action_id = Some(p.action_id);
    c.depth = 1;
    assert!(c.validate_child_of(&p).is_ok());
    c.depth = 0;
    assert!(c.validate_child_of(&p).is_err());
    c.depth = 1;
    c.deadline += chrono::Duration::seconds(1);
    assert!(c.validate_child_of(&p).is_err());
}
