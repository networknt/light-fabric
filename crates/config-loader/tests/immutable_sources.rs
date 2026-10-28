use config_loader::{BoundedLocalSources, EmbeddedConfigFile, RuleBudget};
use std::fs;
struct TempDir(std::path::PathBuf);

#[test]
fn expanded_values_aliases_are_checked_before_materialization() {
    let root = TempDir::new();
    let file = root.path().join("values.yml");
    // Serialized size = scalar + key/anchor value + repeated values and commas.
    // Raw input remains tiny while alias materialization approaches one MiB.
    let scalar = "x".repeat(1024);
    for count in [1019, 1020] {
        let raw = format!("seed: &s {scalar}\nv: [{}]\n", vec!["*s"; count].join(","));
        fs::write(&file, raw).unwrap();
        let sources = BoundedLocalSources::capture(&[], None, root.path(), root.path()).unwrap();
        assert_eq!(sources.values(false).is_ok(), count == 1019);
    }
    // Numeric aliases used as keys must include string quoting in byte costs.
    fs::write(&file, "seed: &s 123\nnested: {*s: value}\n").unwrap();
    let sources = BoundedLocalSources::capture(&[], None, root.path(), root.path()).unwrap();
    assert_eq!(
        sources.values(false).unwrap()["nested"][serde_yaml::Value::Number(123.into())],
        "value"
    );
    fs::write(&file, "seed: &s [*s]\n").unwrap();
    let sources = BoundedLocalSources::capture(&[], None, root.path(), root.path()).unwrap();
    assert!(sources.values(false).is_err());
}

#[test]
fn expanded_values_aggregate_includes_unselected_documents() {
    let root = TempDir::new();
    let dirs = [
        root.path().join("d"),
        root.path().join("l"),
        root.path().join("e"),
    ];
    let raw = format!(
        "seed: &s {}\nv: [{}]\n",
        "x".repeat(1024),
        vec!["*s"; 700].join(",")
    );
    for dir in &dirs {
        fs::create_dir(dir).unwrap();
        fs::write(dir.join("values.yml"), &raw).unwrap();
    }
    let sources = BoundedLocalSources::capture(&[], Some(&dirs[0]), &dirs[1], &dirs[2]).unwrap();
    // Even bootstrap admission checks the later, otherwise unselected source.
    assert!(sources.values(true).is_err());
    assert!(sources.values(false).is_err());
}

