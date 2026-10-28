//! Observed allocations, not heap/RSS ceilings. Isolated test executable.
use light_lambda_runtime::transformer_program::{self, Candidate};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
struct Observed;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Observed {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(n, Ordering::Relaxed);
        unsafe { System.realloc(p, l, n) }
    }
}
#[global_allocator]
static GLOBAL: Observed = Observed;
fn snapshot() -> (usize, usize) {
    (
        ALLOCATIONS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
    )
}
#[test]
fn maximum_rule_action_totals_compile_once_then_reuse_fixed_cache_keys() {
    let mut bodies = serde_json::Map::new();
    let names = (0..32).map(|i| format!("r{i}")).collect::<Vec<_>>();
    for name in &names {
        bodies.insert(name.clone(),json!({"ruleId":name,"ruleName":name,"ruleType":"req-tra","common":"Y","conditionLanguage":"cel","conditionSecurityProfile":"standard","expression":"true","actions":(0..4).map(|n|json!({"actionId":format!("a{n}"),"actionRef":"request-body-literal","actionValues":{"body":"x".repeat(200)}})).collect::<Vec<_>>()}));
    }
    let raw = json!({"endpointRules":{"/x@post":{"req-tra":names}},"ruleBodies":bodies});
    let before = snapshot();
    let start = Instant::now();
    let program = Arc::new(Candidate::compile(&raw).unwrap());
    let compile_micros = start.elapsed().as_micros();
    let after = snapshot();
    transformer_program::aggregate(&[(&program, "req-tra")]).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let result=runtime.block_on(async{let program=program.clone();tokio::task::spawn_blocking(move||{let mut measurements=Vec::new();for _ in 0..3{let before=snapshot();let start=Instant::now();let output=program.execute(json!({"requestBody":"before","requestPath":"/x","requestHeaders":{}}),"/x@post","req-tra").unwrap();assert_eq!(output["requestBody"],"x".repeat(200));let after=snapshot();measurements.push(json!({"elapsedMicros":start.elapsed().as_micros(),"allocationCalls":after.0-before.0,"grossRequestedBytes":after.1-before.1}));}measurements}).await.unwrap()});
    let transport=runtime.block_on(async{
        use light_lambda_runtime::transformer_adapter::TransformerAdapter;
        use serverless_core::*;
        let mut headers=http::HeaderMap::new();for _ in 0..4096{headers.append("x-many",http::HeaderValue::from_static("entry"));}
        let now=tokio::time::Instant::now();let mut inv=Invocation{method:http::Method::POST,raw_path:"/x".into(),path:"/x".into(),raw_query:None,query:MultiMap::default(),headers,body:Some(bytes::Bytes::from_static(b"before")),body_was_base64:false,source:SourceMetadata::default(),request_id:"test-only".into(),deadline:now+std::time::Duration::from_secs(2),response_deadline:now+std::time::Duration::from_secs(3),cancel:tokio_util::sync::CancellationToken::new(),path_params:Default::default(),route:None,original:serde_json::Value::Null};
        let wire=serde_json::to_vec(&json!({"multiValueHeaders":{"x-many":vec!["entry";4096]}})).unwrap().len();assert!(wire<1048576);
        let adapter=TransformerAdapter::new(program.clone(),vec!["/x".into()]);let state=ExchangeState::default();let before=snapshot();let start=Instant::now();adapter.request(&mut inv,&state).await.unwrap();let after=snapshot();assert_eq!(inv.headers.get_all("x-many").iter().count(),4096);
        let request=json!({"elapsedMicros":start.elapsed().as_micros(),"grossRequestedBytes":after.1-before.1,"allocationCalls":after.0-before.0});
        let response_program=Arc::new(Candidate::compile(&json!({"endpointRules":{"/x@post":{"res-tra":["headers"]}},"ruleBodies":{"headers":{"ruleId":"headers","ruleName":"headers","ruleType":"res-tra","common":"Y","conditionLanguage":"cel","conditionSecurityProfile":"standard","expression":"true","actions":[{"actionId":"headers","actionRef":"response-headers-literal","actionValues":{"update":{"x-added":"after"}}}]}}})).unwrap());
        let adapter=TransformerAdapter::new(response_program,vec!["/x".into()]);let response=GatewayResponse{status:http::StatusCode::OK,headers:inv.headers.clone(),body:Some(bytes::Bytes::from_static(b"before")),body_is_binary:false};let before=snapshot();let start=Instant::now();let output=adapter.response(&inv,&state,&response).await.unwrap();let after=snapshot();assert_eq!(output.headers.get_all("x-many").iter().count(),4096);assert_eq!(output.headers["x-added"],"after");
        json!({"retainedHeaderValues":4096,"serializedHeaderInputBytes":wire,"request":request,"response":{"elapsedMicros":start.elapsed().as_micros(),"grossRequestedBytes":after.1-before.1,"allocationCalls":after.0-before.0}})
    });
    println!(
        "TRANSFORMER_ALLOCATION_OBSERVATION {}",
        json!({"rules":32,"actions":128,"serializedCompileBytes":raw.to_string().len(),"compileMicros":compile_micros,"compileAllocationCalls":after.0-before.0,"compileGrossRequestedBytes":after.1-before.1,"runtime":result,"transportStaging":transport,"hardHeapOrRssGuarantee":false})
    );
}
