use mcp_client::{McpGatewayClient, McpProfile};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Default)]
struct Counts {
    initialize: AtomicUsize,
    list: AtomicUsize,
    call: AtomicUsize,
    delete: AtomicUsize,
}

async fn server(
    profile: McpProfile,
    fail_call: bool,
) -> (String, Arc<Counts>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let counts = Arc::new(Counts::default());
    let seen = counts.clone();
    let task = tokio::spawn(async move {
        let mut initialized = false;
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let split;
            loop {
                let mut chunk = [0; 4096];
                let size = socket.read(&mut chunk).await.unwrap();
                assert!(size > 0);
                bytes.extend_from_slice(&chunk[..size]);
                if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&bytes[..pos]);
                    let size = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= pos + 4 + size {
                        split = pos;
                        break;
                    }
                }
            }
            let head = String::from_utf8_lossy(&bytes[..split]).to_ascii_lowercase();
            if head.starts_with("delete ") {
                assert!(head.contains("mcp-session-id: strict-session"));
                seen.delete.fetch_add(1, Ordering::SeqCst);
                socket
                    .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                    .await
                    .unwrap();
                continue;
            }
            let request: Value = serde_json::from_slice(&bytes[split + 4..]).unwrap();
            let method = request["method"].as_str().unwrap();
            assert!(head.contains("accept: application/json, text/event-stream"));
            assert!(head.contains(&format!("mcp-protocol-version: {}", profile.version())));
            let modern = profile == McpProfile::Stateless20260728;
            if modern {
                assert!(!head.contains("mcp-session-id:"));
                assert!(head.contains(&format!("mcp-method: {method}")));
                assert_eq!(
                    request["params"]["_meta"][mcp_client::wire::VERSION_META],
                    profile.version()
                );
                assert_eq!(
                    request["params"]["_meta"][mcp_client::wire::CAPABILITIES_META],
                    json!({})
                );
            } else if method != "initialize" {
                assert!(head.contains("mcp-session-id: strict-session"));
            }
            let mut session = "";
            let result = match method {
                "initialize" => {
                    assert!(!modern);
                    seen.initialize.fetch_add(1, Ordering::SeqCst);
                    session = "Mcp-Session-Id: strict-session\r\n";
                    json!({"protocolVersion":profile.version(),"capabilities":{"tools":{}},"serverInfo":{"name":"strict","version":"1"}})
                }
                "notifications/initialized" => {
                    initialized = true;
                    socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    continue;
                }
                "server/discover" => {
                    assert!(modern);
                    json!({"supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":60000,"cacheScope":"private"})
                }
                "tools/list" => {
                    assert!(modern || initialized);
                    seen.list.fetch_add(1, Ordering::SeqCst);
                    json!({"tools":[
                    {"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"region":{"type":"string","x-mcp-header":"Region"}}},"outputSchema":{"type":"array"}},
                    {"name":"invalid","inputSchema":{"oneOf":[{"type":"object","properties":{"bad":{"type":"string","x-mcp-header":"Bad"}}}]}}
                ],"ttlMs":60000,"cacheScope":"private"})
                }
                "tools/call" => {
                    assert!(modern || initialized);
                    seen.call.fetch_add(1, Ordering::SeqCst);
                    if modern {
                        assert!(head.contains("mcp-name: echo"));
                        assert!(head.contains("mcp-param-region: east"));
                    }
                    if fail_call {
                        socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        continue;
                    }
                    let mut value = json!({"content":[{"type":"text","text":"done","annotations":{"audience":["user"]}}],"structuredContent":[1,2],"_meta":{"trace":"kept"}});
                    if modern {
                        value["resultType"] = json!("complete");
                    }
                    value
                }
                _ => panic!("unexpected method {method}"),
            };
            let response = json!({"jsonrpc":"2.0","id":request["id"],"result":result});
            let (kind, body) = if method == "tools/call" {
                (
                    "text/event-stream",
                    format!(
                        ": heartbeat\n\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{}}}}\n\ndata: {response}\n\n"
                    ),
                )
            } else {
                ("application/json", response.to_string())
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            // Split across HTTP/SSE/JSON boundaries to exercise buffered assembly.
            for chunk in reply.as_bytes().chunks(17) {
                socket.write_all(chunk).await.unwrap();
            }
        }
    });
    (url, counts, task)
}