#[test]
fn expanded_values_exact_byte_and_node_boundaries() {
    let root = TempDir::new();
    let file = root.path().join("values.yml");
    for padding in [1015, 1016] {
        let raw = format!(
            "seed: &s {}\nv: [{}]\np: {}\n",
            "x".repeat(1024),
            vec!["*s"; 1019].join(","),
            "p".repeat(padding)
        );
        fs::write(&file, raw).unwrap();
        let sources = BoundedLocalSources::capture(&[], None, root.path(), root.path()).unwrap();
        let result = sources.values(false);
        assert_eq!(result.is_ok(), padding == 1015);
        if let Ok(map) = result {
            assert_eq!(serde_json::to_vec(&map).unwrap().len(), RuleBudget::RAW);
        }
    }
    for leaves in [253, 254] {
        let raw = format!(
            "seed: &s [{}]\nv: [{}]\np: [{}]\n",
            vec!["0"; 255].join(","),
            vec!["*s"; 254].join(","),
            vec!["0"; leaves].join(",")
        );
        fs::write(&file, raw).unwrap();
        let sources = BoundedLocalSources::capture(&[], None, root.path(), root.path()).unwrap();
        assert_eq!(sources.values(false).is_ok(), leaves == 253);
    }
}
impl TempDir {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "wp12-source-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn all_precedence_combinations_and_stage_mutation_use_original_captures() {
    static EMBEDDED: &[EmbeddedConfigFile] = &[
        EmbeddedConfigFile {
            name: "values.yml",
            content: "marker: embedded\n",
        },
        EmbeddedConfigFile {
            name: "rule.yml",
            content: "marker: embedded\n",
        },
    ];
    for mask in 0..8 {
        let root = TempDir::new();
        let dirs: Vec<_> = ["default", "local", "external"]
            .map(|s| root.path().join(s))
            .into();
        for (i, dir) in dirs.iter().enumerate() {
            fs::create_dir(dir).unwrap();
            if mask & (1 << i) != 0 {
                fs::write(dir.join("values.yml"), format!("marker: source{i}\n")).unwrap();
                fs::write(dir.join("rule.yml"), format!("marker: source{i}\n")).unwrap();
            }
        }
        let sources =
            BoundedLocalSources::capture(EMBEDDED, Some(&dirs[0]), &dirs[1], &dirs[2]).unwrap();
        let expected = (0..3)
            .rev()
            .find(|i| mask & (1 << i) != 0)
            .map(|i| format!("source{i}"))
            .unwrap_or_else(|| "embedded".into());
        for dir in &dirs {
            fs::write(dir.join("values.yml"), "marker: changed\n").unwrap();
            fs::write(dir.join("rule.yml"), "[malformed").unwrap();
        }
        assert_eq!(
            sources.values(false).unwrap()["marker"].as_str(),
            Some(expected.as_str())
        );
        assert_eq!(
            sources.rule().unwrap().unwrap()["marker"].as_str(),
            Some(expected.as_str())
        );
        let bootstrap = (0..2)
            .rev()
            .find(|i| mask & (1 << i) != 0)
            .map(|i| format!("source{i}"))
            .unwrap_or_else(|| "embedded".into());
        assert_eq!(
            sources.values(true).unwrap()["marker"].as_str(),
            Some(bootstrap.as_str())
        );
    }
}

#[test]
fn raw_members_combined_capture_and_syntax_budgets_precede_values_materialization() {
    let root = TempDir::new();
    let default = root.path().join("default");
    let local = root.path().join("local");
    fs::create_dir(&default).unwrap();
    fs::create_dir(&local).unwrap();
    let exact = format!("#{}\nv: 1\n", "x".repeat(RuleBudget::RAW - 7));
    assert_eq!(exact.len(), RuleBudget::RAW);
    fs::write(default.join("values.yml"), &exact).unwrap();
    let sources = BoundedLocalSources::capture(&[], Some(&default), &local, &local).unwrap();
    assert_eq!(sources.values(false).unwrap()["v"].as_u64(), Some(1));
    fs::write(local.join("values.yml"), &exact).unwrap();
    fs::write(local.join("rule.yml"), "[]").unwrap();
    let sources = BoundedLocalSources::capture(&[], Some(&default), &local, &local).unwrap();
    assert_eq!(sources.bytes(), RuleBudget::CAPTURES);
    assert!(sources.rule().is_err());
    fs::write(local.join("values.yml"), format!("{exact} ")).unwrap();
    assert!(BoundedLocalSources::capture(&[], Some(&default), &local, &local).is_err());
    fs::remove_file(default.join("values.yml")).unwrap();
    for depth in [63, 64] {
        fs::write(
            local.join("values.yml"),
            format!("v: {}0{}", "[".repeat(depth), "]".repeat(depth)),
        )
        .unwrap();
        let sources = BoundedLocalSources::capture(&[], None, &local, &local).unwrap();
        assert_eq!(sources.values(false).is_ok(), depth == 63);
    }
}

#[test]
fn values_event_budget_exact_boundary_does_not_apply_rule_limits_to_unrelated_values() {
    let root = TempDir::new();
    // stream/doc/map/key/sequence delimiters contribute nine events.
    for count in [RuleBudget::EVENTS - 9, RuleBudget::EVENTS - 8] {
        fs::write(
            root.path().join("values.yml"),
            format!("v: [{}]", vec!["0"; count].join(",")),
        )
        .unwrap();
        let sources = BoundedLocalSources::capture(&[], None, root.path(), root.path()).unwrap();
        assert_eq!(
            sources.values(false).is_ok(),
            count == RuleBudget::EVENTS - 9
        );
    }
}
