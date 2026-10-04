use serde_json::{Value, json};
use workflow_expression::*;
mod support;
use support::{diagnostic, owner};

// These two tests each reserve the entire process-wide cache budget. Their
// cache contracts are independent; run them one at a time and drain before
// releasing the guard so the next reservation cannot race worker teardown.
static FULL_CACHE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn bindings(context: Value) -> Bindings {
    Bindings::new(
        &context,
        &json!({}),
        Some(&json!({"ok": 1})),
        Some(&json!(3)),
    )
    .unwrap()
}
fn run(source: &str) -> Result<Value, WorkerError> {
    let engine = owner();
    evaluate(
        &compile(&engine, source, Position::Set)?,
        &bindings(json!({})),
        Kind::Json,
    )
}
fn category<T, E: Into<WorkerError>>(result: Result<T, E>, expected: Category) {
    match result {
        Err(e) => {
            let e = diagnostic(e);
            assert_eq!(e.category, expected, "{e}")
        }
        Ok(_) => panic!("expected {}", expected.code()),
    }
}

#[test]
fn raw_selector_matrix() {
    for raw in [
        json!({}),
        json!({"evaluate":{"language":"jq","mode":"strict"}}),
        json!({"document":{"metadata":{}}}),
    ] {
        assert_eq!(resolve_profile(&raw).unwrap(), Profile::LegacyV1);
    }
    let base = json!({"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"}});
    assert_eq!(resolve_profile(&base).unwrap(), Profile::CelWorkflowV2);
    for selector in [
        json!(null),
        json!(1),
        json!(false),
        json!([]),
        json!({}),
        json!(""),
        json!("cel-workflow-v1"),
        json!("cel-workflow-v3"),
    ] {
        let mut raw = base.clone();
        raw["document"]["metadata"]["lightExpressionProfile"] = selector;
        category(resolve_profile(&raw), Category::ProfileUnsupported);
    }
    for evaluate in [
        json!(null),
        json!({}),
        json!({"language":"jq"}),
        json!({"language":1}),
        json!({"language":"cel","mode":null}),
        json!({"language":"cel","mode":""}),
    ] {
        let mut raw = base.clone();
        raw["evaluate"] = evaluate;
        category(resolve_profile(&raw), Category::ProfileUnsupported);
    }
    let mut raw = base;
    raw.as_object_mut().unwrap().remove("evaluate");
    category(resolve_profile(&raw), Category::ProfileUnsupported);
}

#[test]
fn scanner_strings_brackets_escapes_and_coordinates() {
    for source in [
        "'a}b'",
        "\"a}b\"",
        "'''a}b'''",
        "\"\"\"a}b\"\"\"",
        "r'a}b'",
        "R\"a}b\"",
        "b'a}b'",
        "B\"a}b\"",
        "br'a}b'",
        "rb'''a}b'''",
        "'a\\\'}b'",
        "{'a': [1, (2)]}",
        "1 // }\n",
    ] {
        let template = format!("${{ {source} }}");
        let parts = scan(&template).unwrap();
        assert!(
            matches!(parts.as_slice(), [Segment::Span { offset: 0, index: 0, end, .. }] if *end == template.len()),
            "{template}"
        );
    }
    let parts = scan("é$${x}-${ 1 }/${ 2 }").unwrap();
    assert_eq!(
        parts[0],
        Segment::Literal {
            text: "é${x}-".into(),
            offset: 0,
            end: 8
        }
    );
    assert!(
        matches!(&parts[1], Segment::Span { offset:8, source_offset:10, index:0, source, .. } if source == " 1 ")
    );
    assert!(matches!(&parts[3], Segment::Span { index: 1, .. }));
    for bad in [
        "${",
        "${ }",
        "${ [1) }",
        "${ (1] }",
        "${ 'x }",
        "${ r'''x }",
        "${ {'a':1} ",
    ] {
        category(scan(bad), Category::Invalid);
    }
    category(scan("prefix${{ 1 }}"), Category::Unsupported);
    assert_eq!(
        scan("$ and } and $${ literal").unwrap(),
        vec![Segment::Literal {
            text: "$ and } and ${ literal".into(),
            offset: 0,
            end: 23
        }]
    );
}

