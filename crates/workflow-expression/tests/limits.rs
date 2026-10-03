use serde_json::{Value, json};
use workflow_expression::*;
mod support;
use support::{diagnostic, owner};

fn fails<T, E: Into<WorkerError>>(result: Result<T, E>) {
    match result {
        Err(e) => assert_eq!(diagnostic(e).category, Category::Limit),
        Ok(_) => panic!("expected limit"),
    }
}
fn evaluate_with(source: &str, context: &Value, limits: Limits) -> Result<Value, WorkerError> {
    let engine = owner();
    let bindings = Bindings::with_limits(context, &Value::Null, None, None, limits)?;
    evaluate(
        &compile_with_limits(&engine, source, Position::Set, limits)?,
        &bindings,
        Kind::Json,
    )
}
fn nested(depth: usize) -> Value {
    (0..depth).fold(Value::Null, |value, _| json!([value]))
}

#[test]
fn ordinary_small_compile_limits_at_and_one_over() {
    let engine = owner();
    let limits = Limits {
        source_bytes: 3,
        ..Limits::default()
    };
    compile_with_limits(&engine, "1  ", Position::Set, limits).unwrap();
    fails(compile_with_limits(&engine, "1   ", Position::Set, limits));
    let limits = Limits {
        ast_nodes: 3,
        ..Limits::default()
    };
    compile_with_limits(&engine, "[1,2]", Position::Set, limits).unwrap();
    fails(compile_with_limits(
        &engine,
        "[1,2,3]",
        Position::Set,
        limits,
    ));
    let limits = Limits {
        ast_depth: 3,
        ..Limits::default()
    };
    compile_with_limits(&engine, "[[1]]", Position::Set, limits).unwrap();
    fails(compile_with_limits(
        &engine,
        "[[[1]]]",
        Position::Set,
        limits,
    ));
    let limits = Limits {
        comprehension_depth: 1,
        ..Limits::default()
    };
    compile_with_limits(&engine, "[1].map(x,x)", Position::Set, limits).unwrap();
    fails(compile_with_limits(
        &engine,
        "[1].map(x,[1].map(y,y))",
        Position::Set,
        limits,
    ));
    let mut cache = CompileCache::new().unwrap();
    cache
        .compile(Profile::CelWorkflowV2, "[1,2]", Position::Set)
        .unwrap();
    fails(cache.compile_with_limits(
        Profile::CelWorkflowV2,
        "[1,2]",
        Position::Set,
        Limits {
            ast_nodes: 2,
            ..Limits::default()
        },
    ));
}

#[test]
fn production_compile_limits_source_ast_depth_nesting() {
    ordinary_boundary_stack("w1-production-compile", production_compile_boundaries);
}

// Compilation and cleanup share the declared environment; this is not stack/crash qualification.
fn ordinary_boundary_stack(name: &str, f: fn()) {
    std::thread::Builder::new()
        .name(name.to_owned())
        .stack_size(COMPILATION_STACK_BYTES)
        .spawn(f)
        .unwrap()
        .join()
        .unwrap();
}
fn production_compile_boundaries() {
    let engine = owner();
    let cap = Limits::default();
    let at = format!("1{}", " ".repeat(cap.source_bytes - 1));
    eprintln!("W1 compilation-boundary BEGIN source-at");
    compile(&engine, &at, Position::Set).unwrap();
    eprintln!("W1 compilation-boundary PASS source-at");
    eprintln!("W1 compilation-boundary BEGIN source-over");
    fails(compile(&engine, &(at + " "), Position::Set));
    eprintln!("W1 compilation-boundary PASS source-over");
    let make = |n| format!("[{}]", vec!["0"; n].join(","));
    eprintln!("W1 compilation-boundary BEGIN nodes-at");
    compile(&engine, &make(cap.ast_nodes - 1), Position::Set).unwrap();
    eprintln!("W1 compilation-boundary PASS nodes-at");
    eprintln!("W1 compilation-boundary BEGIN nodes-over");
    fails(compile(&engine, &make(cap.ast_nodes), Position::Set));
    eprintln!("W1 compilation-boundary PASS nodes-over");
    let make = |n: usize| format!("{}0{}", "[".repeat(n), "]".repeat(n));
    eprintln!("W1 compilation-boundary BEGIN depth-at");
    compile(&engine, &make(63), Position::Set).unwrap();
    eprintln!("W1 compilation-boundary PASS depth-at");
    eprintln!("W1 compilation-boundary BEGIN depth-over");
    fails(compile(&engine, &make(64), Position::Set));
    eprintln!("W1 compilation-boundary PASS depth-over");
    eprintln!("W1 compilation-boundary BEGIN nesting-at");
    compile(&engine, "[1].map(x,[1].map(y,x+y))", Position::Set).unwrap();
    eprintln!("W1 compilation-boundary PASS nesting-at");
    eprintln!("W1 compilation-boundary BEGIN nesting-over");
    fails(compile(
        &engine,
        "[1].map(x,[1].map(y,[1].map(z,x+y+z)))",
        Position::Set,
    ));
    eprintln!("W1 compilation-boundary PASS nesting-over");
}

