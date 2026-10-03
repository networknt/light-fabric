// Included in the existing completion test module to reuse bounded component fixtures.
use workflow_expression::{Bindings, Position, StepField, StepRequest, WorkerError};

#[derive(Clone, Default)]
struct W5Logs(Arc<std::sync::Mutex<String>>);
impl tracing::field::Visit for W5Logs {
    fn record_debug(&mut self, field:&tracing::field::Field, value:&dyn std::fmt::Debug) {
        self.0.lock().unwrap().push_str(&format!("{field}={value:?}\n"));
    }
}
impl tracing::Subscriber for W5Logs {
    fn enabled(&self,_:&tracing::Metadata<'_>)->bool {true}
    fn new_span(&self,_:&tracing::span::Attributes<'_>)->tracing::span::Id {tracing::span::Id::from_u64(1)}
    fn record(&self,_:&tracing::span::Id,values:&tracing::span::Record<'_>) {values.record(&mut self.clone());}
    fn record_follows_from(&self,_:&tracing::span::Id,_:&tracing::span::Id) {}
    fn event(&self,event:&tracing::Event<'_>) {event.record(&mut self.clone());}
    fn enter(&self,_:&tracing::span::Id) {}
    fn exit(&self,_:&tracing::span::Id) {}
}

#[tokio::test]
async fn w5_rpc_headers_admission_and_model_roundtrip() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor,engine)=executor();
    for call in ["jsonrpc","openrpc"] {
        for headers in [json!({"X-Test":"prefix ${context.s}"}),json!("${ {'X-Test':workflow.input.s}}") ] {
            let task=json!({"call":call,"with":{"endpoint":"https://rpc.invalid","document":{"endpoint":"https://docs.invalid"},"method":"run","headers":headers}});
            let raw=json!({"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"step":task}]});
            workflow_expression::DefinitionValidation::from_raw(&raw).unwrap().validate(&engine).await.unwrap();
            let typed:TaskDefinition=serde_json::from_value(task).unwrap();
            assert_eq!(serde_json::to_value(typed).unwrap()["with"]["headers"],headers);
        }
        let raw=json!({"document":{"metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"step":{"call":call,"with":{"headers":{"x":1}}}}]});
        assert!(workflow_expression::DefinitionValidation::from_raw(&raw).unwrap().validate(&engine).await.is_err());
    }
    let typed:TaskDefinition=serde_json::from_value(json!({"call":"mcp","with":{"method":"resources/read","params":{"uri":"fixed"}}})).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap()["with"]["parameters"],json!({"uri":"fixed"}));
    drop(executor);close(engine).await;
}

#[tokio::test]
async fn w5_every_position_forms_kinds_bindings_cache_and_immutable_input() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor, engine)=executor();
    let context=json!({"s":"value","n":3,"secret":"W5_SENTINEL_CREDENTIAL","workflow":"shadow"});
    let input=json!({"s":"original"});
    let positions=[Position::HttpBody,Position::HttpQuery,Position::HttpHeader,Position::IdempotencyKey,Position::JsonRpcUri,Position::JsonRpcParams,Position::JsonRpcHeader,Position::OpenRpcEndpoint,Position::McpParams,Position::McpResourceUri,Position::A2aParameters,Position::AgentInput,Position::AgentMockOutput,Position::AgentInstructions,Position::AgentPrompt,Position::AskCategory,Position::AskReason,Position::AskAssignee,Position::AskRole];
    // Cache sources from positions where output/value are allowed, then reject the same programs here.
    for (position,source) in [(Position::Export,"${output}"),(Position::AssertJsonPredicate,"${value == value}")] {
        engine.step(StepRequest{bindings:Bindings::new(&context,&input,Some(&json!("x")),Some(&json!(1))).unwrap(),fields:vec![StepField{path:"/prime".into(),position,template:json!(source),value_from:None}],first_true:false}).await.unwrap();
    }
    for position in positions {
        let request=|template|StepRequest{bindings:Bindings::new(&context,&input,None,None).unwrap(),fields:vec![StepField{path:"/field".into(),position,template,value_from:None}],first_true:false};
        let cases=if position==Position::JsonRpcHeader {
            vec![(json!("${ {'x':workflow.input.s}}"),json!({"x":"original"})),(json!({"x":"prefix ${context.s}"}),json!({"x":"prefix value"})),(json!({"x":"literal"}),json!({"x":"literal"}))]
        } else {
            vec![(json!("${workflow.input.s}"),json!("original")),(json!("prefix ${context.s}"),json!("prefix value")),(json!("literal"),json!("literal"))]
        };
        for (source,expected) in cases {assert_eq!(engine.step(request(source)).await.unwrap(),vec![expected],"{position:?}");}
        let bad=if position==Position::JsonRpcHeader {json!("${ {'x':1}}") } else {json!("prefix ${1}")};
        assert!(engine.step(request(bad)).await.is_err(),"{position:?}");
        for source in ["${output}","${value == value}","${s}"] {
            let error=engine.step(request(json!(source))).await.unwrap_err();
            assert!(matches!(error.error,WorkerError::Expression(_)));
            assert!(!format!("{error:?}").contains("W5_SENTINEL"));
        }
    }
    assert_eq!(context["s"],json!("value"));
    assert_eq!(input["s"],json!("original"));
    drop(executor); close(engine).await;
}

#[tokio::test]
async fn w5_prepares_all_authored_fields_before_any_dispatch_job_or_assignment() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let logs=W5Logs::default();let _subscriber=tracing::subscriber::set_default(logs.clone());
    let (mut executor,engine)=executor();
    let mock=Arc::new(expression_requests::Mock::default()); executor.w5_mock=Some(mock.clone());
    for task in [
        json!({"call":"http","idempotencyKey":"${workflow.input.original}","with":{"method":"POST","endpoint":"https://rpc.invalid/{id}","body":{"x":"${context.secret}"}}}),
        json!({"call":"jsonrpc","with":{"endpoint":"https://rpc.invalid","method":"run","params":{"early":"${context.secret}"},"headers":"${ {'z':1}}"}}),
        json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"method":"run","params":{"early":"${context.secret}"},"server":"${1}"}}),
        json!({"call":"mcp","idempotencyKey":"${1}","with":{"method":"tools/call","parameters":{"name":"fixed"},"server":{"endpoint":"https://rpc.invalid","transport":"http"}}}),
        json!({"call":"a2a","idempotencyKey":"${1}","with":{"agentRef":"fixed","method":"message/send","parameters":{"early":"${context.secret}"}}}),
        json!({"call":"agent","with":{"agent":"00000000-0000-0000-0000-000000000001","mode":"service","input":{"coding":{}},"instructions":"${1}"}}),
        json!({"ask":{"prompt":"answer","assignment":{"assigneeId":"user","roleId":"${1}"}}}),
    ] {
        let c=claimed(task,json!({"secret":"W5_SENTINEL_CREDENTIAL","id":"abc"}));
        let result=executor.execute_task(&c).await.unwrap();
        assert_eq!(result.status_code,"F");
        assert_eq!(result.task_output["retryable"],json!(false));
        assert!(!result.task_output.to_string().contains("W5_SENTINEL"));
        assert!(mock.requests.lock().unwrap().is_empty());
        // Lazy unused pool: reaching a job/assignment write or catalog lookup would fail this test.
    }
    assert!(!logs.0.lock().unwrap().contains("W5_SENTINEL"));
    drop(executor); close(engine).await;
}

