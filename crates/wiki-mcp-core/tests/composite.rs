//! Составные инструменты на mock Wiki API (`wiremock`): загрузка файла вложением и ожидание
//! фоновой операции. Реальный API, keychain и сеть не используются.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use wiki_api::executor::{
    ApiClient, ClientConfig, Credentials, MAX_FILE_BODY_BYTES, OrgId, RetryPolicy,
};
use wiki_api::{AccessLevel, ApiToken};
use wiki_mcp_core::{
    CREATE_UPLOAD_URL_TOOL, FilePolicy, RequestAuth, ServerOptions, StreamFailure, TokenSource,
    UPLOAD_ATTACHMENT_TOOL, UploadTarget, WAIT_FOR_OPERATION_TOOL, WikiServer,
};
use wiremock::matchers::{any, body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Фиктивный идентификатор сессии загрузки.
const SESSION: &str = "00000000-0000-4000-8000-000000000001";

fn client(api: &MockServer) -> ApiClient {
    let mut config = ClientConfig::new(
        Some(Credentials::oauth(ApiToken::new("test-token-value"))),
        Some(OrgId::Cloud("test-org".into())),
    )
    .unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(30);
    config.max_response_bytes = 64 * 1024;
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    ApiClient::new(config).unwrap()
}

fn server(api: &MockServer, access: AccessLevel, root: Option<&Path>) -> WikiServer {
    let roots: Vec<PathBuf> = root.into_iter().map(Path::to_path_buf).collect();
    WikiServer::new(
        client(api),
        ServerOptions {
            max_access: access,
            ..ServerOptions::default()
        },
        FilePolicy::new(&roots).unwrap(),
    )
    .unwrap()
}

fn args(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => panic!("не объект"),
    }
}

fn text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn call(server: &WikiServer, name: &str, value: Value) -> CallToolResult {
    server.call(name, args(value)).await.unwrap()
}

/// Канонический временный каталог (разрешённый корень) и файл в нём с узнаваемым
/// содержимым.
fn temp_file(name: &str, size: u64) -> (tempfile::TempDir, PathBuf, Vec<u8>) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let file = root.join(name);
    std::fs::write(&file, &content).unwrap();
    (dir, root, content)
}

async fn mount_create(api: &MockServer, file_name: &str, size: u64, storage: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/upload_sessions"))
        .and(body_json(
            json!({"file_name": file_name, "file_size": size}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "session_id": SESSION,
            "file_name": file_name,
            "file_size": size,
            "status": "not_started",
            "storage_type": storage,
        })))
        .expect(1)
        .mount(api)
        .await;
}

async fn mount_step(api: &MockServer, step: &str, status: u16, times: u64) {
    Mock::given(method("POST"))
        .and(path(format!("/v1/upload_sessions/{SESSION}/{step}")))
        .respond_with(ResponseTemplate::new(status).set_body_json(json!({
            "session_id": SESSION,
            "status": if status == 200 { step } else { "error" },
        })))
        .expect(times)
        .mount(api)
        .await;
}

/// Номера и тела загруженных частей в порядке запросов.
async fn uploaded_parts(api: &MockServer) -> Vec<(String, Vec<u8>)> {
    api.received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/upload_part"))
        .map(|r| {
            let number = r
                .url
                .query_pairs()
                .find(|(k, _)| k == "part_number")
                .map(|(_, v)| v.into_owned())
                .unwrap();
            (number, r.body)
        })
        .collect()
}

