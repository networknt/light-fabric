use serde_json::{Value, json};
use workflow_expression::*;
fn raw(task: Value) -> Value {
    json!({"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"t":task}]})
}
fn validate(raw: &Value) -> Result<(), ValidationError> {
    let engine = Engine::new(WorkerConfig {
        workers: 2,
        cache_entries: 8,
        cache_bytes: 128 * 1024,
        ..WorkerConfig::default()
    })
    .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let result = match DefinitionValidation::from_raw(raw) {
        Ok(plan) => runtime.block_on(plan.validate(&engine)),
        Err(d) => Err(ValidationError::Diagnostic(d)),
    };
    engine.shutdown(std::time::Duration::from_secs(2)).unwrap();
    result
}
#[test]
fn authored_position_inventory_checks_roots_with_field_and_span_identity() {
    let rows = vec![
        (json!({"set":{"x":"x"}}), "/do/0/t/set/x", false, false),
        (
            json!({"call":"http","with":{"body":"x"}}),
            "/do/0/t/with/body",
            false,
            false,
        ),
        (
            json!({"call":"http","with":{"query":{"x":"x"}}}),
            "/do/0/t/with/query/x",
            false,
            false,
        ),
        (
            json!({"call":"http","with":{"headers":{"x":"x"}}}),
            "/do/0/t/with/headers/x",
            false,
            false,
        ),
        (
            json!({"call":"http","with":{},"idempotencyKey":"x"}),
            "/do/0/t/idempotencyKey",
            false,
            false,
        ),
        (
            json!({"call":"jsonrpc","with":{"endpoint":{"uri":"x"}}}),
            "/do/0/t/with/endpoint/uri",
            false,
            false,
        ),
        (
            json!({"call":"jsonrpc","with":{"params":"x"}}),
            "/do/0/t/with/params",
            false,
            false,
        ),
        (
            json!({"call":"jsonrpc","with":{"headers":{"x":"x"}}}),
            "/do/0/t/with/headers/x",
            false,
            false,
        ),
        (
            json!({"call":"openrpc","with":{"document":{"endpoint":"x"}}}),
            "/do/0/t/with/document/endpoint",
            false,
            false,
        ),
        (
            json!({"call":"mcp","with":{"params":"x"}}),
            "/do/0/t/with/params",
            false,
            false,
        ),
        (
            json!({"call":"mcp","with":{"resource":"x"}}),
            "/do/0/t/with/resource",
            false,
            false,
        ),
        (
            json!({"call":"a2a","with":{"parameters":"x"}}),
            "/do/0/t/with/parameters",
            false,
            false,
        ),
        (
            json!({"call":"agent","with":{"input":"x"}}),
            "/do/0/t/with/input",
            false,
            false,
        ),
        (
            json!({"call":"agent","with":{"mockOutput":"x"}}),
            "/do/0/t/with/mockOutput",
            false,
            false,
        ),
        (
            json!({"call":"agent","with":{"instructions":"x"}}),
            "/do/0/t/with/instructions",
            false,
            false,
        ),
        (
            json!({"call":"agent","with":{"prompt":"x"}}),
            "/do/0/t/with/prompt",
            false,
            false,
        ),
        (
            json!({"ask":{"assignment":{"categoryCode":"x"}}}),
            "/do/0/t/ask/assignment/categoryCode",
            false,
            false,
        ),
        (
            json!({"ask":{"assignment":{"reasonCode":"x"}}}),
            "/do/0/t/ask/assignment/reasonCode",
            false,
            false,
        ),
        (
            json!({"ask":{"assignment":{"assigneeId":"x"}}}),
            "/do/0/t/ask/assignment/assigneeId",
            false,
            false,
        ),
        (
            json!({"ask":{"assignment":{"roleId":"x"}}}),
            "/do/0/t/ask/assignment/roleId",
            false,
            false,
        ),
        (
            json!({"switch":[{"case":{"when":"x","then":"end"}}]}),
            "/do/0/t/switch/0/case/when",
            false,
            false,
        ),
        (
            json!({"assert":{"value":"x"}}),
            "/do/0/t/assert/value",
            false,
            false,
        ),
        (
            json!({"assert":{"equals":"x"}}),
            "/do/0/t/assert/equals",
            false,
            false,
        ),
        (
            json!({"assert":{"contains":"x"}}),
            "/do/0/t/assert/contains",
            false,
            false,
        ),
        (
            json!({"assert":{"json":{"p":{"equals":"x"}}}}),
            "/do/0/t/assert/json/p/equals",
            false,
            false,
        ),
        (
            json!({"assert":{"json":{"p":{"contains":"x"}}}}),
            "/do/0/t/assert/json/p/contains",
            false,
            false,
        ),
        (
            json!({"assert":{"json":{"p":"x"}}}),
            "/do/0/t/assert/json/p",
            false,
            true,
        ),
        (
            json!({"set":{},"export":{"as":{"x":"x"}}}),
            "/do/0/t/export/as/x",
            true,
            false,
        ),
        (
            json!({"run":{"shell":{}},"export":{"as":{"x":"x"}}}),
            "/do/0/t/export/as/x",
            true,
            false,
        ),
    ];
    assert_eq!(rows.len(), 29);
    for (task, path, output, value) in rows {
        let mut definition = raw(task);
        for (source, allowed) in [
            ("context", true),
            ("workflow.input", true),
            ("output", output),
            ("value", value),
            ("private_authored_root", false),
        ] {
            *definition.pointer_mut(path).unwrap() = json!(format!("${{ {source} }}"));
            let result = validate(&definition);
            if allowed {
                assert!(result.is_ok(), "{path} {source}: {result:?}");
            } else {
                let ValidationError::Diagnostic(d) = result.unwrap_err() else {
                    panic!("availability error")
                };
                assert_eq!(d.field, path);
                assert_eq!(d.task.as_deref(), Some("t"));
                assert_eq!((d.error.span_index, d.error.offset), (Some(0), Some(2)));
                assert!(!format!("{d:?}").contains("private_authored_root"));
            }
        }
    }
    let mut definition = raw(json!({"set":{}}));
    definition["output"] = json!({"as":"${ context }"});
    assert!(validate(&definition).is_ok());
    definition["output"]["as"] = json!("${ output }");
    assert!(
        matches!(validate(&definition),Err(ValidationError::Diagnostic(d)) if d.field=="/output/as")
    );
}
#[test]
fn unsupported_raw_fields_and_switch_shapes_are_not_discarded() {
    for task in [
        json!({"set":{},"if":null}),
        json!({"set":{},"input":{}}),
        json!({"set":{},"output":{}}),
        json!({"assert":{"schema":{}}}),
        json!({"assert":{"rule":{}}}),
        json!({"call":"rule","with":{}}),
        json!({"call":"http","with":{"output":"response"}}),
        json!({"export":{"as":"${ context }"},"set":{}}),
        json!({"call":"mcp","with":{"transport":{"stdio":{}}}}),
    ] {
        assert!(
            matches!(validate(&raw(task)),Err(ValidationError::Diagnostic(d)) if d.error.category==Category::Unsupported)
        );
    }
    for switch in [
        json!([{"c":{"then":"end"}}]),
        json!([{"default":{"when":"${ true }","then":"end"}}]),
        json!([{"default":{"then":"end"}},{"default":{"then":"end"}}]),
        json!([{"default":{"then":"end"}},{"c":{"when":"${ true }","then":"end"}}]),
        json!([{"c":{"when":"${ true }"}}]),
        json!({"c":{"when":"${ true }","then":"end"}}),
    ] {
        assert!(
            matches!(validate(&raw(json!({"switch":switch}))),Err(ValidationError::Diagnostic(d)) if d.error.category==Category::Invalid)
        );
    }
    for pointer in ["/input/from", "/output/schema"] {
        let mut definition = raw(json!({"set":{}}));
        let parent = if pointer.starts_with("/input") {
            "input"
        } else {
            "output"
        };
        definition[parent] = json!({pointer.split('/').next_back().unwrap():{}});
        assert!(
            matches!(validate(&definition),Err(ValidationError::Diagnostic(d)) if d.field==pointer)
        );
    }
    let mut definition = raw(json!({"set":{}}));
    definition["evaluate"]["mode"] = Value::Null;
    assert_eq!(
        DefinitionValidation::from_raw(&definition)
            .err()
            .unwrap()
            .error
            .category,
        Category::ProfileUnsupported
    );
}
#[test]
fn uri_scope_template_forms_locals_and_deterministic_diagnostics() {
    for endpoint in ["https://host/{id}", "/path/{id}"] {
        assert!(validate(&raw(json!({"call":"http","with":{"endpoint":endpoint}}))).is_ok());
    }
    for endpoint in [
        "https://{id}/path",
        "/path?x={id}",
        "/{bad.name}",
        "/{bad-name}",
        "/${ context.uri }",
    ] {
        assert!(matches!(
            validate(&raw(json!({"call":"http","with":{"endpoint":endpoint}}))),
            Err(ValidationError::Diagnostic(_))
        ));
    }
    assert!(validate(&raw(json!({"call":"jsonrpc","with":{"endpoint":"https://${ context.host }/${ context.path }"}}))).is_ok());
    assert!(validate(&raw(json!({"call":"openrpc","with":{"document":{"endpoint":"https://${ context.host }/doc"},"server":{"url":"${ context.uri }"}}}))).is_ok());
    assert!(validate(&raw(json!({"set":{"x":"${ [1].map(output,output) }"}}))).is_ok());
    for _ in 0..3 {
        let e = validate(&raw(
            json!({"set":{"z":"${ forbidden }","a":"x${ context.ok }/${ forbidden }"}}),
        ))
        .unwrap_err();
        let ValidationError::Diagnostic(d) = e else {
            panic!()
        };
        assert_eq!(d.field, "/do/0/t/set/a");
        assert_eq!(d.error.span_index, Some(1));
        assert_eq!(d.error.offset, Some(19));
    }
    for task in [
        json!({"switch":[{"c":{"when":"plain","then":"end"}}]}),
        json!({"set":{},"export":{"as":{"x":"plain"}}}),
        json!({"call":"http","with":{"headers":{"x":1}}}),
    ] {
        assert!(validate(&raw(task)).is_err());
    }
    assert!(validate(&raw(json!({"set":{"x":"$${ literal","y":"literal"}}))).is_ok());
    assert!(validate(&raw(json!({"set":{"x":"${{ context }}"}}))).is_err());
}