#[tokio::test]
async fn w5_openrpc_fetch_method_stages_headers_and_no_second_evaluation() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (mut executor,engine)=executor();
    let document=json!({"servers":[{"url":"https://rpc.invalid/${context.s}"}],"methods":[{"name":"run","params":[{"name":"x","required":true,"schema":{"type":"string"}}]}]});
    let mock=Arc::new(expression_requests::Mock{document,response:json!({"jsonrpc":"2.0","result":{"ok":true}}),..Default::default()});
    executor.w5_mock=Some(mock.clone());
    let task=json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid/${context.s}"},"method":"run","params":{"x":"${context.literal}"},"headers":"${ {'X-Test':context.literal}}"}});
    let c=claimed(task,json!({"s":"path","literal":"${missing}"}));
    let prepared=executor.w5_prepare(&c,executor.find_task_definition(&c.definition,"step").unwrap()).await.unwrap();
    let TaskDefinition::Call(CallTaskDefinition::OpenRpc(call))=prepared else {panic!()};
    let result=executor.execute_openrpc_call(&call.with,&c.context_data,Some(&c)).await.unwrap();
    assert_eq!(result.task_output,json!({"ok":true}));
    {let requests=mock.requests.lock().unwrap(); assert_eq!(requests.len(),2);assert!(requests[0].0);assert!(!requests[1].0);
     assert_eq!(requests[0].1.url().as_str(),"https://docs.invalid/path");assert_eq!(requests[1].1.url().as_str(),"https://rpc.invalid/path");
     assert_eq!(requests[1].1.headers()["x-test"],"${missing}");
     let body:Value=serde_json::from_slice(requests[1].1.body().unwrap().as_bytes().unwrap()).unwrap();assert_eq!(body["params"]["x"],json!("${missing}"));}
    mock.requests.lock().unwrap().clear();
    let c=claimed(json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"method":"run","params":{"x":2}}}),json!({"s":"path"}));
    let prepared=executor.w5_prepare(&c,executor.find_task_definition(&c.definition,"step").unwrap()).await.unwrap();
    let TaskDefinition::Call(CallTaskDefinition::OpenRpc(call))=prepared else {panic!()};
    assert!(executor.execute_openrpc_call(&call.with,&c.context_data,Some(&c)).await.is_err());
    assert_eq!(mock.requests.lock().unwrap().len(),1);
    assert!(mock.requests.lock().unwrap()[0].0);
    mock.requests.lock().unwrap().clear();
    let c=claimed(json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"method":"run","params":"x ${1}"}}),json!({}));
    assert!(executor.w5_prepare(&c,executor.find_task_definition(&c.definition,"step").unwrap()).await.is_err());
    assert!(mock.requests.lock().unwrap().is_empty());
    drop(executor);close(engine).await;
}