#[tokio::test]
async fn strict_modern_headers_sse_models_cache_partitions_and_concurrency() {
    let (url, counts, server) = server(McpProfile::Stateless20260728, false).await;
    let client = Arc::new(McpGatewayClient::new(&url).unwrap());
    let tools = client.list_tools(Some("Bearer alice")).await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].output_schema.as_ref().unwrap()["type"], "array");
    let mut calls = Vec::new();
    for _ in 0..4 {
        let client = client.clone();
        calls.push(tokio::spawn(async move {
            client
                .call_tool(Some("Bearer alice"), "echo", json!({"region":"east"}))
                .await
                .unwrap()
        }));
    }
    for call in calls {
        let result = call.await.unwrap();
        assert_eq!(result.structured_content, Some(json!([1, 2])));
        assert_eq!(result.extra["_meta"]["trace"], "kept");
        assert_eq!(
            serde_json::to_value(result.content).unwrap()[0]["annotations"]["audience"][0],
            "user"
        );
    }
    assert_eq!(counts.list.load(Ordering::SeqCst), 1);
    client.list_tools(Some("Bearer bob")).await.unwrap();
    assert_eq!(counts.list.load(Ordering::SeqCst), 2);
    assert!(
        client
            .call_tool(Some("Bearer alice"), "invalid", json!({}))
            .await
            .is_err()
    );
    assert_eq!(counts.call.load(Ordering::SeqCst), 4);
    assert_eq!(counts.initialize.load(Ordering::SeqCst), 0);
    client.close(Some("Bearer alice")).await.unwrap();
    server.abort();
}

