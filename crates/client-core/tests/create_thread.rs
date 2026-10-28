//! `create_thread` (client-api.md §5.2) against a local fake hub: the route, the bearer token,
//! the `Idempotency-Key` header, the exact JSON body, the 201 answer and the error body.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use client_core::{CreateThread, CreateThreadError, FirstPrompt, ThreadCreated, create_thread};
use protocol::tunnel::{ContentPart, Workspace};
use serde_json::{Value, json};

/// One request the fake hub received.
#[derive(Debug, Clone)]
struct Seen {
    authorization: Option<String>,
    idempotency_key: Option<String>,
    body: Value,
}

#[derive(Clone)]
struct Hub {
    seen: Arc<Mutex<Vec<Seen>>>,
    answer: Arc<Mutex<(StatusCode, Value)>>,
}

async fn create(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    };
    hub.seen.lock().expect("lock").push(Seen {
        authorization: header("authorization"),
        idempotency_key: header("idempotency-key"),
        body,
    });
    let (status, answer) = hub.answer.lock().expect("lock").clone();
    (status, Json(answer))
}

/// Starts the fake hub with `answer` as its reply to `POST /v1/threads`. Returns the `…/v1`
/// base URL and the hub's record of what it received.
async fn start(status: StatusCode, answer: Value) -> (String, Hub) {
    let hub = Hub {
        seen: Arc::new(Mutex::new(Vec::new())),
        answer: Arc::new(Mutex::new((status, answer))),
    };
    let app = Router::new()
        .route("/v1/threads", post(create))
        .with_state(hub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let api = format!("http://{}/v1", listener.local_addr().expect("address"));
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serves");
    });
    (api, hub)
}

fn request_with_prompt() -> CreateThread {
    CreateThread {
        machine_id: "mch_laptop".to_owned(),
        agent: "claude".to_owned(),
        workspace: Workspace {
            path: "/src/app".to_owned(),
            branch: None,
            spawn: Some("same_dir".to_owned()),
        },
        permission_mode: Some("default".to_owned()),
        title: Some("Upgrade axum".to_owned()),
        prompt: Some(FirstPrompt {
            task_id: "tk_01".to_owned(),
            message_id: "msg_01".to_owned(),
            parts: vec![
                ContentPart::Text {
                    text: "Upgrade axum to 0.8".to_owned(),
                },
                ContentPart::File {
                    name: "notes.txt".to_owned(),
                    mime_type: "text/plain".to_owned(),
                    url: "https://hub.example/v1/attachments/org_1/att_1?sig=abc".to_owned(),
                    size: Some(46),
                    sha256: None,
                },
            ],
        }),
    }
}

#[tokio::test]
async fn posts_the_first_prompt_with_the_key_and_parses_the_201() {
    let (api, hub) = start(
        StatusCode::CREATED,
        json!({ "thread_id": "th_01", "task_id": "tk_01", "status": "started" }),
    )
    .await;

    let created = create_thread(&api, Some("dev"), "idk_create_1", &request_with_prompt())
        .await
        .expect("creates");

    assert_eq!(
        created,
        ThreadCreated {
            thread_id: "th_01".to_owned(),
            task_id: Some("tk_01".to_owned()),
            status: "started".to_owned(),
        }
    );
    let seen = hub.seen.lock().expect("lock").clone();
    assert_eq!(seen.len(), 1, "one request");
    assert_eq!(seen[0].authorization.as_deref(), Some("Bearer dev"));
    assert_eq!(seen[0].idempotency_key.as_deref(), Some("idk_create_1"));
    assert_eq!(
        seen[0].body,
        json!({
            "machine_id": "mch_laptop",
            "agent": "claude",
            "workspace": { "path": "/src/app", "spawn": "same_dir" },
            "permission_mode": "default",
            "title": "Upgrade axum",
            "prompt": {
                "task_id": "tk_01",
                "message_id": "msg_01",
                "parts": [
                    { "type": "text", "text": "Upgrade axum to 0.8" },
                    {
                        "type": "file",
                        "name": "notes.txt",
                        "mime_type": "text/plain",
                        "url": "https://hub.example/v1/attachments/org_1/att_1?sig=abc",
                        "size": 46
                    }
                ]
            }
        })
    );
}

#[tokio::test]
async fn an_idle_thread_omits_the_prompt_and_the_task_id() {
    let (api, hub) = start(
        StatusCode::CREATED,
        json!({ "thread_id": "th_02", "status": "idle" }),
    )
    .await;
    let mut request = request_with_prompt();
    request.prompt = None;
    request.permission_mode = None;
    request.title = None;

    let created = create_thread(&api, None, "idk_create_2", &request)
        .await
        .expect("creates");

    assert_eq!(created.thread_id, "th_02");
    assert_eq!(created.task_id, None);
    let seen = hub.seen.lock().expect("lock").clone();
    assert_eq!(seen[0].authorization, None, "no token, no bearer header");
    assert_eq!(
        seen[0].body,
        json!({
            "machine_id": "mch_laptop",
            "agent": "claude",
            "workspace": { "path": "/src/app", "spawn": "same_dir" },
        })
    );
}

#[tokio::test]
async fn a_retry_sends_the_same_key() {
    let (api, hub) = start(
        StatusCode::CREATED,
        json!({ "thread_id": "th_03", "task_id": "tk_01", "status": "started" }),
    )
    .await;
    let request = request_with_prompt();

    let first = create_thread(&api, Some("dev"), "idk_create_3", &request)
        .await
        .expect("first");
    let second = create_thread(&api, Some("dev"), "idk_create_3", &request)
        .await
        .expect("retry");

    assert_eq!(first, second);
    let seen = hub.seen.lock().expect("lock").clone();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].idempotency_key, seen[1].idempotency_key);
}

#[tokio::test]
async fn a_refused_create_returns_the_hub_error_body() {
    let (api, _hub) = start(
        StatusCode::BAD_REQUEST,
        json!({ "error": "invalid_request", "message": "workspace.path is required" }),
    )
    .await;

    let error = create_thread(&api, Some("dev"), "idk_create_4", &request_with_prompt())
        .await
        .expect_err("refused");

    match error {
        CreateThreadError::Refused { status, body } => {
            assert_eq!(status, 400);
            assert_eq!(body["error"], "invalid_request");
        }
        other => panic!("expected Refused, got {other:?}"),
    }
}

#[tokio::test]
async fn a_bad_key_is_refused_before_anything_is_sent() {
    let (api, hub) = start(
        StatusCode::CREATED,
        json!({ "thread_id": "th_05", "status": "idle" }),
    )
    .await;

    let error = create_thread(&api, Some("dev"), "not a key!", &request_with_prompt())
        .await
        .expect_err("bad key");

    assert!(matches!(error, CreateThreadError::BadKey(_)), "{error:?}");
    assert!(
        hub.seen.lock().expect("lock").is_empty(),
        "nothing was sent"
    );
}
