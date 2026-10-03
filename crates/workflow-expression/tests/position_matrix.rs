use serde_json::json;
use workflow_expression::*;
mod support;
use support::{diagnostic, owner};

// E03 production contract §§2.1–2.4, 3.4–3.5 and 7.1.
// Expectations are authored from the contract, never Position::kind()/allows().
const MATRIX: [(Position, Kind, bool, bool, bool); 30] = [
    (Position::Set, Kind::Json, false, false, false),
    (Position::HttpBody, Kind::Json, false, false, false),
    (Position::HttpQuery, Kind::String, false, false, false),
    (Position::HttpHeader, Kind::String, false, false, false),
    (Position::IdempotencyKey, Kind::String, false, false, false),
    (Position::JsonRpcUri, Kind::String, false, false, false),
    (Position::JsonRpcParams, Kind::Json, false, false, false),
    (Position::JsonRpcHeader, Kind::Json, false, false, false),
    (Position::OpenRpcEndpoint, Kind::String, false, false, false),
    (Position::McpParams, Kind::Json, false, false, false),
    (Position::McpResourceUri, Kind::String, false, false, false),
    (Position::A2aParameters, Kind::Json, false, false, false),
    (Position::AgentInput, Kind::Json, false, false, false),
    (Position::AgentMockOutput, Kind::Json, false, false, false),
    (
        Position::AgentInstructions,
        Kind::String,
        false,
        false,
        false,
    ),
    (Position::AgentPrompt, Kind::String, false, false, false),
    (Position::AskCategory, Kind::String, false, false, false),
    (Position::AskReason, Kind::String, false, false, false),
    (Position::AskAssignee, Kind::String, false, false, false),
    (Position::AskRole, Kind::String, false, false, false),
    (Position::SwitchWhen, Kind::Predicate, false, false, true),
    (Position::AssertValue, Kind::Json, false, false, false),
    (Position::AssertEquals, Kind::Json, false, false, false),
    (Position::AssertContains, Kind::Json, false, false, false),
    (Position::AssertJsonEquals, Kind::Json, false, false, false),
    (
        Position::AssertJsonContains,
        Kind::Json,
        false,
        false,
        false,
    ),
    (
        Position::AssertJsonPredicate,
        Kind::Predicate,
        false,
        true,
        true,
    ),
    (Position::Export, Kind::Export, true, false, true),
    (Position::RunnerExport, Kind::Export, true, false, true),
    (Position::WorkflowOutput, Kind::Json, false, false, true),
];

#[test]
fn contract_position_matrix() {
    std::thread::Builder::new()
        .name("w1-position-matrix".into())
        .stack_size(COMPILATION_STACK_BYTES)
        .spawn(check_matrix)
        .unwrap()
        .join()
        .unwrap();
}

fn check_matrix() {
    let engine = owner();
    let bindings = Bindings::new(
        &json!({"ok": true}),
        &json!({"ok": true}),
        Some(&json!({"ok": true})),
        Some(&json!({"ok": true})),
    )
    .unwrap();
    let mut cache = CompileCache::new().unwrap();
    for (source, seed) in [
        ("context.ok", Position::Set),
        ("workflow.input.ok", Position::Set),
        ("output.ok", Position::Export),
        ("value.ok", Position::AssertJsonPredicate),
        ("'text'", Position::Set),
        ("true", Position::Set),
        ("3", Position::Set),
        ("{'ok':true}", Position::Set),
    ] {
        cache.compile(Profile::CelWorkflowV2, source, seed).unwrap();
        cache
            .compile(Profile::CelWorkflowV2, &format!(" {source} "), seed)
            .unwrap();
    }
    let retained = (cache.len().unwrap(), cache.retained_bytes().unwrap());
    for (position, kind, output, value, whole_required) in MATRIX {
        for (source, permitted) in [
            ("context.ok", true),
            ("workflow.input.ok", true),
            ("output.ok", output),
            ("value.ok", value),
        ] {
            // Fresh parse and exact-source cache hit must make the same decision.
            for result in [
                compile(&engine, source, position),
                cache.compile(Profile::CelWorkflowV2, source, position),
            ] {
                if permitted {
                    // Root admission is independent of the result-kind checks below.
                    drop(result.unwrap());
                } else {
                    assert_eq!(
                        diagnostic(result.err().unwrap()).category,
                        Category::Invalid,
                        "{position:?}: {source}"
                    );
                }
            }
        }
        assert_eq!(
            diagnostic(compile(&engine, "ok", position).err().unwrap()).category,
            Category::Invalid
        );
        for (source, expected, string_result, bool_result, object_result) in [
            ("'text'", json!("text"), true, false, false),
            ("true", json!(true), false, true, false),
            ("3", json!(3), false, false, false),
            ("{'ok':true}", json!({"ok":true}), false, false, true),
        ] {
            // Template compilation reuses seeded programs but must retain this position's kind.
            let template = cache
                .json_template(&json!(format!("${{ {source} }}")), position)
                .unwrap();
            let result = evaluate_json(&template, &bindings);
            let accepted = if position == Position::WorkflowOutput {
                object_result
            } else {
                match kind {
                    Kind::Json | Kind::Export => true,
                    Kind::String => string_result,
                    Kind::Predicate => bool_result,
                }
            };
            if accepted {
                assert_eq!(result.unwrap(), expected, "{position:?}: {source}");
            } else {
                assert_eq!(
                    diagnostic(result.unwrap_err()).category,
                    Category::ResultType,
                    "{position:?}: {source}"
                );
            }
        }
        let literal = cache.template("literal", position);
        let embedded = cache.template("prefix${'text'}", position);
        if whole_required {
            assert_eq!(
                diagnostic(literal.err().unwrap()).category,
                Category::Invalid
            );
            assert_eq!(
                diagnostic(embedded.err().unwrap()).category,
                Category::Invalid
            );
        } else {
            assert_eq!(
                evaluate_template(&literal.unwrap(), &bindings).unwrap(),
                json!("literal")
            );
            assert_eq!(
                evaluate_template(&embedded.unwrap(), &bindings).unwrap(),
                json!("prefixtext")
            );
        }
        assert_eq!(
            (cache.len().unwrap(), cache.retained_bytes().unwrap()),
            retained
        );
    }
}
