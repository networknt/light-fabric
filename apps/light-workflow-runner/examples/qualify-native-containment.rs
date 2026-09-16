//! Run inside a disposable systemd user service with Delegate=yes.
//! This qualifies kernel descendant cleanup, not Controller/VM integration.
use execution_runner_protocol::ExecutionId;
use light_workflow_runner::native_containment::NativeContainment;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let execution = ExecutionId::new();
    let containment = NativeContainment::create(execution)?;
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "read -r gate; setsid /bin/sleep 300 & wait"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    containment.attach(child.id().ok_or("child pid missing")?)?;
    child
        .stdin
        .take()
        .ok_or("child stdin missing")?
        .write_all(b"start\n")
        .await?;
    let path = std::path::Path::new("/sys/fs/cgroup")
        .join(containment.parent.trim_start_matches('/'))
        .join(format!("execution-{execution}"));
    let ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(path.join("cgroup.procs"))?
                .lines()
                .count()
                >= 2
            {
                return Ok::<_, std::io::Error>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    // Always clean the exact created group, including after readiness failure.
    containment.cleanup(execution).await?;
    child.wait().await?;
    ready??;
    assert!(
        std::fs::read_to_string(path.join("cgroup.procs"))?
            .trim()
            .is_empty()
    );
    containment.cleanup(execution).await?; // replay must not affect another group
    println!(
        "PASSED: delegated cgroup cleanup removed worker and setsid descendant; replay passed"
    );
    std::fs::remove_dir(path)?;
    Ok(())
}