#[tokio::test]
async fn w5_openrpc_authored_and_derived_invalid_destinations_have_distinct_effect_counts() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (mut executor,engine)=executor();
    let mock=Arc::new(expression_requests::Mock{document:json!({"servers":[{"url":"file:///invalid"}],"methods":[{"name":"run"}]}),..Default::default()});
    executor.w5_mock=Some(mock.clone());
    let c=claimed(json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"server":{"url":"file:///invalid"},"method":"run"}}),json!({}));
    assert!(executor.w5_prepare(&c,executor.find_task_definition(&c.definition,"step").unwrap()).await.is_err());
    assert!(mock.requests.lock().unwrap().is_empty());
    let c=claimed(json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"method":"run"}}),json!({}));
    assert_eq!(executor.execute_task(&c).await.unwrap().status_code,"F");
    {let requests=mock.requests.lock().unwrap();assert_eq!(requests.len(),1);assert!(requests[0].0);}
    drop(executor);close(engine).await;
}

#[tokio::test]
async fn w5_cancellation_and_lease_loss_after_evaluation_prevent_send() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (mut executor,engine)=executor();
    let mock=Arc::new(expression_requests::Mock::default()); executor.w5_mock=Some(mock.clone());
    let c=claimed(json!({"call":"jsonrpc","with":{"endpoint":"https://rpc.invalid","method":"run","params":"${ {'x':workflow.input.original}}"}}),json!({}));
    let prepared=executor.w5_prepare(&c,executor.find_task_definition(&c.definition,"step").unwrap()).await.unwrap();
    mock.stale.store(true,std::sync::atomic::Ordering::SeqCst);
    let TaskDefinition::Call(CallTaskDefinition::JsonRpc(call))=prepared else {panic!()};
    let error=executor.execute_jsonrpc_call(&call.with,&c.context_data,Some(&c)).await.err().unwrap();
    assert!(error.downcast_ref::<sqlx::Error>().is_some_and(expression_completion::rollback_completion));
    assert!(mock.requests.lock().unwrap().is_empty());
    drop(executor);close(engine).await;
}

