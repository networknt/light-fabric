use std::process::Command;
#[test]
fn stdout_schema_rust_log_and_dependency_redaction() {
    let exe = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples/logging_probe");
    for (filter, levels) in [
        (None, vec!["INFO", "WARN", "ERROR"]),
        (Some("debug"), vec!["DEBUG", "INFO", "WARN", "ERROR"]),
        (Some("error"), vec!["ERROR"]),
    ] {
        let mut cmd = Command::new(&exe);
        cmd.env_clear();
        if let Some(filter) = filter {
            cmd.env("RUST_LOG", filter);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let text = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), levels.len());
        for (line, level) in lines.iter().zip(levels) {
            let map = line.as_object().unwrap();
            assert_eq!(map.len(), 4);
            assert!(map.contains_key("timestamp"));
            assert_eq!(line["level"], level);
            assert_eq!(line["target"], "light_lambda::init");
            assert_eq!(line["fields"].as_object().unwrap().len(), 1);
            assert_eq!(
                line["fields"]["message"],
                format!("test-only-{}", level.to_lowercase())
            );
        }
    }
    let out = Command::new(exe)
        .arg("privacy")
        .env_clear()
        .env("RUST_LOG", "trace")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(!stdout.contains("test-only-secret-sentinel"));
    assert!(stdout.contains("light_security::test_probe"));
    assert!(stdout.contains("credential"));
    assert!(stdout.contains("TRACE"));
}
