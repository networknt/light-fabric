//! Qualification only, no production wiring.
#[path = "profile_support/router_rewrite.rs"]
mod rewrite;
use rewrite::Program;
use serde_json::Value;
#[test]
fn java_two_stage_replacement_characterization_and_additional_groups() {
    // Paired observations: fresh router-rewrite-characterization, not altered WP2 fixtures.
    let rows: Vec<Value> = serde_json::from_str(r###"[{"id": "literal", "pattern": "/x", "replacement": "/rewritten", "path": "/x", "result": "/rewritten", "lambdaCaptures": 0}, {"id": "capture", "pattern": "/(x)", "replacement": "/group-$1", "path": "/x", "result": "/group-x", "lambdaCaptures": 0}, {"id": "lookahead", "pattern": "/(?=x)x", "replacement": "/ahead", "path": "/x", "result": "/ahead", "lambdaCaptures": 0}, {"id": "backreference", "pattern": "/(x)\\1", "replacement": "/same", "path": "/xx", "result": "/same", "lambdaCaptures": 0}, {"id": "unicode", "pattern": "/\\p{L}+", "replacement": "/unicode", "path": "/é", "result": "/unicode", "lambdaCaptures": 0}, {"id": "escaped-replacement", "pattern": "/(x)", "replacement": "/literal-\\$1", "path": "/x", "result": "/literal-$1", "lambdaCaptures": 0}, {"id": "full-match-then-replace-all", "pattern": "/x|/xx", "replacement": "/a", "path": "/xx", "result": "/ax", "lambdaCaptures": 0}, {"id": "empty-end", "pattern": "(?:/x)*", "replacement": "/a", "path": "/x", "result": "/a/a", "lambdaCaptures": 0}, {"id": "repeat65", "pattern": "(?:/x){65}", "replacement": "/repeated", "path": "/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x/x", "result": "/repeated", "lambdaCaptures": 0}]"###).unwrap();
    let start = std::time::Instant::now();
    let bytes: usize = rows
        .iter()
        .map(|r| r["pattern"].as_str().unwrap().len() + r["replacement"].as_str().unwrap().len())
        .sum();
    for row in &rows {
        let program = Program::compile(&[(
            row["pattern"].as_str().unwrap(),
            row["replacement"].as_str().unwrap(),
        )]);
        assert_eq!(
            program.rewrite(row["path"].as_str().unwrap()).unwrap(),
            row["result"].as_str().unwrap(),
            "{}",
            row["id"]
        );
    }
    let program = Program::compile(&[
        (r"/(?<first>x)(y)", r"/${first}-$2-$0-\$1"),
        (r"/.*", "/second"),
    ]);
    assert_eq!(program.rewrite("/xy").unwrap(), "/x-y-/xy-$1");
    assert_eq!(program.rewrite("/no").unwrap(), "/second");
    let program = Program::compile(&[(r"/(?<=/)x", "/behind")]);
    assert_eq!(program.rewrite("/x").unwrap(), "/behind");
    let optional = Program::compile(&[(r"/(?<optional>x)?y", r"/${optional}-$1")]);
    assert_eq!(optional.rewrite("/y").unwrap(), "/-");
    let greedy_number = Program::compile(&[(r"/(x)", r"/$12")]);
    assert_eq!(greedy_number.rewrite("/x").unwrap(), "/x2");
    let mut rules = (0..31)
        .map(|_| ("/unmatched", "/ignored"))
        .collect::<Vec<_>>();
    rules.push(("(?:/x)*", "/a"));
    let started = std::time::Instant::now();
    let scaled = Program::compile(&rules);
    let compile_micros = started.elapsed().as_micros();
    let path = "/x".repeat(4096);
    let started = std::time::Instant::now();
    let result = scaled.rewrite(&path).unwrap();
    // Greedy repetition consumes the whole path, then matches empty at end.
    assert_eq!(result.len(), 4);
    println!(
        "ROUTER_REWRITE_SCALE {{\"rules\":32,\"inputBytes\":8192,\"resultBytes\":4,\"compileMicros\":{},\"executionMicros\":{}}}",
        compile_micros,
        started.elapsed().as_micros()
    );
    println!(
        "ROUTER_REWRITE_CANDIDATE {{\"javaCases\":{},\"sourceAndReplacementBytes\":{},\"compileAndRunMicros\":{}}}",
        rows.len(),
        bytes,
        start.elapsed().as_micros()
    );
}