#[tokio::test]
async fn large_file_is_uploaded_in_parts_and_attached() {
    // Файл больше 2 × 16 МиБ: две полные части и остаток.
    let size = 2 * MAX_FILE_BODY_BYTES + 1000;
    let (_dir, root, content) = temp_file("report.bin", size);
    let api = MockServer::start().await;
    mount_create(&api, "report.bin", size, "s3").await;
    Mock::given(method("PUT"))
        .and(path(format!("/v1/upload_sessions/{SESSION}/upload_part")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "in_progress"})))
        .expect(3)
        .mount(&api)
        .await;
    mount_step(&api, "finish", 200, 1).await;
    mount_step(&api, "abort", 200, 0).await;
    Mock::given(method("POST"))
        .and(path("/v1/pages/42/attachments"))
        .and(body_json(json!({"upload_sessions": [SESSION]})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{"id": 7, "name": "report.bin", "download_url": "/files/report.bin"}]
        })))
        .expect(1)
        .mount(&api)
        .await;

    let server = server(&api, AccessLevel::Write, Some(&root));
    let result = call(
        &server,
        UPLOAD_ATTACHMENT_TOOL,
        // Путь относительно рабочего каталога тоже допустим, но здесь — абсолютный; `idx`
        // строкой из цифр принимается, как у остальных инструментов.
        json!({"idx": "42", "file": root.join("report.bin").display().to_string()}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(body["results"][0]["download_url"], "/files/report.bin");
    assert_eq!(
        body["upload"],
        json!({"session_id": SESSION, "file_name": "report.bin", "file_size": size, "parts": 3})
    );

    let parts = uploaded_parts(&api).await;
    let numbers: Vec<&str> = parts.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(numbers, ["1", "2", "3"]);
    let sizes: Vec<u64> = parts.iter().map(|(_, b)| b.len() as u64).collect();
    assert_eq!(sizes, [MAX_FILE_BODY_BYTES, MAX_FILE_BODY_BYTES, 1000]);
    let mut offset = 0;
    for (_, body) in &parts {
        assert_eq!(body.as_slice(), &content[offset..offset + body.len()]);
        offset += body.len();
    }
    assert_eq!(offset as u64, size);
}

#[tokio::test]
async fn failed_part_aborts_the_session() {
    let size = MAX_FILE_BODY_BYTES + 10;
    let (_dir, root, _) = temp_file("big.bin", size);
    let api = MockServer::start().await;
    mount_create(&api, "Отчёт.bin", size, "s3").await;
    let part_path = format!("/v1/upload_sessions/{SESSION}/upload_part");
    Mock::given(method("PUT"))
        .and(path(part_path.clone()))
        .and(query_param("part_number", "1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "in_progress"})))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("PUT"))
        .and(path(part_path))
        .and(query_param("part_number", "2"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({
            "error_code": "INTERNAL_ERROR",
            "debug_message": "storage is unavailable"
        })))
        .expect(1)
        .mount(&api)
        .await;
    mount_step(&api, "finish", 200, 0).await;
    mount_step(&api, "abort", 200, 1).await;
    Mock::given(method("POST"))
        .and(path("/v1/pages/42/attachments"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&api)
        .await;

    let server = server(&api, AccessLevel::Write, Some(&root));
    let result = call(
        &server,
        UPLOAD_ATTACHMENT_TOOL,
        json!({"idx": 42, "file": root.join("big.bin").display().to_string(), "file_name": " Отчёт.bin "}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    for expected in [
        "step `upload_file_part` (part 2 of 2)",
        "HTTP 500",
        "INTERNAL_ERROR",
        &format!("The upload session `{SESSION}` was aborted."),
    ] {
        assert!(message.contains(expected), "{expected}: {message}");
    }

    // Сервер продолжает работать после ошибки.
    let result = call(
        &server,
        "wait_for_operation",
        json!({"operation_type": "nope", "operation_id": "x"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
}

#[tokio::test]
async fn abort_failure_is_reported() {
    let (_dir, root, content) = temp_file("small.txt", 100);
    let api = MockServer::start().await;
    mount_create(&api, "small.txt", 100, "mds").await;
    Mock::given(method("PUT"))
        .and(path(format!("/v1/upload_sessions/{SESSION}/upload_part")))
        .and(query_param("part_number", "1"))
        .and(wiremock::matchers::body_bytes(content))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "in_progress"})))
        .expect(1)
        .mount(&api)
        .await;
    mount_step(&api, "finish", 200, 1).await;
    // Прикрепление не удалось (страницы нет), отмена — тоже.
    Mock::given(method("POST"))
        .and(path("/v1/pages/404/attachments"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error_code": "NOT_FOUND"})))
        .expect(1)
        .mount(&api)
        .await;
    mount_step(&api, "abort", 500, 1).await;

    let server = server(&api, AccessLevel::Write, Some(&root));
    let result = call(
        &server,
        UPLOAD_ATTACHMENT_TOOL,
        json!({"idx": 404, "file": root.join("small.txt").display().to_string()}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    for expected in [
        "step `attach_uploaded_files`",
        "HTTP 404",
        "Aborting the upload session",
        "failed too",
        "`abort_upload_session`",
    ] {
        assert!(message.contains(expected), "{expected}: {message}");
    }
}

#[tokio::test]
async fn single_request_storage_rejects_large_files() {
    let size = MAX_FILE_BODY_BYTES + 1;
    let (_dir, root, _) = temp_file("big.bin", size);
    let api = MockServer::start().await;
    mount_create(&api, "big.bin", size, "mds").await;
    Mock::given(method("PUT"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&api)
        .await;
    mount_step(&api, "abort", 200, 1).await;

    let server = server(&api, AccessLevel::Write, Some(&root));
    let result = call(
        &server,
        UPLOAD_ATTACHMENT_TOOL,
        json!({"idx": 1, "file": root.join("big.bin").display().to_string()}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("storage `mds`"), "{message}");
    assert!(message.contains("was aborted"), "{message}");
}

#[tokio::test]
async fn bad_files_are_rejected_before_any_request() {
    let (_dir, root, _) = temp_file("empty.bin", 0);
    let (_outside_dir, outside, _) = temp_file("secret.txt", 10);
    let api = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Write, Some(&root));

    let cases = [
        (
            json!({"idx": 1, "file": root.join("empty.bin").display().to_string()}),
            "is empty",
        ),
        (
            json!({"idx": 1, "file": outside.join("secret.txt").display().to_string()}),
            "argument `file`",
        ),
        (
            json!({"idx": 1, "file": root.join("missing.bin").display().to_string()}),
            "argument `file`",
        ),
        (
            json!({"idx": "page", "file": root.join("empty.bin").display().to_string()}),
            "`idx`",
        ),
        (json!({"idx": 1}), "missing required argument `file`"),
        (
            json!({"idx": 1, "file": "x", "body": {}}),
            "unknown argument(s) `body`",
        ),
    ];
    for (value, expected) in cases {
        let result = call(&server, UPLOAD_ATTACHMENT_TOOL, value.clone()).await;
        assert_eq!(result.is_error, Some(true), "{value}");
        let message = text(&result);
        assert!(message.contains(expected), "{value}: {message}");
    }
}

#[tokio::test]
async fn upload_needs_write_access_and_a_local_server() {
    let api = MockServer::start().await;
    let read = server(&api, AccessLevel::Read, None);
    assert!(read.registry().get(UPLOAD_ATTACHMENT_TOOL).is_none());
    let result = call(
        &read,
        UPLOAD_ATTACHMENT_TOOL,
        json!({"idx": 1, "file": "x.bin"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`write` access"),
        "{}",
        text(&result)
    );
    assert!(read.registry().get(WAIT_FOR_OPERATION_TOOL).is_some());

    let remote = WikiServer::with_token_source(
        client(&api),
        ServerOptions {
            max_access: AccessLevel::Destructive,
            local_machine: false,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[std::env::temp_dir()]).unwrap(),
        TokenSource::Client,
    )
    .unwrap();
    assert!(remote.registry().get(UPLOAD_ATTACHMENT_TOOL).is_none());
    assert!(remote.registry().get(WAIT_FOR_OPERATION_TOOL).is_some());
    let result = call(
        &remote,
        UPLOAD_ATTACHMENT_TOOL,
        json!({"idx": 1, "file": "x.bin"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("HTTP mode"), "{}", text(&result));
}

fn status(value: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "status": value,
        "progress": {"percentage": 50.0, "details": "copying"},
        "result": if value == "success" { json!({"slug": "team/copy"}) } else { Value::Null },
    }))
}

#[tokio::test]
async fn wait_returns_the_final_status_after_polling() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/operations/clone/op-1"))
        .respond_with(status("scheduled"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/operations/clone/op-1"))
        .respond_with(status("in_progress"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/operations/clone/op-1"))
        .respond_with(status("success"))
        .expect(1)
        .mount(&api)
        .await;

    let server = server(&api, AccessLevel::Read, None);
    let result = call(
        &server,
        WAIT_FOR_OPERATION_TOOL,
        json!({"operation_type": "clone", "operation_id": "op-1", "timeout_seconds": 20}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(body["status"], "success");
    assert_eq!(body["result"]["slug"], "team/copy");
}

#[tokio::test]
async fn wait_reports_failed_operations() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/operations/move/op-2"))
        .respond_with(status("failed"))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/operations/clone_inline_grid/op-3"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({"error_code": "NOT_FOUND"})))
        .expect(1)
        .mount(&api)
        .await;

    let server = server(&api, AccessLevel::Read, None);
    let result = call(
        &server,
        WAIT_FOR_OPERATION_TOOL,
        json!({"operation_type": "move", "operation_id": "op-2"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("operation `move` `op-2` failed") && message.contains("\"failed\""),
        "{message}"
    );

    let result = call(
        &server,
        WAIT_FOR_OPERATION_TOOL,
        json!({"operation_type": "clone_inline_grid", "operation_id": "op-3"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("get_clone_inline_grid_operation_status") && message.contains("HTTP 404"),
        "{message}"
    );

    for (value, expected) in [
        (
            json!({"operation_type": "copy", "operation_id": "x"}),
            "must be one of",
        ),
        (
            json!({"operation_type": "clone"}),
            "missing required argument `operation_id`",
        ),
        (
            json!({"operation_type": "clone", "operation_id": "x", "timeout_seconds": 1000}),
            "from 1 to 120",
        ),
    ] {
        let result = call(&server, WAIT_FOR_OPERATION_TOOL, value.clone()).await;
        assert_eq!(result.is_error, Some(true), "{value}");
        assert!(
            text(&result).contains(expected),
            "{value}: {}",
            text(&result)
        );
    }
}

#[tokio::test]
async fn wait_times_out_with_the_last_status() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/operations/clone/op-4"))
        .respond_with(status("in_progress"))
        .mount(&api)
        .await;

    let server = server(&api, AccessLevel::Read, None);
    let started = Instant::now();
    let result = call(
        &server,
        WAIT_FOR_OPERATION_TOOL,
        json!({"operation_type": "clone", "operation_id": "op-4", "timeout_seconds": 1}),
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    for expected in [
        "not finished after 1 s",
        "keeps running in the background",
        "\"in_progress\"",
    ] {
        assert!(message.contains(expected), "{expected}: {message}");
    }
    assert!(elapsed >= Duration::from_secs(1), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    // Опросы: сразу, через 0,5 с и на исходе тайм-аута.
    let polls = api.received_requests().await.unwrap().len();
    assert!((2..=4).contains(&polls), "{polls}");
}

// --- Загрузка из потока (одноразовый адрес HTTP-режима) ---

fn stream_target(size: u64) -> UploadTarget {
    UploadTarget {
        idx: 42,
        file_name: "stream.bin".into(),
        file_size: size,
    }
}

fn owner_auth() -> RequestAuth {
    RequestAuth {
        credentials: Credentials::oauth(ApiToken::new("stream-token-value")),
        org: Some(OrgId::Cloud("test-org".into())),
    }
}

#[tokio::test]
async fn stream_upload_attaches_a_file_in_parts() {
    let size = MAX_FILE_BODY_BYTES + 5;
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    let api = MockServer::start().await;
    mount_create(&api, "stream.bin", size, "s3").await;
    Mock::given(method("PUT"))
        .and(path(format!("/v1/upload_sessions/{SESSION}/upload_part")))
        .and(wiremock::matchers::header(
            "authorization",
            "OAuth stream-token-value",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "in_progress"})))
        .expect(2)
        .mount(&api)
        .await;
    mount_step(&api, "finish", 200, 1).await;
    mount_step(&api, "abort", 200, 0).await;
    Mock::given(method("POST"))
        .and(path("/v1/pages/42/attachments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            // Токен в ответе Wiki не доходит до отправителя.
            "results": [{"id": 1, "download_url": "/f", "debug": "stream-token-value"}]
        })))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Write, None);
    let mut reader: &[u8] = &content;
    let text = server
        .upload_stream(owner_auth(), &stream_target(size), &mut reader)
        .await
        .unwrap();
    assert!(!text.contains("stream-token-value"), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["upload"]["parts"], 2);
    let parts = uploaded_parts(&api).await;
    let sizes: Vec<u64> = parts.iter().map(|(_, b)| b.len() as u64).collect();
    assert_eq!(sizes, [MAX_FILE_BODY_BYTES, 5]);
    let joined: Vec<u8> = parts.into_iter().flat_map(|(_, b)| b).collect();
    assert_eq!(joined, content);
}

#[tokio::test]
async fn stream_of_wrong_length_aborts_the_session() {
    for (sent, failure, expected) in [
        (9_usize, StreamFailure::Incomplete, "has 9 bytes"),
        (11, StreamFailure::TooLarge, "longer than 10 bytes"),
    ] {
        let api = MockServer::start().await;
        mount_create(&api, "stream.bin", 10, "s3").await;
        let part_expected = u64::from(sent > 10);
        Mock::given(method("PUT"))
            .and(path(format!("/v1/upload_sessions/{SESSION}/upload_part")))
            .respond_with(ResponseTemplate::new(200))
            .expect(part_expected)
            .mount(&api)
            .await;
        mount_step(&api, "finish", 200, 0).await;
        mount_step(&api, "abort", 200, 1).await;
        let server = server(&api, AccessLevel::Write, None);
        let content = vec![7_u8; sent];
        let mut reader: &[u8] = &content;
        let err = server
            .upload_stream(owner_auth(), &stream_target(10), &mut reader)
            .await
            .unwrap_err();
        assert_eq!(err.failure, failure, "{sent}");
        assert!(err.message.contains(expected), "{sent}: {}", err.message);
        assert!(err.message.contains("was aborted"), "{}", err.message);
    }
}

#[tokio::test]
async fn stream_upload_reports_wiki_errors() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/upload_sessions"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({"error_code": "FORBIDDEN"})))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Write, None);
    let mut reader: &[u8] = b"0123456789";
    let err = server
        .upload_stream(owner_auth(), &stream_target(10), &mut reader)
        .await
        .unwrap_err();
    assert_eq!(err.failure, StreamFailure::Wiki);
    assert!(
        err.message.contains("create_upload_session"),
        "{}",
        err.message
    );
    assert!(err.message.contains("HTTP 403"), "{}", err.message);
}

#[tokio::test]
async fn upload_url_is_only_for_remote_servers() {
    let api = MockServer::start().await;
    let local = server(&api, AccessLevel::Write, None);
    assert!(local.registry().get(CREATE_UPLOAD_URL_TOOL).is_none());
    let result = call(
        &local,
        CREATE_UPLOAD_URL_TOOL,
        json!({"idx": 1, "file_name": "a", "file_size": 1}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("upload_attachment"),
        "{}",
        text(&result)
    );

    let remote = |access| {
        WikiServer::with_token_source(
            client(&api),
            ServerOptions {
                max_access: access,
                local_machine: false,
                ..ServerOptions::default()
            },
            FilePolicy::disabled(),
            TokenSource::Client,
        )
        .unwrap()
    };
    let write = remote(AccessLevel::Write);
    assert!(write.registry().get(CREATE_UPLOAD_URL_TOOL).is_some());
    assert!(write.instructions().contains("call `create_upload_url`"));
    // Без адресов транспорта инструмент отвечает ошибкой, а не паникой.
    let result = call(
        &write,
        CREATE_UPLOAD_URL_TOOL,
        json!({"idx": 1, "file_name": "a", "file_size": 1}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("not available"), "{}", text(&result));
    // Файловый инструмент в HTTP-режиме подсказывает адрес загрузки.
    let result = call(
        &write,
        UPLOAD_ATTACHMENT_TOOL,
        json!({"idx": 1, "file": "x.bin"}),
    )
    .await;
    assert!(
        text(&result).contains("create_upload_url"),
        "{}",
        text(&result)
    );

    let read = remote(AccessLevel::Read);
    assert!(read.registry().get(CREATE_UPLOAD_URL_TOOL).is_none());
    assert!(!read.instructions().contains("create_upload_url`"));
}
