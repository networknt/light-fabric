// Explicitly invoked qualification: the official package remains external and pinned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires MCP_CONFORMANCE_CLI and MCP_CONFORMANCE_OUTPUT"]
async fn mcp_official_tools_profile_with_live_reload() {
    let cli = std::env::var("MCP_CONFORMANCE_CLI").expect("pinned official CLI path");
    let output =
        std::path::PathBuf::from(std::env::var("MCP_CONFORMANCE_OUTPUT").expect("evidence path"));
    std::fs::create_dir_all(&output).unwrap();
    assert_eq!(
        std::fs::read_dir(&output).unwrap().count(),
        0,
        "use an empty evidence directory"
    );
    let config_dir = TempDir::new().unwrap();
    let external_dir = TempDir::new().unwrap();
    let port = free_tcp_port();
    let backend_port = free_tcp_port();
    let config_path = config_dir.path().join(light_pingora::MCP_ROUTER_FILE);
    let trigger = config_dir.path().join("trigger");
    let acknowledged = config_dir.path().join("reloaded");
    std::fs::write(config_dir.path().join("server.yml"), format!("ip: 127.0.0.1\nadvertisedAddress: 127.0.0.1\nhttpsPort: 8443\nserviceId: com.networknt.light-gateway-1.0.0\nstartOnRegistryFailure: true\nenvironment: dev\nhttpPort: {port}\nenableHttp: true\nenableHttps: false\nenableRegistry: false\ndynamicPort: false\nshutdownGracefulPeriod: 100\n")).unwrap();
    std::fs::write(config_dir.path().join("handler.yml"), "handlers: [mcp]\npaths:\n  - path: /mcp\n    method: POST\n    exec: [mcp]\ndefaultHandlers: []\n").unwrap();
    let tool = |name: &str| json!({"name":name,"description":"Completed backend diagnostic; no client-side capability required.","endpointName":name,"endpoint":name,"path":"/mcp","method":"POST","apiType":"mcp","protocol":"http","targetHost":format!("http://127.0.0.1:{backend_port}"),"backendMcpProtocol":"stateless","backendCredentialMode":"anonymous","backendResource":format!("http://127.0.0.1:{backend_port}/mcp"),"sessionIndependent":true,"inputSchema":{"type":"object","properties":{}},"toolMetadata":{"runtime":{"allowPrivateTargetHost":true}}});
    let config = json!({"enabled":true,"path":"/mcp","originAllowlist":[format!("http://127.0.0.1:{port}"),format!("http://localhost:{port}")],"protocols":{"stateless":{"enabled":true,"maxSubscriptionDurationMs":2000}},"tools":[tool("test_logging_tool"),tool("test_streaming_elicitation"),tool("test_trigger_tool_change"),tool("test_simple_text"),tool("test_image_content"),tool("test_audio_content"),tool("test_embedded_resource"),tool("test_multiple_content_types"),tool("test_error_handling")]});
    std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
    // Backend diagnostic names exercise ordinary complete results. This fixture
    // does not claim to implement elicitation, sampling, or MRTR.
    let script = r#"
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
import json,sys,pathlib,time,threading,base64,io,wave
trigger=pathlib.Path(sys.argv[2]); ack=pathlib.Path(sys.argv[3]); lock=threading.Lock(); generation=0
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):
  global generation
  req=json.loads(self.rfile.read(int(self.headers['Content-Length'])))
  name=req['params']['name']
  if name=='test_trigger_tool_change':
   with lock:
    generation+=1; trigger.write_text(str(generation)); deadline=time.monotonic()+5
    while not (ack.exists() and ack.read_text()==str(generation)):
     if time.monotonic()>deadline:raise RuntimeError('reload did not complete')
     time.sleep(.01)
  frames=[]
  if name=='test_logging_tool':frames.append({'jsonrpc':'2.0','method':'notifications/message','params':{'level':'info','data':'backend-only diagnostic'}})
  text={'type':'text','text':'diagnostic complete'}
  image={'type':'image','mimeType':'image/png','data':'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/l9sAAAAASUVORK5CYII='}
  audio=io.BytesIO()
  with wave.open(audio,'wb') as w:w.setnchannels(1);w.setsampwidth(2);w.setframerate(8000);w.writeframes(b'\0\0')
  sound={'type':'audio','mimeType':'audio/wav','data':base64.b64encode(audio.getvalue()).decode()}
  resource={'type':'resource','resource':{'uri':'test://diagnostic','mimeType':'text/plain','text':'diagnostic resource'}}
  content={'test_image_content':[image],'test_audio_content':[sound],'test_embedded_resource':[resource],'test_multiple_content_types':[text,image,resource]}.get(name,[text])
  frames.append({'jsonrpc':'2.0','id':req['id'],'result':{'resultType':'complete','content':content,'isError':name=='test_error_handling'}})
  body=''.join('data: '+json.dumps(f)+'\n\n' for f in frames).encode()
  self.send_response(200);self.send_header('Content-Type','text/event-stream');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