#[tokio::test]
async fn w5_agent_and_ask_prepare_typed_values_and_legacy_bypass() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor,engine)=executor();
    let c=claimed(json!({"call":"agent","with":{"agent":"fixed","input":{"x":"${workflow.input.original}"},"mockOutput":"${ {'n':context.old}}","instructions":"i ${string(context.old)}","prompt":"${context.literal}"}}),json!({"old":3,"literal":"${missing}"}));
    let prepared=executor.w5_prepare(&c,executor.find_task_definition(&c.definition,"step").unwrap()).await.unwrap();
    let TaskDefinition::Call(CallTaskDefinition::Agent(call))=prepared else {panic!()};
    assert_eq!(call.with.input,Some(json!({"x":9})));assert_eq!(call.with.mock_output,Some(json!({"n":3})));
    let catalog=AgentCatalog{agent:AgentDefinitionRecord{agent_def_id:Uuid::new_v4(),agent_name:None,model_provider:"mock".into(),model_name:"fixed".into(),api_key_ref:Some("literal:W5_SENTINEL_CREDENTIAL".into()),temperature:0.0,max_tokens:None,aggregate_version:1},skills:vec![],tools:vec![]};
    let messages=executor.build_agent_messages(&call.with,&catalog,call.with.input.as_ref().unwrap(),&c.context_data,None,true).unwrap();
    let encoded=serde_json::to_string(&messages).unwrap();assert!(encoded.contains("missing"));assert!(!encoded.contains("W5_SENTINEL"));
    let c=claimed(json!({"ask":{"prompt":"answer","assignment":{"categoryCode":"${'category'}","reasonCode":"reason ${string(workflow.input.original)}","assigneeId":"${'user'}","roleId":"role"}}}),json!({}));
    let prepared=executor.w5_prepare(&c,executor.find_task_definition(&c.definition,"step").unwrap()).await.unwrap();
    let TaskDefinition::Ask(ask)=prepared else {panic!()};
    assert_eq!(ask.ask.assignment.unwrap().reason_code.as_deref(),Some("reason 9"));
    assert_eq!(request_json(&executor,&json!("${missing}"),&json!({}),true),json!("${missing}"));
    assert_eq!(request_string(&executor,"${{ old }}",&json!({"old":"legacy"}),false),"legacy");
    drop(executor);close(engine).await;
}

#[tokio::test]
async fn w5_agent_dispatch_prepared_inline_and_fenced_service_job() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (mut executor, engine)=executor();
    let mock=Arc::new(expression_requests::Mock::default());executor.w5_mock=Some(mock.clone());
    let c=claimed(json!({"call":"agent","with":{"agent":"fixed","input":{"x":"${workflow.input.original}"},"mockOutput":"${ {'answer':context.old}}"}}),json!({"old":3}));
    let result=executor.execute_task(&c).await.unwrap();
    assert_eq!(result.status_code,"C");assert_eq!(result.task_output["answer"],json!(3));
    let c=claimed(json!({"call":"agent","with":{"agent":"00000000-0000-0000-0000-000000000001","mode":"service","input":{"coding":{"x":"${workflow.input.original}"}}}}),json!({}));
    assert_eq!(executor.execute_task(&c).await.unwrap().status_code,"W");
    assert_eq!(*mock.jobs.lock().unwrap(),vec![json!({"coding":{"x":9}})]);
    mock.jobs.lock().unwrap().clear();mock.stale.store(true,std::sync::atomic::Ordering::SeqCst);
    let error=executor.execute_task(&c).await.err().unwrap();
    assert!(error.downcast_ref::<sqlx::Error>().is_some_and(expression_completion::rollback_completion));
    assert!(mock.jobs.lock().unwrap().is_empty());assert!(mock.requests.lock().unwrap().is_empty());
    drop(executor);close(engine).await;
}