#[test]
fn small_envelope_and_output_limit_boundaries() {
    let c = json!({"s":"x"});
    let envelope = json!({"context":c,"workflow":{"input":null}});
    let bytes = serde_json::to_vec(&envelope).unwrap().len();
    let limits = Limits {
        input_bytes: bytes,
        input_nodes: 5,
        input_depth: 2,
        ..Limits::default()
    };
    Bindings::with_limits(&c, &Value::Null, None, None, limits).unwrap();
    fails(Bindings::with_limits(
        &c,
        &Value::Null,
        None,
        None,
        Limits {
            input_bytes: bytes - 1,
            ..limits
        },
    ));
    fails(Bindings::with_limits(
        &c,
        &Value::Null,
        None,
        None,
        Limits {
            input_nodes: 4,
            ..limits
        },
    ));
    fails(Bindings::with_limits(
        &c,
        &Value::Null,
        None,
        None,
        Limits {
            input_depth: 1,
            ..limits
        },
    ));
    let limits = Limits {
        output_bytes: 3,
        ..Limits::default()
    };
    assert_eq!(
        evaluate_with("'x'", &json!({}), limits).unwrap(),
        json!("x")
    );
    fails(evaluate_with("'xx'", &json!({}), limits));
    let limits = Limits {
        output_nodes: 3,
        ..Limits::default()
    };
    evaluate_with("[1,2]", &json!({}), limits).unwrap();
    fails(evaluate_with("[1,2,3]", &json!({}), limits));
    let limits = Limits {
        output_depth: 2,
        ..Limits::default()
    };
    evaluate_with("[[0]]", &json!({}), limits).unwrap();
    fails(evaluate_with("[[[0]]]", &json!({}), limits));
}

#[test]
fn production_input_bytes_nodes_and_depth() {
    ordinary_boundary_stack("w1-production-input", production_input_boundaries);
}
fn production_input_boundaries() {
    let cap = Limits::default();
    let empty = json!({"s":""});
    let overhead = serde_json::to_vec(&json!({"context":empty,"workflow":{"input":null}}))
        .unwrap()
        .len();
    let at = json!({"s":"x".repeat(cap.input_bytes-overhead)});
    Bindings::new(&at, &Value::Null, None, None).unwrap();
    let over = json!({"s":"x".repeat(cap.input_bytes-overhead+1)});
    fails(Bindings::new(&over, &Value::Null, None, None));
    // envelope + context + array + workflow + null-input = five nodes, plus array members.
    let at = json!({"a":vec![Value::Null;cap.input_nodes-5]});
    Bindings::new(&at, &Value::Null, None, None).unwrap();
    let over = json!({"a":vec![Value::Null;cap.input_nodes-4]});
    fails(Bindings::new(&over, &Value::Null, None, None));
    let at = json!({"a":nested(62)});
    Bindings::new(&at, &Value::Null, None, None).unwrap();
    let over = json!({"a":nested(63)});
    fails(Bindings::new(&over, &Value::Null, None, None));
    // Optional output/value are part of the same envelope, including their field names and nodes.
    let limits = Limits {
        input_nodes: 4,
        ..cap
    };
    Bindings::with_limits(&json!({}), &Value::Null, None, None, limits).unwrap();
    fails(Bindings::with_limits(
        &json!({}),
        &Value::Null,
        Some(&Value::Null),
        None,
        limits,
    ));
}

#[test]
fn production_output_bytes_nodes_and_depth() {
    ordinary_boundary_stack("w1-production-output", production_output_boundaries);
}
fn production_output_boundaries() {
    let cap = Limits::default();
    let at = json!({"s":"x".repeat(cap.output_bytes-2)});
    evaluate_with("context.s", &at, cap).unwrap();
    let over = json!({"s":"x".repeat(cap.output_bytes-1)});
    fails(evaluate_with("context.s", &over, cap));
    // Ordinary string splitting: one validation fixture per boundary, no timing or amplification measurement.
    let at = json!({"s":"x".repeat(cap.output_nodes-1)});
    let result = evaluate_with("context.s.split('')", &at, cap).unwrap();
    assert_eq!(result.as_array().unwrap().len(), cap.output_nodes - 1);
    let over = json!({"s":"x".repeat(cap.output_nodes)});
    fails(evaluate_with("context.s.split('')", &over, cap));
    let c = json!({"a":nested(62)});
    evaluate_with("[[context.a]]", &c, cap).unwrap();
    fails(evaluate_with("[[[context.a]]]", &c, cap));
}