ThreadingHTTPServer(('127.0.0.1',int(sys.argv[1])),Handler).serve_forever()
"#;
    let mut backend = tokio::process::Command::new("python3")
        .args([
            "-u",
            "-c",
            script,
            &backend_port.to_string(),
            trigger.to_str().unwrap(),
            acknowledged.to_str().unwrap(),
        ])
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_for_tcp(format!("127.0.0.1:{backend_port}").parse().unwrap()).await;
    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
        .with_config_dir(config_dir.path())
        .with_external_config_dir(external_dir.path())
        .build();
    let running = runtime.start().await.unwrap();
    wait_for_tcp(format!("127.0.0.1:{port}").parse().unwrap()).await;
    let runtime_config = running.config.clone();
    let reloader = tokio::spawn(async move {
        let mut seen = String::new();
        loop {
            if let Ok(value) = std::fs::read_to_string(&trigger) {
                if value != seen && !value.is_empty() {
                    let mut next = config.clone();
                    let mut added = next["tools"][0].clone();
                    added["name"] = json!(format!("fixture_generation_{value}"));
                    next["tools"].as_array_mut().unwrap().push(added);
                    std::fs::write(&config_path, serde_yaml::to_string(&next).unwrap()).unwrap();
                    let result = runtime_config
                        .module_registry
                        .reload_modules(
                            ReloadContext::new(runtime_config.clone()),
                            &[light_pingora::MCP_ROUTER_MODULE_ID.to_string()],
                        )
                        .await;
                    assert!(result.failed.is_empty(), "{result:?}");
                    seen = value.clone();
                    std::fs::write(&acknowledged, value).unwrap();
                }
            }
            tokio::time::sleep(TokioDuration::from_millis(10)).await;
        }
    });
    for scenario in [
        "server-stateless",
        "caching",
        "dns-rebinding-protection",
        "tools-list",
        "tools-call-simple-text",
        "tools-call-image",
        "tools-call-audio",
        "tools-call-embedded-resource",
        "tools-call-mixed-content",
        "tools-call-error",
        "server-sse-multiple-streams",
    ] {
        let result = timeout(
            TokioDuration::from_secs(90),
            tokio::process::Command::new("node")
                .args([
                    &cli,
                    "server",
                    "--url",
                    &format!("http://127.0.0.1:{port}/mcp"),
                    "--scenario",
                    scenario,
                    "--spec-version",
                    "2026-07-28",
                    "--verbose",
                    "--output-dir",
                    output.to_str().unwrap(),
                ])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        std::fs::write(output.join(format!("{scenario}.log")), &result.stdout).unwrap();
        std::fs::write(output.join(format!("{scenario}.stderr")), &result.stderr).unwrap();
        assert!(
            matches!(result.status.code(), Some(0 | 1)),
            "runner process failed: {:?}",
            result.status
        );
    }
    reloader.abort();
    backend.kill().await.unwrap();
    running.shutdown().await.unwrap();
    let mut checks = Vec::new();
    for entry in std::fs::read_dir(&output).unwrap() {
        let path = entry.unwrap().path().join("checks.json");
        if path.is_file() {
            for check in
                serde_json::from_slice::<Vec<serde_json::Value>>(&std::fs::read(path).unwrap())
                    .unwrap()
            {
                checks.push(check);
            }
        }
    }
    assert_eq!(
        checks.len(),
        57,
        "pinned runner inventory changed; review applicability"
    );
    assert_eq!(
        checks
            .iter()
            .find(|c| c["id"] == "sep-2575-server-implements-discover")
            .unwrap()["details"]["result"]["capabilities"],
        json!({"tools":{"listChanged":true}})
    );
    let mut not_applicable = Vec::new();
    for check in &checks {
        let id = check["id"].as_str().unwrap().to_string();
        match check["status"].as_str().unwrap() {
            "SUCCESS" => {}
            "INFO" if id == "server-sse-streams-functional" => {
                assert_eq!(check["details"]["numSseStreams"], 0);
                not_applicable.push(id.clone());
            }
            "FAILURE"
                if matches!(
                    id.as_str(),
                    "sep-2575-server-rejects-undeclared-capability"
                        | "sep-2575-missing-capability-http-400"
                ) =>
            {
                assert_eq!(check["details"]["untestable"], true);
                assert!(
                    check["errorMessage"]
                        .as_str()
                        .unwrap()
                        .contains("does not list the diagnostic tool 'test_missing_capability'")
                );
                not_applicable.push(id.clone());
            }
            "FAILURE"
                if matches!(
                    id.as_str(),
                    "sep-2549-prompts-list-caching-hints"
                        | "sep-2549-resources-list-caching-hints"
                        | "sep-2549-resources-templates-list-caching-hints"
                ) =>
            {
                assert!(
                    check["errorMessage"]
                        .as_str()
                        .unwrap()
                        .contains("JSON-RPC error -32601")
                );
                not_applicable.push(id.clone());
            }
            "SKIPPED"
                if matches!(
                    id.as_str(),
                    "sep-2549-resources-read-caching-hints"
                        | "sep-2575-server-sends-prompts-list-changed-on-subscription"
                ) =>
            {
                not_applicable.push(id.clone());
            }
            _ => panic!("unexpected conformance result: {check}"),
        }
    }
    assert_eq!(not_applicable.len(), 8);
    std::fs::write(output.join("tools-profile-summary.json"), serde_json::to_vec_pretty(&json!({"profile":"tools facade with bounded subscriptions","applicablePassed":checks.len()-not_applicable.len(),"notApplicable":not_applicable,"rawOfficialFailuresPreserved":true,"fullSpecificationConformance":false})).unwrap()).unwrap();
}