#[test]
fn numeric_conversion_rows_global_and_member_dispatch() {
    let accepted = [
        ("int", "0", json!(0)),
        ("int", "9223372036854775807", json!(i64::MAX)),
        ("int", "-9223372036854775808", json!(i64::MIN)),
        ("int", "9223372036854775807u", json!(i64::MAX)),
        ("int", "0u", json!(0)),
        ("int", "-1.9", json!(-1)),
        ("int", "-9223372036854775808.0", json!(i64::MIN)),
        (
            "int",
            "9223372036854774784.0",
            json!(9_223_372_036_854_774_784i64),
        ),
        ("int", "'-0001'", json!(-1)),
        ("int", "'9223372036854775807'", json!(i64::MAX)),
        ("int", "'-9223372036854775808'", json!(i64::MIN)),
        ("uint", "18446744073709551615u", json!(u64::MAX)),
        ("uint", "0u", json!(0)),
        ("uint", "9223372036854775807", json!(i64::MAX)),
        ("uint", "0", json!(0)),
        ("uint", "1.9", json!(1)),
        ("uint", "-0.0", json!(0)),
        (
            "uint",
            "18446744073709549568.0",
            json!(18_446_744_073_709_549_568u64),
        ),
        ("uint", "'18446744073709551615'", json!(u64::MAX)),
        ("uint", "'0001'", json!(1)),
        ("double", "3", json!(3.0)),
        ("double", "9007199254740993", json!(9007199254740992.0)),
        (
            "double",
            "18446744073709551615u",
            json!(18446744073709551616.0),
        ),
        ("double", "12.75", json!(12.75)),
        ("double", "'12.75'", json!(12.75)),
        ("double", "'1e2'", json!(100.0)),
        ("double", "'-0.0'", json!(-0.0)),
    ];
    for (function, argument, expected) in accepted {
        for source in [
            format!("{function}({argument})"),
            format!("({argument}).{function}()"),
        ] {
            assert_eq!(run(&source).unwrap(), expected, "{source}");
        }
    }
    let rejected = [
        ("int", "18446744073709551615u"),
        ("int", "9223372036854775808u"),
        ("int", "1e300"),
        ("int", "9.223372036854775807e18"),
        ("int", "-9223372036854777856.0"),
        ("int", "0.0/0.0"),
        ("int", "1.0/0.0"),
        ("int", "'9223372036854775808'"),
        ("int", "'-9223372036854775809'"),
        ("int", "'+1'"),
        ("int", "' 1'"),
        ("int", "'1 '"),
        ("int", "''"),
        ("int", "'1.0'"),
        ("uint", "-1"),
        ("uint", "-0.1"),
        ("uint", "18446744073709551616.0"),
        ("uint", "0.0/0.0"),
        ("uint", "1.0/0.0"),
        ("uint", "'18446744073709551616'"),
        ("uint", "'-0'"),
        ("uint", "'+1'"),
        ("uint", "' 1'"),
        ("uint", "''"),
        ("double", "'1e400'"),
        ("double", "'NaN'"),
        ("double", "'inf'"),
        ("double", "'infinity'"),
        ("double", "'0x1'"),
        ("double", "'+1'"),
        ("double", "' 1'"),
        ("double", "'1 '"),
        ("double", "''"),
        ("double", "'01'"),
        ("double", "'1.'"),
        ("double", "'.1'"),
        ("double", "1.0/0.0"),
    ];
    for (function, argument) in rejected {
        for source in [
            format!("{function}({argument})"),
            format!("({argument}).{function}()"),
        ] {
            category(run(&source), Category::Evaluation);
        }
    }
    for function in ["int", "uint", "double"] {
        for argument in ["true", "null", "[]", "{}", "b'1'"] {
            category(
                run(&format!("{function}({argument})")),
                Category::Evaluation,
            );
            category(
                run(&format!("({argument}).{function}()")),
                Category::Evaluation,
            );
        }
        for source in [
            format!("{function}()"),
            format!("{function}(1,2)"),
            format!("(1).{function}(2)"),
        ] {
            category(run(&source), Category::Evaluation);
        }
    }
}

