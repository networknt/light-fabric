use gateway_policy::observation::{RecordKind, RecordSink, StdoutSink};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

// The child writes into an unread pipe: the actual stdout writer stalls while
// producers fill the real queue. Success proves bounded producer completion,
// not delivery or a bound on the writer's underlying stdout operation.
#[test]
fn full_stdout_queue_does_not_block_policy_producers() {
    let marker = std::env::temp_dir().join(format!("wp12-stdout-producer-{}", std::process::id()));
    assert!(!marker.exists());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stdout_queue_child", "--nocapture"])
        .env("WP12_STDOUT_QUEUE_CHILD", "1")
        .env("WP12_STDOUT_QUEUE_MARKER", &marker)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    loop {
        if marker.exists() {
            assert_eq!(
                std::fs::read(&marker).unwrap(),
                b"512 producer calls completed"
            );
            child.kill().unwrap();
            child.wait().unwrap();
            std::fs::remove_file(&marker).unwrap();
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited before producer-completion signal: {status}");
        }
        if start.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("actual bounded queue producer blocked on stalled stdout");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn stdout_queue_child() {
    if std::env::var_os("WP12_STDOUT_QUEUE_CHILD").is_none() {
        return;
    }
    let value = serde_json::json!({"synthetic": "x".repeat(32000)});
    for _ in 0..512 {
        StdoutSink.try_emit(RecordKind::Emf, value.clone());
    }
    std::fs::write(
        std::env::var_os("WP12_STDOUT_QUEUE_MARKER").unwrap(),
        b"512 producer calls completed",
    )
    .unwrap();
    // Leave the blocked writer alive until the parent observes producer finish.
    loop {
        std::thread::park();
    }
}
