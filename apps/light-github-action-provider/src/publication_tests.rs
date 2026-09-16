use super::*;
use axum::{Json, Router, extract::State, routing::get};
use development_workflow_contract::publication::*;
use std::{
    collections::BTreeSet,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Clone, Default)]
struct Remote {
    writes: Arc<AtomicUsize>,
    issues: Arc<Mutex<Vec<Value>>>,
    document: Arc<Mutex<Option<Value>>>,
}
async fn list(State(remote): State<Remote>) -> Json<Value> {
    Json(json!(*remote.issues.lock().await))
}
async fn create(State(remote): State<Remote>, Json(body): Json<Value>) -> StatusCode {
    remote.writes.fetch_add(1, Ordering::SeqCst);
    remote.issues.lock().await.push(json!({"id":11,"number":1,"title":body["title"],"body":body["body"],"user":{"id":7},"html_url":"https://github.com/o/r/issues/1"}));
    // GitHub committed but delivery of its response failed.
    StatusCode::INTERNAL_SERVER_ERROR
}
async fn create_comment(State(remote): State<Remote>, Json(body): Json<Value>) -> StatusCode {
    remote.writes.fetch_add(1, Ordering::SeqCst);
    remote.issues.lock().await.push(json!({"id":22,"body":body["body"],"user":{"id":7},"html_url":"https://github.com/o/r/issues/1#issuecomment-22"}));
    StatusCode::INTERNAL_SERVER_ERROR
}
async fn document(State(remote): State<Remote>) -> (StatusCode, Json<Value>) {
    match remote.document.lock().await.clone() {
        Some(value) => (StatusCode::OK, Json(value)),
        None => (StatusCode::NOT_FOUND, Json(json!({}))),
    }
}
async fn create_document(State(remote): State<Remote>, Json(body): Json<Value>) -> StatusCode {
    remote.writes.fetch_add(1, Ordering::SeqCst);
    *remote.document.lock().await = Some(
        json!({"type":"file","path":"qualification/design.md","sha":"b".repeat(40),"content":body["content"]}),
    );
    StatusCode::INTERNAL_SERVER_ERROR
}

#[tokio::test]
async fn lost_publication_response_restart_and_changed_retry() {
    let remote = Remote::default();
    let app = Router::new()
        .route("/user", get(|| async { Json(json!({"id":7})) }))
        .route("/repos/o/r/issues", get(list).post(create))
        .route(
            "/repos/o/r/issues/1/comments",
            get(list).post(create_comment),
        )
        .route(
            "/repos/o/r/contents/qualification/design.md",
            get(document).put(create_document),
        )
        .route(
            "/repos/o/r/commits",
            get(|| async { Json(json!([{"sha":"a".repeat(40)}])) }),
        )
        .with_state(remote.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("publication.sqlite");
    let db = Connection::open(&path).unwrap();
    db.pragma_update(None, "synchronous", "FULL").unwrap();
    publication::initialize(&db).unwrap();
    let mut state = AppState {
        db: Arc::new(Mutex::new(db)),
        http: reqwest::Client::new(),
        api: Url::parse(&format!("http://{address}/")).unwrap(),
        service_secret: Arc::new(vec![b's'; 32]),
        github_token: Arc::new("fixture-token".into()),
        repositories: Arc::new(BTreeMap::new()),
        branch_prefix: Arc::new("qualification/".into()),
        work_root: Arc::new(dir.path().into()),
        publication_policy: Some(Arc::new(PublicationPolicy {
            repositories: BTreeSet::from(["o/r".into()]),
            document_branches: BTreeSet::new(),
            document_paths: BTreeSet::new(),
            allow_issues: true,
            allow_comments: true,
        })),
    };
    let request = PublicationDelivery {
        key: "a".repeat(64),
        body: "Accepted design".into(),
        plan: PublicationPlan {
            feature_id: "feature".into(),
            slot: "design-issue".into(),
            revision: 1,
            candidate_digest: format!("sha256:{}", "b".repeat(64)),
            repository: "o/r".into(),
            destination: Destination::Issue {
                title: "Qualification".into(),
            },
            content: development_workflow_contract::ArtifactRef {
                id: "retained".into(),
                digest: format!("sha256:{}", "b".repeat(64)),
            },
        },
    };
    assert!(publication::deliver(&state, &request, true).await.is_err());
    assert_eq!(remote.writes.load(Ordering::SeqCst), 1);
    // Reopen the durable journal as a restarted provider would.
    state.db = Arc::new(Mutex::new(Connection::open(&path).unwrap()));
    let receipt = publication::deliver(&state, &request, false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(receipt.key, request.key);
    assert_eq!(receipt.request_digest, request.request_digest().unwrap());
    assert_eq!(
        publication::deliver(&state, &request, true).await.unwrap(),
        Some(receipt)
    );
    assert_eq!(remote.writes.load(Ordering::SeqCst), 1);
    let mut changed = request.clone();
    changed.body = "Changed design".into();
    assert!(publication::deliver(&state, &changed, true).await.is_err());
    let mut missing = request.clone();
    missing.key = "c".repeat(64);
    assert!(
        publication::deliver(&state, &missing, false)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(remote.writes.load(Ordering::SeqCst), 1);
    Arc::make_mut(state.publication_policy.as_mut().unwrap())
        .document_branches
        .insert("qualification/publication".into());
    Arc::make_mut(state.publication_policy.as_mut().unwrap())
        .document_paths
        .insert("qualification/design.md".into());
    for (key, destination) in [
        ("d", Destination::Comment { issue: 1 }),
        (
            "e",
            Destination::Document {
                branch: "qualification/publication".into(),
                path: "qualification/design.md".into(),
                expected_blob: None,
            },
        ),
    ] {
        let mut selected = request.clone();
        selected.key = key.repeat(64);
        selected.plan.slot = format!("publication-{key}");
        selected.plan.destination = destination;
        let before = remote.writes.load(Ordering::SeqCst);
        assert!(publication::deliver(&state, &selected, true).await.is_err());
        assert_eq!(remote.writes.load(Ordering::SeqCst), before + 1);
        state.db = Arc::new(Mutex::new(Connection::open(&path).unwrap()));
        let receipt = publication::deliver(&state, &selected, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.request_digest, selected.request_digest().unwrap());
        if matches!(selected.plan.destination, Destination::Document { .. }) {
            assert_eq!(receipt.commit.as_deref(), Some("a".repeat(40).as_str()));
        }
        assert_eq!(
            publication::deliver(&state, &selected, true).await.unwrap(),
            Some(receipt)
        );
        assert_eq!(remote.writes.load(Ordering::SeqCst), before + 1);
    }
    state.publication_policy = None;
    assert!(publication::deliver(&state, &request, false).await.is_err());
    server.abort();
}