#[test]
fn w5_endpoint_placeholders_lightapi_and_runtime_registered_targets() {
    let context=json!({"id":"a/b?c#d%; space","n":u64::MAX,"literal":"{id}${missing}"});
    for uri in ["https://rpc.invalid/{id}","https://rpc.invalid/{n}","https://rpc.invalid/{literal}"] {
        let encoded=workflow_expression::encode_path_placeholders(uri,&context).unwrap();
        assert!(!encoded.contains('{'));assert!(!encoded.contains(' '));
    }
    for value in [json!(""),json!("."),json!(".."),Value::Null,json!(true),json!(1.2),json!([]),json!({})] {
        assert!(workflow_expression::encode_path_placeholders("https://rpc.invalid/{id}",&json!({"id":value})).is_err());
    }
    for uri in ["h{a}ttp://rpc.invalid/x","https://{id}.invalid/x","https://rpc.invalid/x?q={id}","https://rpc.invalid/x#{id}","https://rpc.invalid/{a.b}","https://rpc.invalid/{a-b}","https://rpc.invalid/${context.id}"] {
        assert!(workflow_expression::encode_path_placeholders(uri,&context).is_err());
    }
    assert!(workflow_expression::encode_path_placeholders("https://rpc.invalid/{missing}",&context).is_err());
    let document=json!({"operations":{"get":{"endpointId":"fixed","protocol":"http","method":"GET","authentication":{"type":"none"},"endpoint":"items/{id}"}}});
    let target=resolve_lightapi_http_endpoint(&document,"fixed","dev","GET","https://rpc.invalid/base").unwrap();
    assert!(target.ends_with("items/{id}"));
    let encoded=workflow_expression::encode_path_placeholders(&target,&context).unwrap();
    assert!(encoded.contains("a%2Fb%3Fc%23d%25%3B%20space"));
}

#[tokio::test]
async fn w5_http_rpc_and_mcp_resource_dispatch_preserve_prepared_data() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (mut executor,engine)=executor();
    let mock=Arc::new(expression_requests::Mock{response:json!({"jsonrpc":"2.0","result":{"ok":true}}),..Default::default()});executor.w5_mock=Some(mock.clone());
    for task in [
        json!({"call":"http","with":{"method":"GET","endpoint":"https://rpc.invalid/{id}","query":{"x":"${context.literal}"},"headers":{"X-Test":"${context.literal}"},"body":{"x":"${workflow.input.original}"}}}),
        json!({"call":"jsonrpc","with":{"endpoint":"https://rpc.invalid/${context.id}","method":"run","params":"${ {'x':context.literal}}","headers":"${ {'X-Test':context.literal}}"}}),
        json!({"call":"mcp","with":{"resource":"${context.literal}","server":{"endpoint":"https://rpc.invalid","transport":"http"}}}),
    ] {
        let c=claimed(task,json!({"id":"path","literal":"${missing}"}));
        let result=executor.execute_task(&c).await.unwrap();assert_eq!(result.status_code,"C","{}",result.task_output);
    }
    {let requests=mock.requests.lock().unwrap();assert_eq!(requests.len(),3);
     assert_eq!(requests[0].1.headers()["x-test"],"${missing}");assert!(requests[0].1.url().as_str().contains("path"));
     for (index,key) in [(1,"x"),(2,"uri")] {let body:Value=serde_json::from_slice(requests[index].1.body().unwrap().as_bytes().unwrap()).unwrap();assert_eq!(body["params"][key],json!("${missing}"));}}
    drop(executor);close(engine).await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; W5 assignments/wait atomicity, original snapshot and resumed completion"]