#[tokio::test]
async fn strict_retained_legacy_profiles_initialize_call_and_cleanup() {
    for profile in [
        McpProfile::Legacy20251125,
        McpProfile::Legacy20250618,
        McpProfile::Legacy20250326,
    ] {
        let (url, counts, server) = server(profile, false).await;
        let client = McpGatewayClient::new(&url).unwrap().with_profile(profile);
        client.list_tools(Some("Bearer alice")).await.unwrap();
        client
            .call_tool(Some("Bearer alice"), "echo", json!({"region":"east"}))
            .await
            .unwrap();
        client.close(Some("Bearer alice")).await.unwrap();
        assert_eq!(counts.initialize.load(Ordering::SeqCst), 1);
        assert_eq!(counts.call.load(Ordering::SeqCst), 1);
        assert_eq!(counts.delete.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[tokio::test]
async fn ambiguous_execution_is_never_replayed_or_downgraded() {
    let (url, counts, server) = server(McpProfile::Stateless20260728, true).await;
    let client = McpGatewayClient::new(&url).unwrap();
    assert!(
        client
            .call_tool(None, "echo", json!({"region":"east"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("503")
    );
    assert_eq!(counts.call.load(Ordering::SeqCst), 1);
    assert_eq!(counts.initialize.load(Ordering::SeqCst), 0);
    server.abort();
}

#[test]
fn wire_rejects_mismatched_ids_incomplete_streams_and_invalid_header_definitions() {
    for body in [
        r#"{"jsonrpc":"2.0","id":"1","result":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#,
    ] {
        assert!(
            mcp_client::wire::response(body.as_bytes(), "application/json", &json!(1)).is_err()
        );
    }
    assert!(
        mcp_client::wire::response(
            b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n",
            "text/event-stream",
            &json!(1)
        )
        .is_err()
    );
    for schema in [
        json!({"$defs":{"unused":{"type":"string","x-mcp-header":"Bad"}}}),
        json!({"properties":{"a":{"type":"number","x-mcp-header":"A"}}}),
        json!({"properties":{"a":{"type":"string","x-mcp-header":"A"},"b":{"type":"string","x-mcp-header":"a"}}}),
    ] {
        assert!(mcp_client::wire::parameter_headers(&schema).is_err());
    }
    assert!(McpProfile::from_version("2024-11-05").is_err());
}

#[tokio::test]
async fn lost_legacy_session_is_cleared_without_replaying_failed_call() {
    for profile in [
        McpProfile::Legacy20250326,
        McpProfile::Legacy20250618,
        McpProfile::Legacy20251125,
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let observed = count.clone();
        let task = tokio::spawn(async move {
            for (step, expected) in [
                "initialize",
                "notifications/initialized",
                "tools/call",
                "initialize",
                "notifications/initialized",
                "tools/call",
            ]
            .iter()
            .enumerate()
            {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let split = loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&bytes[..pos]).to_ascii_lowercase();
                        let size = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .unwrap()
                            .trim()
                            .parse::<usize>()
                            .unwrap();
                        if bytes.len() >= pos + 4 + size {
                            break pos;
                        }
                    }
                };
                let request: Value = serde_json::from_slice(&bytes[split + 4..]).unwrap();
                assert_eq!(request["method"], *expected);
                observed.fetch_add(1, Ordering::SeqCst);
                if *expected == "notifications/initialized" {
                    socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    continue;
                }
                let result = match *expected {
                    "initialize" => {
                        json!({"protocolVersion":profile.version(),"capabilities":{},"serverInfo":{"name":"restart","version":"1"}})
                    }
                    "tools/list" => {
                        json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]})
                    }
                    _ => json!({"content":[{"type":"text","text":"ok"}]}),
                };
                let body = if step == 2 { json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"unknown session","data":{"hint":"restart"}}}) } else { json!({"jsonrpc":"2.0","id":request["id"],"result":result}) }.to_string();
                let status = if step == 2 { "404 Not Found" } else { "200 OK" };
                let session = if *expected == "initialize" {
                    "Mcp-Session-Id: restart-session\r\n"
                } else {
                    ""
                };
                socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{session}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let client = McpGatewayClient::new(&url).unwrap().with_profile(profile);
        let error = client.call_tool(None, "echo", json!({})).await.unwrap_err();
        assert_eq!(count.load(Ordering::SeqCst), 3, "failed call was replayed");
        assert_eq!(
            error
                .downcast_ref::<mcp_client::protocol::JsonRpcError>()
                .unwrap()
                .code,
            -32000
        );
        client.call_tool(None, "echo", json!({})).await.unwrap();
        task.await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 6);
    }
}

#[tokio::test]
async fn more_than_32_credentials_keep_serving_and_legacy_calls_skip_listing() {
    let (url, counts, task) = server(McpProfile::Stateless20260728, false).await;
    let client = McpGatewayClient::new(&url).unwrap();
    for i in 0..40 {
        assert_eq!(
            client
                .list_tools(Some(&format!("Bearer user-{i}")))
                .await
                .unwrap()
                .len(),
            1
        );
    }
    client.list_tools(Some("Bearer user-0")).await.unwrap();
    assert_eq!(counts.list.load(Ordering::SeqCst), 41);
    task.abort();
    for profile in [
        McpProfile::Legacy20250326,
        McpProfile::Legacy20250618,
        McpProfile::Legacy20251125,
    ] {
        let (url, counts, task) = server(profile, false).await;
        let client = McpGatewayClient::new(&url).unwrap().with_profile(profile);
        for _ in 0..3 {
            client
                .call_tool(None, "echo", json!({"region":"east"}))
                .await
                .unwrap();
        }
        assert_eq!(counts.list.load(Ordering::SeqCst), 0);
        assert_eq!(counts.call.load(Ordering::SeqCst), 3);
        assert_eq!(counts.initialize.load(Ordering::SeqCst), 1);
        client.close(None).await.unwrap();
        task.abort();
    }
}