#[test]
fn size_every_overload_global_member_and_rejections() {
    for (argument, expected) in [
        ("'é🙂'", 2),
        ("b'abc'", 3),
        ("bytes('é🙂')", 6),
        ("[1,2]", 2),
        ("{'a':1}", 1),
        ("''", 0),
    ] {
        assert_eq!(run(&format!("size({argument})")).unwrap(), json!(expected));
        assert_eq!(
            run(&format!("({argument}).size()")).unwrap(),
            json!(expected)
        );
    }
    for arg in ["1", "1u", "1.0", "true", "null"] {
        category(run(&format!("size({arg})")), Category::Evaluation);
        category(run(&format!("({arg}).size()")), Category::Evaluation);
    }
    for source in ["size()", "size('x',1)", "'x'.size(1)"] {
        category(run(source), Category::Evaluation);
    }
}

#[test]
fn arithmetic_and_production_output_policy() {
    let engine = owner();
    for (source, expected) in [
        ("1/3", json!(0)),
        ("-7/2", json!(-3)),
        ("-7%2", json!(-1)),
        ("0.1+0.2", json!(0.30000000000000004)),
        ("9223372036854775807", json!(i64::MAX)),
        ("18446744073709551615u", json!(u64::MAX)),
        ("-0.0", json!(-0.0)),
    ] {
        assert_eq!(run(source).unwrap(), expected);
    }
    for source in [
        "9223372036854775807+1",
        "(-9223372036854775808)/-1",
        "18446744073709551615u+1u",
        "1/0",
        "1%0",
        "1+1.5",
        "1+1u",
    ] {
        category(run(source), Category::Evaluation);
    }
    for source in [
        "1.0/0.0",
        "0.0/0.0",
        "-1.0/0.0",
        "[1.0/0.0]",
        "{'x':0.0/0.0}",
        "b'x'",
        "{1:'x'}",
        "{'x':{1:'y'}}",
    ] {
        category(run(source), Category::JsonProfile);
    }
    let context: Value = serde_json::from_str(r#"{"price":12.75,"score":0.93,"id":9007199254740993,"imax":9223372036854775807,"umax":18446744073709551615,"exponent":1e2,"beyond":18446744073709551616}"#).unwrap();
    let b = bindings(context.clone());
    for position in [Position::Set, Position::Export, Position::WorkflowOutput] {
        assert_eq!(
            evaluate(
                &compile(&engine, "context", position).unwrap(),
                &b,
                position.kind()
            )
            .unwrap(),
            context
        );
    }
    for key in ["price", "score", "id", "imax", "umax", "exponent", "beyond"] {
        assert_eq!(
            evaluate(
                &compile(&engine, &format!("context.{key}"), Position::Set).unwrap(),
                &b,
                Kind::Json
            )
            .unwrap(),
            context[key]
        );
    }
    assert_eq!(
        evaluate(
            &compile(&engine, "double(context.id)*0.5", Position::Set).unwrap(),
            &b,
            Kind::Json
        )
        .unwrap(),
        json!(4503599627370496.0)
    );
    assert_eq!(
        serde_json::to_string(&run("-0.0").unwrap()).unwrap(),
        "-0.0"
    );
}

#[test]
fn extensions_and_admitted_language() {
    let engine = owner();
    for (source, expected) in [
        ("'é🙂x'.substring(1,2)", json!("🙂")),
        ("'é🙂x'.substring(2)", json!("x")),
        ("'é🙂x'.indexOf('x')", json!(2)),
        ("'é🙂x'.indexOf('é',1)", json!(-1)),
        ("'é🙂'.indexOf('',2)", json!(2)),
        ("'é🙂'.split('')", json!(["é", "🙂"])),
        ("'a,b,c'.split(',',2)", json!(["a", "b,c"])),
        ("'a,b'.split(',',0)", json!([])),
        ("'a,b'.split(',',-1)", json!(["a", "b"])),
        ("['a','b'].join('-')", json!("a-b")),
        ("['a','b'].join()", json!("ab")),
        (
            "jsonEncode({'z':12.75,'a':18446744073709551615u})",
            json!("{\"a\":18446744073709551615,\"z\":12.75}"),
        ),
        ("[1,2].map(x,x*2)", json!([2, 4])),
        ("[1,2].filter(x,x>1)", json!([2])),
        ("[1,2].all(x,x>0)", json!(true)),
        ("[1,2].exists(x,x==2)", json!(true)),
        ("[1,2].exists_one(x,x==2)", json!(true)),
        ("has({'a':1}.a)", json!(true)),
        ("'abc'.contains('b')", json!(true)),
        ("'abc'.startsWith('a')", json!(true)),
        ("'abc'.endsWith('c')", json!(true)),
        ("[1,2].contains(2)", json!(true)),
        ("{'x':1}.contains('x')", json!(true)),
        ("string(12)", json!("12")),
        ("string(bytes('é'))", json!("é")),
    ] {
        assert_eq!(run(source).unwrap(), expected, "{source}");
    }
    for source in [
        "'abc'.substring(-1)",
        "'abc'.substring(2,1)",
        "'abc'.substring(4)",
        "'abc'.indexOf('x',4)",
        "['a',1].join()",
        "'x'.split(1)",
    ] {
        category(run(source), Category::Evaluation);
    }
    for source in [
        "'a'.matches('a')",
        "timestamp('x')",
        "duration('1s')",
        "type(1)",
        "dyn(1)",
        "unknown(1)",
        "[1].sort()",
    ] {
        category(
            compile(&engine, source, Position::Set),
            Category::Unsupported,
        );
    }
    for source in ["[1].map(x)", "[1].filter(x)"] {
        category(compile(&engine, source, Position::Set), Category::Invalid);
    }
}

#[test]
fn binding_roots_lexical_locals_and_program_only_cache() {
    let _cache_guard = FULL_CACHE_TEST.lock().unwrap();
    let engine = owner();
    for position in [
        Position::Set,
        Position::SwitchWhen,
        Position::HttpBody,
        Position::WorkflowOutput,
    ] {
        for source in ["output", "value", "issue", "x"] {
            category(compile(&engine, source, position), Category::Invalid);
        }
        compile(&engine, "context", position).unwrap();
        compile(&engine, "workflow.input", position).unwrap();
    }
    compile(&engine, "output", Position::Export).unwrap();
    compile(&engine, "value", Position::AssertJsonPredicate).unwrap();
    for source in [
        "[1].map(output,output)",
        "[1].map(x,[2].map(x,x))",
        "[1].map(x,x)+[2].map(x,x)",
    ] {
        compile(&engine, source, Position::Set).unwrap();
    }
    for source in [
        "[1].map(x,x)+x",
        "[1].map(x,[x].map(y,y))+y",
        "[output].map(output,output)",
    ] {
        category(compile(&engine, source, Position::Set), Category::Invalid);
    }
    let mut cache = CompileCache::new().unwrap();
    cache
        .compile(Profile::CelWorkflowV2, "output.ok", Position::Export)
        .unwrap();
    assert_eq!(cache.len().unwrap(), 1);
    for p in [Position::Set, Position::SwitchWhen] {
        category(
            cache.compile(Profile::CelWorkflowV2, "output.ok", p),
            Category::Invalid,
        );
    }
    cache
        .compile(Profile::CelWorkflowV2, "true", Position::Set)
        .unwrap();
    let pred = cache
        .compile(Profile::CelWorkflowV2, "true", Position::SwitchWhen)
        .unwrap();
    assert_eq!(
        evaluate(&pred, &bindings(json!({})), Kind::Predicate).unwrap(),
        json!(true)
    );
    category(
        cache.template("true", Position::SwitchWhen),
        Category::Invalid,
    );
    category(
        cache.compile(Profile::LegacyV1, "true", Position::Set),
        Category::ProfileUnsupported,
    );
    cache
        .compile(Profile::CelWorkflowV2, " true ", Position::Set)
        .unwrap();
    assert_eq!(cache.len().unwrap(), 3);
    for i in 0..140 {
        cache
            .compile(Profile::CelWorkflowV2, &i.to_string(), Position::Set)
            .unwrap();
    }
    assert_eq!(cache.len().unwrap(), 128);
    assert!(cache.retained_bytes().unwrap() <= CompileCache::MAX_BYTES);
    cache.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn template_kinds_and_sanitized_coordinates() {
    let _cache_guard = FULL_CACHE_TEST.lock().unwrap();
    let engine = owner();
    let mut cache = CompileCache::new().unwrap();
    let b = bindings(json!({"s":"sentinel-private-data", "n":3}));
    for (source, position, expected) in [
        ("${ context.n }", Position::Set, json!(3)),
        (
            "x${ context.s }",
            Position::Set,
            json!("xsentinel-private-data"),
        ),
        ("$${ x }", Position::Set, json!("${ x }")),
        ("plain", Position::HttpHeader, json!("plain")),
        ("${ true }", Position::SwitchWhen, json!(true)),
    ] {
        assert_eq!(
            evaluate_template(&cache.template(source, position).unwrap(), &b).unwrap(),
            expected
        );
    }
    for (source, p) in [
        ("literal", Position::Export),
        (" ${ true }", Position::SwitchWhen),
        ("x${true}", Position::SwitchWhen),
        (".output", Position::WorkflowOutput),
    ] {
        category(cache.template(source, p), Category::Invalid);
    }
    for (source, p) in [
        ("x${ context.n }", Position::Set),
        ("${3}", Position::HttpHeader),
        ("${3}", Position::SwitchWhen),
        ("${3}", Position::WorkflowOutput),
    ] {
        category(
            evaluate_template(&cache.template(source, p).unwrap(), &b),
            Category::ResultType,
        );
    }
    let err = diagnostic(
        evaluate_template(
            &cache
                .template("x${ context.s + 1 }", Position::Set)
                .unwrap(),
            &b,
        )
        .unwrap_err(),
    );
    assert_eq!(err.span_index, Some(0));
    assert_eq!(err.offset, Some(3));
    assert!(!format!("{err} {err:?}").contains("sentinel-private-data"));
    let err = compile(&engine, "private_authored_unknown", Position::Set)
        .err()
        .unwrap();
    assert!(!format!("{err} {err:?}").contains("private_authored_unknown"));
    cache.shutdown(std::time::Duration::from_secs(2)).unwrap();
}

#[test]
fn http_path_placeholders() {
    let c = json!({"s":"a/b?#%; é${x}{y}","n":u64::MAX,"dot":".","dots":"..","empty":"","bool":true,"float":1.0,"nil":null,"list":[],"obj":{}});
    assert_eq!(
        encode_path_placeholders("https://host/base/{s}/{n}?x=1#f", &c).unwrap(),
        "https://host/base/a%2Fb%3F%23%25%3B%20%C3%A9%24%7Bx%7D%7By%7D/18446744073709551615?x=1#f"
    );
    assert_eq!(
        encode_path_placeholders("/base/{n}", &c).unwrap(),
        "/base/18446744073709551615"
    );
    assert_eq!(
        encode_path_placeholders("https://host/%2F", &c).unwrap(),
        "https://host/%2F"
    );
    for uri in [
        "https://{s}/path",
        "h{n}://host/path",
        "https://host/path?q={s}",
        "https://host/path#{s}",
        "https://host/${ context.s }",
        "/{bad-name}",
        "/{bad.name}",
        "/{}",
        "/{1bad}",
        "/{s",
        "/s}",
        "/{missing}",
        "/{dot}",
        "/{dots}",
        "/{empty}",
        "/{bool}",
        "/{float}",
        "/{nil}",
        "/{list}",
        "/{obj}",
    ] {
        assert!(encode_path_placeholders(uri, &c).is_err(), "{uri}");
    }
    assert_eq!(
        encode_path_placeholders("/{s}", &json!({"s":"%2E"})).unwrap(),
        "/%252E"
    );
}