async fn w5_postgres_ask_assignments_wait_and_resumed_completion_once() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let mut g=PgGate::new(json!({"ask":{"prompt":"answer","assignment":{"categoryCode":"${'category'}","reasonCode":"${'reason'}","assigneeId":"${'user'}","roleId":"${'role'}"}},"export":{"as":{"captured":"${context.old}","answer":"${output.answer}"}}})).await;
    sqlx::query("CREATE TABLE task_asst_t(host_id uuid,task_asst_id uuid,task_id uuid,assigned_ts timestamptz,assignee_id text,assignment_type text,assignment_id text,reason_code text,category_code text,update_user text,update_ts timestamptz,aggregate_version bigint,active bool)").execute(&g.pool).await.unwrap();
    let owner=Uuid::new_v4();g.claimed.task.task_type="ask".into();g.claimed.host_lease=Some(HostTaskLease{owner,fencing_token:1});
    sqlx::query("UPDATE task_info_t SET task_type='ask',execution_placement='host',lease_owner=$1,lease_fencing_token=1,lease_expires_ts=clock_timestamp()+interval '1 hour'").bind(owner).execute(&g.pool).await.unwrap();
    let waiting=g.executor.execute_task(&g.claimed).await.unwrap();assert_eq!(waiting.status_code,"W");
    let mut tx=g.pool.begin().await.unwrap();g.executor.finish_task(&mut tx,&g.claimed,waiting).await.unwrap();tx.commit().await.unwrap();
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM task_asst_t").fetch_one(&g.pool).await.unwrap();assert_eq!(count,2);
    sqlx::query("UPDATE process_info_t SET context_data=jsonb_set(context_data,'{old}','99')").execute(&g.pool).await.unwrap();
    sqlx::query("UPDATE task_info_t SET status_code='C',result_code=$1,lease_owner=$2,lease_fencing_token=2,lease_expires_ts=clock_timestamp()+interval '1 hour'").bind("{\"answer\":9}").bind(owner).execute(&g.pool).await.unwrap();
    g.claimed.task.status_code="C".into();g.claimed.task.result_code=Some("{\"answer\":9}".into());g.claimed.host_lease=Some(HostTaskLease{owner,fencing_token:2});
    let mut tx=g.pool.begin().await.unwrap();assert!(g.executor.w4_context(&mut tx,&mut g.claimed).await.unwrap());
    let result=g.executor.completed_ask_result(&g.claimed);g.executor.finish_task(&mut tx,&g.claimed,result).await.unwrap();tx.commit().await.unwrap();
    assert_eq!(g.context().await["captured"],json!(1));assert_eq!(g.context().await["answer"],json!(9));
    let mut tx=g.pool.begin().await.unwrap();assert!(g.executor.finish_task(&mut tx,&g.claimed,g.executor.completed_ask_result(&g.claimed)).await.is_err());tx.rollback().await.unwrap();
    g.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; W5 post-evaluation lease loss forbids assignment writes and rolls back waiting"]
async fn w5_postgres_ask_stale_after_preparation_zero_assignments() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let mut g=PgGate::new(json!({"ask":{"prompt":"answer","assignment":{"assigneeId":"user","roleId":"${'role'}"}}})).await;
    sqlx::query("CREATE TABLE task_asst_t(host_id uuid,task_asst_id uuid,task_id uuid,assigned_ts timestamptz,assignee_id text,assignment_type text,assignment_id text,reason_code text,category_code text,update_user text,update_ts timestamptz,aggregate_version bigint,active bool)").execute(&g.pool).await.unwrap();
    let owner=Uuid::new_v4();g.claimed.task.task_type="ask".into();g.claimed.host_lease=Some(HostTaskLease{owner,fencing_token:1});
    sqlx::query("UPDATE task_info_t SET task_type='ask',lease_owner=$1,lease_fencing_token=1,lease_expires_ts=clock_timestamp()+interval '1 hour'").bind(owner).execute(&g.pool).await.unwrap();
    let result=g.executor.execute_task(&g.claimed).await.unwrap();
    sqlx::query("UPDATE task_info_t SET lease_expires_ts=clock_timestamp()-interval '1 second'").execute(&g.pool).await.unwrap();
    let mut tx=g.pool.begin().await.unwrap();let error=g.executor.finish_task(&mut tx,&g.claimed,result).await.unwrap_err();assert!(expression_completion::rollback_completion(&error));tx.rollback().await.unwrap();
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM task_asst_t").fetch_one(&g.pool).await.unwrap(),0);
    assert_eq!(sqlx::query_scalar::<_,String>("SELECT status_code FROM task_info_t").fetch_one(&g.pool).await.unwrap(),"A");
    g.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; W5 native enqueue rejects lost task fence before job creation"]
async fn w5_postgres_agent_enqueue_expired_fence_creates_no_job() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let g=PgGate::new(json!({"call":"agent","with":{"agent":"00000000-0000-0000-0000-000000000001","mode":"service","input":{"coding":{}}}})).await;
    sqlx::query("ALTER TABLE workflow_invocation_t ADD COLUMN principal_subject text, ADD COLUMN end_user_subject text").execute(&g.pool).await.unwrap();
    sqlx::query("UPDATE workflow_invocation_t SET principal_subject='user',end_user_subject='user'").execute(&g.pool).await.unwrap();
    sqlx::query("CREATE TABLE workflow_agent_job_t(host_id uuid,job_id uuid)").execute(&g.pool).await.unwrap();
    let owner=Uuid::new_v4();sqlx::query("UPDATE task_info_t SET task_type='call',lease_owner=$1,lease_fencing_token=1,lease_expires_ts=clock_timestamp()-interval '1 second'").bind(owner).execute(&g.pool).await.unwrap();
    let error=crate::native_jobs::enqueue_with_artifacts_fenced(&g.pool,g.claimed.task.host_id,g.claimed.task.process_id,g.claimed.task.task_id,"step",Uuid::new_v4(),json!({"coding":{}}),json!({"type":"object"}),Utc::now()+chrono::Duration::minutes(5),100,0,0,4,None,Some((Some((owner,1)),"cel-workflow-v2"))).await.unwrap_err();
    assert!(error.downcast_ref::<sqlx::Error>().is_some_and(expression_completion::rollback_completion));
    assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM workflow_agent_job_t").fetch_one(&g.pool).await.unwrap(),0);
    g.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; W5 terminal agent results use retained context and W4 once-only completion"]