#[test]
fn conservative_cache_retention_eviction() {
    ordinary_boundary_stack("w1-cache-budget", cache_retention_boundaries);
}

fn cache_retention_boundaries() {
    let mut cache = CompileCache::new().unwrap();
    for i in 0..128 {
        cache
            .compile(Profile::CelWorkflowV2, &i.to_string(), Position::Set)
            .unwrap();
    }
    assert_eq!(cache.len().unwrap(), 128);
    cache
        .compile(Profile::CelWorkflowV2, "128", Position::Set)
        .unwrap();
    assert_eq!(cache.len().unwrap(), 128);
    // Establish the charge for a one-node program, then exercise the byte boundary
    // with a lowered budget. Production AST boundary coverage belongs only above.
    drop(cache); // return the process-wide reservation before a separate fixture
    let mut probe = CompileCache::new().unwrap();
    probe
        .compile(Profile::CelWorkflowV2, "1", Position::Set)
        .unwrap();
    let charge = probe.retained_bytes().unwrap();
    drop(probe);
    let budget = charge * 2;
    let mut cache = CompileCache::with_budget(8, budget).unwrap();
    for source in ["1", "2"] {
        cache
            .compile(Profile::CelWorkflowV2, source, Position::Set)
            .unwrap();
    }
    assert_eq!(
        (cache.len().unwrap(), cache.retained_bytes().unwrap()),
        (2, budget)
    );
    cache
        .compile(Profile::CelWorkflowV2, "3", Position::Set)
        .unwrap();
    assert_eq!(
        (cache.len().unwrap(), cache.retained_bytes().unwrap()),
        (2, budget)
    );
    let before = (cache.len().unwrap(), cache.retained_bytes().unwrap());
    cache
        .compile(Profile::CelWorkflowV2, "[1,2]", Position::Set)
        .unwrap();
    assert_eq!(
        (cache.len().unwrap(), cache.retained_bytes().unwrap()),
        before
    );
    let mut at = CompileCache::with_budget(8, charge).unwrap();
    at.compile(Profile::CelWorkflowV2, "1", Position::Set)
        .unwrap();
    assert_eq!(
        (at.len().unwrap(), at.retained_bytes().unwrap()),
        (1, charge)
    );
    let mut over = CompileCache::with_budget(8, charge - 1).unwrap();
    over.compile(Profile::CelWorkflowV2, "1", Position::Set)
        .unwrap();
    assert!(over.is_empty().unwrap());
    assert_eq!(over.retained_bytes().unwrap(), 0);
    for (entries, bytes) in [(0, charge), (8, 0)] {
        let mut disabled = CompileCache::with_budget(entries, bytes).unwrap();
        disabled
            .compile(Profile::CelWorkflowV2, "1", Position::Set)
            .unwrap();
        assert!(disabled.is_empty().unwrap());
    }
}

#[test]
fn deterministic_sorted_json_errors_and_recursive_kinds() {
    let engine = owner();
    let mut cache = CompileCache::new().unwrap();
    let b = Bindings::new(
        &json!({}),
        &Value::Null,
        Some(&json!({"n":3})),
        Some(&Value::Null),
    )
    .unwrap();
    let value = json!({"${key}":["literal","${ output.n }"],"z":12.75});
    let compiled = cache.json_template(&value, Position::Export).unwrap();
    assert_eq!(
        evaluate_json(&compiled, &b).unwrap(),
        json!({"${key}":["literal",3],"z":12.75})
    );
    let compiled = cache
        .json_template(&json!({"n":"${ 3 }","s":"plain"}), Position::WorkflowOutput)
        .unwrap();
    assert_eq!(
        evaluate_json(&compiled, &b).unwrap(),
        json!({"n":3,"s":"plain"})
    );
    for _ in 0..4 {
        let compiled = cache
            .json_template(&json!({"z":"${ 1/0 }","a":"${ b'x' }"}), Position::Set)
            .unwrap();
        assert_eq!(
            diagnostic(evaluate_json(&compiled, &b).unwrap_err()).category,
            Category::JsonProfile
        );
        let compiled = compile(&engine, "{'z':1/0,'a':b'x'}", Position::Set).unwrap();
        // CEL evaluation precedes conversion; a library runtime failure wins before output exists.
        assert_eq!(
            diagnostic(evaluate(&compiled, &b, Kind::Json).unwrap_err()).category,
            Category::Evaluation
        );
        let compiled = compile(&engine, "{'z':b'x','a':1.0/0.0}", Position::Set).unwrap();
        assert_eq!(
            diagnostic(evaluate(&compiled, &b, Kind::Json).unwrap_err()).category,
            Category::JsonProfile
        );
    }
}
