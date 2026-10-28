//! Non-child terminal guards only: no rewrite semantics or recovery proof.
use light_client::{ClientConfig, ClientFactory};
use light_lambda_runtime::{
    http_router::{ControllerDispatchFailure, HttpRouterBackend},
    router_java_worker::{
        Artifacts, QualificationLimits,
        recovery::{RecoveryController, RecoveryFailure, RecoveryPolicy},
    },
};
use serverless_aws::{
    alb::{self, DecodeContext},
    limits::Limits,
};
use serverless_core::{ExchangeState, StatusError, Target};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn failed_state_deadline_and_uninitialized_controller_never_reach_transport() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let backend = HttpRouterBackend::new(
        BTreeMap::from([(
            "owned".into(),
            format!("http://{}", listener.local_addr().unwrap()),
        )]),
        &ClientFactory::from_config(&ClientConfig::default()),
        Duration::from_secs(1),
        &Limits::default(),
    )
    .unwrap();
    let capacity = Arc::new(tokio::sync::Semaphore::new(1));
    // No initialize call: artifact paths are never opened, and no worker exists.
    let controller = RecoveryController::new(
        Arc::new(Artifacts {
            java: "/never-executed".into(),
            classpath: vec![],
            pins: vec![],
        }),
        vec![].into(),
        QualificationLimits {
            frame_bytes: 1024,
            input_utf16: 64,
            output_utf16: 64,
            heap_mib: 1,
            stack_kib: 256,
            address_space_bytes: 1 << 20,
        },
        capacity.clone(),
        RecoveryPolicy {
            attempts: 1,
            cooldown: Duration::from_secs(1),
        },
    )
    .unwrap();
    let now = Instant::now();
    let mut inv = alb::decode(serde_json::json!({"requestContext":{"elb":{"targetGroupArn":"owned"}},"httpMethod":"GET","path":"/x","multiValueHeaders":{"service_id":["owned"]}}),
        DecodeContext { request_id: "guards".into(), deadline: now + Duration::from_secs(1), response_deadline: now + Duration::from_secs(2), cancel: CancellationToken::new() }, &Limits::default()).unwrap();
    let target = Target::HttpService("owned".into());
    let mut failed = ExchangeState::default();
    failed.record_request_status(StatusError::new(
        401,
        "ERR10000",
        "INVALID_AUTH_TOKEN",
        "private-first-cause",
    ));
    match backend
        .dispatch_with_controller(&inv, &failed, &target, &controller)
        .await
    {
        Err(ControllerDispatchFailure::Transport(error)) => {
            assert_eq!(error.0.code, "ERR10087");
            assert!(!error.0.description.contains("private-first-cause"));
        }
        _ => panic!("failed state must win before controller/transport"),
    }
    assert_eq!(
        failed.request_failure().unwrap().description,
        "private-first-cause"
    );
    assert!(inv.headers.contains_key("service_id"));
    assert_eq!(inv.path, "/x");
    let fresh = ExchangeState::default();
    assert!(matches!(
        backend
            .dispatch_with_controller(&inv, &fresh, &target, &controller)
            .await,
        Err(ControllerDispatchFailure::Rewrite(
            RecoveryFailure::NotInitialized
        ))
    ));
    inv.deadline = now;
    assert!(matches!(
        backend
            .dispatch_with_controller(&inv, &fresh, &target, &controller)
            .await,
        Err(ControllerDispatchFailure::Transport(_))
    ));
    assert_eq!(capacity.available_permits(), 1);
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}