async fn w5_postgres_agent_result_retained_context_and_completion_once() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let g=PgGate::new(json!({"call":"agent","with":{"agent":"fixed","input":{}},"export":{"as":{"captured":"${context.old}","answer":"${output.answer}"}}})).await;
    sqlx::query("CREATE TABLE workflow_agent_job_t(host_id uuid,job_id uuid,workflow_process_id uuid,workflow_task_id uuid,state text,public_output jsonb,error jsonb,output_schema jsonb)").execute(&g.pool).await.unwrap();
    let job=Uuid::new_v4();
    sqlx::query("INSERT INTO workflow_agent_job_t VALUES($1,$2,$3,$4,'SUCCEEDED',$5,NULL,$6)").bind(g.claimed.task.host_id).bind(job).bind(g.claimed.task.process_id).bind(g.claimed.task.task_id).bind(json!({"answer":9})).bind(json!({"type":"object"})).execute(&g.pool).await.unwrap();
    sqlx::query("UPDATE task_info_t SET task_type='call',status_code='W'").execute(&g.pool).await.unwrap();
    sqlx::query("UPDATE process_info_t SET context_data=jsonb_set(context_data,'{old}','99')").execute(&g.pool).await.unwrap();
    assert!(g.executor.reconcile_agent_job(g.claimed.task.host_id,job).await.unwrap());
    assert_eq!(g.context().await["captured"],json!(1));assert_eq!(g.context().await["answer"],json!(9));
    assert!(!g.executor.reconcile_agent_job(g.claimed.task.host_id,job).await.unwrap());
    g.close().await;
}

