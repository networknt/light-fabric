use light_runtime::{ShutdownReason, ShutdownWatcher};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const FIXTURE_ENV: &str = "LIGHT_RUNTIME_SIGNAL_FIXTURE";

#[test]
fn subprocess_signal_helper() {
    if std::env::var_os(FIXTURE_ENV).is_none() {
        return;
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("fixture runtime");
    runtime.block_on(async {
        let mut watcher = ShutdownWatcher::install().expect("install shutdown watcher");
        println!("READY");
        std::io::stdout().flush().expect("flush readiness");
        let reason = watcher.recv().await;
        println!("REASON={reason:?}");
        std::io::stdout().flush().expect("flush reason");
    });
}

#[cfg(unix)]
fn assert_signal(signal: &str, expected: ShutdownReason) {
    let executable = std::env::current_exe().expect("current test executable");
    let mut child = Command::new(executable)
        .args(["--exact", "subprocess_signal_helper", "--nocapture"])
        .env(FIXTURE_ENV, "1")
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn signal fixture");
    let stdout = child.stdout.take().expect("fixture stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        assert_ne!(reader.read_line(&mut line).expect("read fixture output"), 0);
        if line.trim() == "READY" {
            break;
        }
    }

    let status = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .expect("send fixture signal");
    assert!(status.success());
    wait_for_exit(&mut child);

    let mut remainder = String::new();
    reader
        .read_to_string(&mut remainder)
        .expect("read shutdown reason");
    assert!(remainder.contains(&format!("REASON={expected:?}")));
    assert!(child.wait().expect("fixture exit status").success());
}

#[cfg(unix)]
fn wait_for_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if child.try_wait().expect("poll fixture").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.kill().expect("terminate stuck signal fixture");
    panic!("signal fixture did not exit within two seconds");
}

#[cfg(unix)]
#[test]
fn watcher_accepts_sigterm_in_a_subprocess() {
    assert_signal("-TERM", ShutdownReason::Terminate);
}

#[cfg(unix)]
#[test]
fn watcher_accepts_sigint_in_a_subprocess() {
    assert_signal("-INT", ShutdownReason::Interrupt);
}

#[cfg(unix)]
#[test]
fn watcher_installation_outside_a_tokio_reactor_panics() {
    let result = std::panic::catch_unwind(ShutdownWatcher::install);
    assert!(result.is_err());
}
