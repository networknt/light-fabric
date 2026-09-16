//! Fixed document checks over already verified retained bytes. The published
//! definition selects checks; neither a model result nor caller input can do so.
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use task_workspace::SnapshotPackage;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
enum Check {
    DocumentLines {
        repository: String,
        path: String,
        #[serde(rename = "requiredLines")]
        required_lines: Vec<String>,
    },
}

pub(crate) fn evaluate(
    definition: &Value,
    package: &SnapshotPackage,
) -> Result<Option<BTreeMap<String, bool>>, &'static str> {
    let Some(raw) = definition.pointer("/document/metadata/developmentWorkflowDocumentChecks")
    else {
        return Ok(None);
    };
    let checks: BTreeMap<String, Check> =
        serde_json::from_value(raw.clone()).map_err(|_| "invalid pinned document checks")?;
    if checks.is_empty() || checks.len() > 32 {
        return Err("document check count outside bounds");
    }
    let mut results = BTreeMap::new();
    for (name, check) in checks {
        if name.is_empty() || name.len() > 128 {
            return Err("invalid document check name");
        }
        let Check::DocumentLines {
            repository,
            path,
            required_lines,
        } = check;
        if repository.is_empty()
            || path.is_empty()
            || path.len() > 1024
            || path.starts_with('/')
            || path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            || required_lines.is_empty()
            || required_lines.len() > 64
            || required_lines
                .iter()
                .any(|line| line.is_empty() || line.len() > 1024 || line.contains(['\n', '\r']))
        {
            return Err("invalid pinned document check parameters");
        }
        let passed = package
            .repositories
            .get(&repository)
            .and_then(|repo| repo.files.get(&path))
            .filter(|file| file.bytes.len() <= 1024 * 1024)
            .and_then(|file| std::str::from_utf8(&file.bytes).ok())
            .is_some_and(|contents| {
                required_lines
                    .iter()
                    .all(|line| contents.lines().any(|actual| actual == line))
            });
        results.insert(name, passed);
    }
    Ok(Some(results))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package() -> SnapshotPackage {
        serde_json::from_value(json!({"schemaVersion":1,"workspaceId":"w","taskId":"t",
        "featureId":"f","stageId":"s","snapshotId":"p",
        "checkpoint":{"digest":"unused","repositories":[]},
        "repositories":{"repo":{"baseCommit":"unused","tree":"unused","files":{
            "design.md":{"executable":false,"bytes":b"# Design\n## Acceptance\n".to_vec()}
        }}}}))
        .unwrap()
    }

    #[test]
    fn fixed_document_checks_use_exact_retained_lines() {
        let definition = json!({"document":{"metadata":{"developmentWorkflowDocumentChecks":{
            "design-shape":{"kind":"document-lines","repository":"repo","path":"design.md",
                "requiredLines":["# Design","## Acceptance"]}
        }}}});
        let mut package = package();
        assert_eq!(
            evaluate(&definition, &package).unwrap().unwrap()["design-shape"],
            true
        );
        package
            .repositories
            .get_mut("repo")
            .unwrap()
            .files
            .get_mut("design.md")
            .unwrap()
            .bytes = b"# Design\nmentions ## Acceptance but lacks the heading".to_vec();
        assert_eq!(
            evaluate(&definition, &package).unwrap().unwrap()["design-shape"],
            false
        );
        package
            .repositories
            .get_mut("repo")
            .unwrap()
            .files
            .get_mut("design.md")
            .unwrap()
            .bytes = vec![255];
        assert_eq!(
            evaluate(&definition, &package).unwrap().unwrap()["design-shape"],
            false
        );
        package.repositories.clear();
        assert_eq!(
            evaluate(&definition, &package).unwrap().unwrap()["design-shape"],
            false
        );
    }

    #[test]
    fn absent_checks_are_not_fabricated_and_bad_policies_fail() {
        assert!(evaluate(&json!({}), &package()).unwrap().is_none());
        for checks in [
            json!({}),
            json!({"x":{"kind":"shell","command":"true"}}),
            json!({"x":{"kind":"document-lines","repository":"repo","path":"../design.md","requiredLines":["# Design"]}}),
            json!({"x":{"kind":"document-lines","repository":"repo","path":"design.md","requiredLines":[]}}),
        ] {
            assert!(
                evaluate(
                    &json!({"document":{"metadata":{"developmentWorkflowDocumentChecks":checks}}}),
                    &package()
                )
                .is_err()
            );
        }
    }
}