#[tokio::test]
async fn w5_openrpc_document_authority_and_literal_variable_boundaries() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (mut executor, engine) = executor();
    for (url, variables, selector, expected) in [
        ("https://${context.host}/rpc", json!({}), None, None),
        ("https://rpc.invalid/${context.s}", json!({}), None, Some("https://rpc.invalid/path")),
        ("https://rpc.invalid/{a}/${context.s}/{b}", json!({"a":{"default":"${context.secret}"},"b":{"default":"fixed"}}), None, Some("https://rpc.invalid/${context.secret}/path/fixed")),
        ("https://rpc.invalid/{a}/{b}", json!({"a":{"default":"{b}"},"b":{"default":"fixed"}}), None, Some("https://rpc.invalid/{b}/fixed")),
        ("https://rpc.invalid/{base-path}", json!({"base-path":{"default":"fixed"}}), None, Some("https://rpc.invalid/fixed")),
        ("https://rpc.invalid/{a}/${context.s}/{b}", json!({"a":{"default":"default"},"b":{"default":"fixed"}}), Some(json!({"name":"main","variables":{"a":"${context.secret}","b":"{a}"}})), Some("https://rpc.invalid/${context.secret}/path/{a}")),
        ("https://rpc.invalid/${context.literal}", json!({"a":{"default":"wrong"}}), None, Some("https://rpc.invalid/{a}")),
    ] {
        let mock = Arc::new(expression_requests::Mock {
            document: json!({"servers":[{"name":"main","url":url,"variables":variables}],"methods":[{"name":"run"}]}),
            response: json!({"jsonrpc":"2.0","result":{"ok":true}}), ..Default::default()
        });
        executor.w5_mock = Some(mock.clone());
        let mut task = json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"method":"run"}});
        if let Some(selector) = selector {task["with"]["server"] = selector;}
        let c = claimed(task, json!({"host":"evil.invalid","s":"path","secret":"W5_SENTINEL_CREDENTIAL","literal":"{a}"}));
        let result = executor.execute_task(&c).await.unwrap();
        let requests = mock.requests.lock().unwrap();
        assert!(requests[0].0);
        if let Some(expected) = expected {
            assert_eq!(result.status_code, "C", "{url}: {:?}", result.task_output);
            assert_eq!(requests.len(), 2);
            assert!(!requests[1].0);
            assert_eq!(requests[1].1.url(), &reqwest::Url::parse(expected).unwrap());
            assert!(!requests[1].1.url().as_str().contains("W5_SENTINEL"));
        } else {
            assert_eq!(result.status_code, "F");
            assert_eq!(requests.len(), 1);
            assert!(!result.task_output.to_string().contains("W5_SENTINEL"));
        }
    }
    // Authored URLs have already crossed the preparation boundary. Their evaluated
    // `${...}` bytes are data and must never enter the document-derived scanner.
    let mock = Arc::new(expression_requests::Mock {
        document: json!({"methods":[{"name":"run"}]}),
        response: json!({"jsonrpc":"2.0","result":{"ok":true}}), ..Default::default()
    });
    executor.w5_mock = Some(mock.clone());
    let c = claimed(json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"server":{"url":"https://rpc.invalid/${context.literal}","variables":{"otherVariable":"expanded"}},"method":"run"}}), json!({"literal":"${otherVariable}","secret":"W5_SENTINEL_CREDENTIAL"}));
    let result = executor.execute_task(&c).await.unwrap();
    assert_eq!(result.status_code, "C", "{:?}", result.task_output);
    assert_eq!(mock.requests.lock().unwrap()[1].1.url(), &reqwest::Url::parse("https://rpc.invalid/${otherVariable}").unwrap());
    drop(executor); close(engine).await;
}

#[tokio::test]
async fn w5_openrpc_authored_variables_and_original_document_offsets() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (mut executor, engine) = executor();
    for (source, literal, variables, expected) in [
        ("https://rpc.invalid/${context.literal}", "{otherVariable}", json!({"otherVariable":"expanded"}), "https://rpc.invalid/{otherVariable}"),
        ("https://rpc.invalid/{base-path}/${context.literal}", "{otherVariable}", json!({"base-path":"fixed","otherVariable":"expanded"}), "https://rpc.invalid/fixed/{otherVariable}"),
        ("https://rpc.invalid/{base-path}/${context.literal}", "path", json!({"base-path":"${context.secret}"}), "https://rpc.invalid/${context.secret}/path"),
    ] {
        let mock = Arc::new(expression_requests::Mock { document:json!({"methods":[{"name":"run"}]}),response:json!({"jsonrpc":"2.0","result":true}),..Default::default() });
        executor.w5_mock = Some(mock.clone());
        let c=claimed(json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"server":{"url":source,"variables":variables},"method":"run"}}),json!({"literal":literal,"secret":"W5_SENTINEL_CREDENTIAL"}));
        assert_eq!(executor.execute_task(&c).await.unwrap().status_code,"C");
        assert_eq!(mock.requests.lock().unwrap()[1].1.url(), &reqwest::Url::parse(expected).unwrap());
    }
    let url="https://rpc.invalid/{base-path}/${context.missing}";
    let mock = Arc::new(expression_requests::Mock { document:json!({"servers":[{"url":url,"variables":{"base-path":{"default":"${context.secret}-long-variable-value"}}}],"methods":[{"name":"run"}]}),..Default::default() });
    executor.w5_mock=Some(mock.clone());
    let c=claimed(json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"method":"run"}}),json!({"secret":"W5_SENTINEL_CREDENTIAL"}));
    let result=executor.execute_task(&c).await.unwrap();
    assert_eq!(result.status_code,"F");
    assert_eq!(result.task_output["details"]["field"],json!("/with/server/url"));
    assert_eq!(result.task_output["details"]["spanIndex"],json!(0));
    assert_eq!(result.task_output["details"]["offset"],json!(url.find("${context.missing}").unwrap() + 2));
    assert_eq!(mock.requests.lock().unwrap().len(),1);
    drop(executor);close(engine).await;
}
