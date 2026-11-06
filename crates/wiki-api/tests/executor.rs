//! Тесты исполнителя запросов на `wiremock`, без реальной сети.

use std::time::{Duration, Instant};

use serde_json::json;
use wiki_api::ApiToken;
use wiki_api::HttpMethod;
use wiki_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, Credentials, MAX_FILE_BODY_BYTES, OrgId,
    RequestBody, ResponseBody, RetryPolicy,
};
use wiremock::matchers::{body_bytes, body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Узнаваемое фиктивное значение, чтобы искать утечки в текстах ошибок.
const TOKEN: &str = "test-token-LEAKCHECK-7f3a9";
/// Фиктивный идентификатор организации.
const ORG: &str = "0000001";
/// Фиктивный идентификатор сессии загрузки.
const SESSION: &str = "00000000-0000-4000-8000-000000000001";

fn config(base: &str) -> ClientConfig {
    ClientConfig {
        base_url: url::Url::parse(base).unwrap(),
        credentials: Some(Credentials::oauth(ApiToken::new(TOKEN))),
        org: Some(OrgId::Yandex360(ORG.into())),
        timeout: Duration::from_secs(5),
        max_response_bytes: 64 * 1024,
        retry: RetryPolicy {
            max_retries: 2,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_secs(5),
        },
    }
}

fn client(server: &MockServer) -> ApiClient {
    ApiClient::new(config(&server.uri())).unwrap()
}

fn get(path: &str) -> ApiRequest {
    ApiRequest::new(HttpMethod::Get, path)
}

fn upload_part(file: std::path::PathBuf) -> ApiRequest {
    let mut request = ApiRequest::new(
        HttpMethod::Put,
        format!("/v1/upload_sessions/{SESSION}/upload_part"),
    );
    request.query = vec![("part_number".into(), "1".into())];
    request.body = RequestBody::File(file);
    request
}

fn http_error(err: ApiError) -> wiki_api::executor::HttpError {
    match err {
        ApiError::Http(e) => *e,
        other => panic!("ожидалась Http, получено {other:?}"),
    }
}

#[tokio::test]
async fn sends_oauth_org_and_user_agent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/users/me"))
        .and(header("authorization", format!("OAuth {TOKEN}").as_str()))
        .and(header("x-org-id", ORG))
        .and(header(
            "user-agent",
            concat!("wiki-mcp/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(|req: &Request| {
            // Заголовок другой схемы организации не отправляется.
            assert!(req.headers.get("x-cloud-org-id").is_none());
            ResponseTemplate::new(200).set_body_json(json!({"username": "me"}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let response = client(&server).execute(get("/v1/users/me")).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, ResponseBody::Json(json!({"username": "me"})));
}

#[tokio::test]
async fn sends_iam_bearer_and_cloud_org() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/users/me"))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(header("x-cloud-org-id", "bpf-cloud-org"))
        .respond_with(|req: &Request| {
            assert!(req.headers.get("x-org-id").is_none());
            ResponseTemplate::new(200).set_body_json(json!({"ok": true}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.credentials = Some(Credentials::iam(ApiToken::new(TOKEN)));
    cfg.org = Some(OrgId::Cloud("bpf-cloud-org".into()));
    let response = ApiClient::new(cfg)
        .unwrap()
        .execute(get("/v1/users/me"))
        .await
        .unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
}

#[tokio::test]
async fn organization_from_profile_sets_its_header() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/users/me"))
        .and(header("x-cloud-org-id", "bpf-profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let org = wiki_api::Organization::new(wiki_api::OrgKind::Cloud, "bpf-profile").unwrap();
    let mut cfg = config(&server.uri());
    cfg.org = Some(OrgId::from(&org));
    ApiClient::new(cfg)
        .unwrap()
        .execute(get("/v1/users/me"))
        .await
        .unwrap();
}

#[tokio::test]
async fn missing_token_or_org_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.credentials = None;
    let err = ApiClient::new(cfg)
        .unwrap()
        .execute(get("/v1/users/me"))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::MissingToken), "{err:?}");

    let mut cfg = config(&server.uri());
    cfg.org = None;
    let no_org = ApiClient::new(cfg).unwrap();
    let err = no_org.execute(get("/v1/users/me")).await.unwrap_err();
    assert!(matches!(err, ApiError::MissingOrg), "{err:?}");

    // Организацию можно задать копии клиента.
    let with_org = no_org.with_org(OrgId::Yandex360("1".into())).unwrap();
    assert!(with_org.config().org.is_some());
    for bad in ["", "1 2", "1\n", "один"] {
        let err = no_org.with_org(OrgId::Yandex360(bad.into())).unwrap_err();
        assert!(
            matches!(err, ApiError::InvalidConfig(_)),
            "{bad:?}: {err:?}"
        );
    }
}

#[tokio::test]
async fn passes_query_parameters_as_is() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/descendants"))
        .and(query_param("slug", "users/me/абв & x=y"))
        .and(query_param("page_size", "10"))
        .and(query_param("cursor", "eyJpZCI6IDF9"))
        .and(query_param("fields", "attributes,breadcrumbs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [], "next_cursor": null, "prev_cursor": "abc"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut request = get("/v1/pages/descendants");
    request.query = vec![
        ("slug".into(), "users/me/абв & x=y".into()),
        ("page_size".into(), "10".into()),
        ("cursor".into(), "eyJpZCI6IDF9".into()),
        ("fields".into(), "attributes,breadcrumbs".into()),
    ];
    let response = client(&server).execute(request).await.unwrap();
    // Курсоры разбивки остаются в теле ответа как есть.
    assert_eq!(
        response.body,
        ResponseBody::Json(json!({"results": [], "next_cursor": null, "prev_cursor": "abc"}))
    );
}

#[tokio::test]
async fn keeps_base_url_path_prefix() {
    let server = MockServer::start().await;
    Mock::given(path("/proxy/v1/users/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(2)
        .mount(&server)
        .await;

    for base in [
        format!("{}/proxy", server.uri()),
        format!("{}/proxy/", server.uri()),
    ] {
        let client = ApiClient::new(config(&base)).unwrap();
        let response = client.execute(get("/v1/users/me")).await.unwrap();
        assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
    }
}

#[tokio::test]
async fn json_body_is_sent_as_object() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .and(header("content-type", "application/json"))
        .and(body_json(
            json!({"page_type": "page", "slug": "a/b", "title": "T"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .expect(2)
        .mount(&server)
        .await;

    let client = client(&server);
    let mut request = ApiRequest::new(HttpMethod::Post, "/v1/pages");
    request.body = RequestBody::Json(json!({"page_type": "page", "slug": "a/b", "title": "T"}));
    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(response.status, 200);

    // Регрессия: тело, переданное строкой с JSON, уходит объектом, а не строкой.
    request.body = RequestBody::Json(json!(
        r#"{"page_type": "page", "slug": "a/b", "title": "T"}"#
    ));
    client.execute(request).await.unwrap();
}

#[tokio::test]
async fn file_body_is_sent_raw() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    Mock::given(method("PUT"))
        .and(path(format!("/v1/upload_sessions/{SESSION}/upload_part")))
        .and(query_param("part_number", "1"))
        .and(header("content-type", "application/octet-stream"))
        .and(body_bytes(payload.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": "in_progress"})))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("part-1.bin");
    std::fs::write(&file, &payload).unwrap();

    let response = client(&server).execute(upload_part(file)).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::Json(json!({"status": "in_progress"}))
    );
}

#[tokio::test]
async fn file_body_is_checked_before_sending() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let client = client(&server);

    // Пустой файл и файл больше 16 МиБ отклоняются с понятной ошибкой.
    let empty = base.join("empty.bin");
    std::fs::write(&empty, b"").unwrap();
    let big = base.join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(MAX_FILE_BODY_BYTES + 1)
        .unwrap();
    for (file, size) in [(empty, 0), (big, MAX_FILE_BODY_BYTES + 1)] {
        let err = client.execute(upload_part(file)).await.unwrap_err();
        match &err {
            ApiError::FileBodySize { size: got, max, .. } => {
                assert_eq!(*got, size);
                assert_eq!(*max, 16 * 1024 * 1024);
            }
            other => panic!("ожидалась FileBodySize, получено {other:?}"),
        }
        let text = err.to_string();
        assert!(text.contains("16 МиБ") && text.contains("5 МиБ"), "{text}");
    }

    // Относительный путь, отсутствующий файл и каталог.
    let err = client
        .execute(upload_part("relative.bin".into()))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    let err = client
        .execute(upload_part(base.join("missing.bin")))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Io(_)), "{err:?}");
    let err = client.execute(upload_part(base.clone())).await.unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
}

#[tokio::test]
async fn file_body_of_max_size_is_accepted() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .respond_with(|req: &Request| {
            assert_eq!(req.body.len() as u64, MAX_FILE_BODY_BYTES);
            ResponseTemplate::new(200).set_body_json(json!({"ok": true}))
        })
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("max.bin");
    std::fs::File::create(&file)
        .unwrap()
        .set_len(MAX_FILE_BODY_BYTES)
        .unwrap();
    client(&server).execute(upload_part(file)).await.unwrap();
}

#[tokio::test]
async fn file_part_body_sends_only_the_range() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    Mock::given(method("PUT"))
        .and(path(format!("/v1/upload_sessions/{SESSION}/upload_part")))
        .and(header("content-type", "application/octet-stream"))
        .and(body_bytes(payload[40_000..70_000].to_vec()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("whole.bin");
    std::fs::write(&file, &payload).unwrap();
    let client = client(&server);

    let mut request = upload_part(file.clone());
    request.body = RequestBody::FilePart {
        path: file.clone(),
        offset: 40_000,
        len: 30_000,
    };
    client.execute(request).await.unwrap();

    // Диапазон за концом файла, пустой и слишком большой диапазон не отправляются.
    for (offset, len) in [
        (90_000, 20_000),
        (0, 0),
        (0, MAX_FILE_BODY_BYTES + 1),
        (u64::MAX, 1),
    ] {
        let mut request = upload_part(file.clone());
        request.body = RequestBody::FilePart {
            path: file.clone(),
            offset,
            len,
        };
        let err = client.execute(request).await.unwrap_err();
        assert!(
            matches!(
                err,
                ApiError::InvalidRequest(_) | ApiError::FileBodySize { .. }
            ),
            "{offset}+{len}: {err:?}"
        );
    }
}

#[tokio::test]
async fn wiki_errors_are_parsed() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/pages/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error_code": "NOT_FOUND",
            "debug_message": "Page not found",
            "details": {}
        })))
        .mount(&server)
        .await;
    Mock::given(path("/v1/pages"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error_code": "VALIDATION_ERROR",
            "debug_message": "Validation failed",
            "message": "url: Строка не соответствует шаблону",
            "details": {"body": {"title": [{"debug_message": "field required"}]}}
        })))
        .mount(&server)
        .await;
    Mock::given(path("/v1/plain-error"))
        .respond_with(ResponseTemplate::new(502).set_body_string("Bad gateway"))
        .mount(&server)
        .await;
    Mock::given(path("/v1/huge-error"))
        .respond_with(ResponseTemplate::new(500).set_body_string("x".repeat(100_000)))
        .mount(&server)
        .await;

    let client = client(&server);
    let err = client.execute(get("/v1/pages/404")).await.unwrap_err();
    assert_eq!(err.status(), Some(404));
    let text = err.to_string();
    assert!(
        text.contains("404") && text.contains("NOT_FOUND") && text.contains("Page not found"),
        "{text}"
    );
    assert!(!text.contains("details"), "{text}");
    let e = http_error(err);
    assert_eq!(e.error_code.as_deref(), Some("NOT_FOUND"));
    assert_eq!(e.debug_message.as_deref(), Some("Page not found"));
    assert_eq!(e.details, None);

    let mut request = ApiRequest::new(HttpMethod::Post, "/v1/pages");
    request.body = RequestBody::Json(json!({}));
    let err = client.execute(request).await.unwrap_err();
    assert!(err.to_string().contains("field required"), "{err}");
    let e = http_error(err);
    assert_eq!(e.status, 400);
    assert_eq!(e.error_code.as_deref(), Some("VALIDATION_ERROR"));
    assert_eq!(
        e.message.as_deref(),
        Some("url: Строка не соответствует шаблону")
    );
    assert_eq!(
        e.details,
        Some(json!({"body": {"title": [{"debug_message": "field required"}]}}))
    );

    let err = client.execute(get("/v1/plain-error")).await.unwrap_err();
    assert!(err.to_string().contains("Bad gateway"), "{err}");
    let e = http_error(err);
    assert_eq!(e.status, 502);
    assert!(e.error_code.is_none() && e.debug_message.is_none());

    let e = http_error(client.execute(get("/v1/huge-error")).await.unwrap_err());
    assert_eq!(e.status, 500);
    assert!(
        e.body.len() < 9000,
        "тело ошибки должно обрезаться: {}",
        e.body.len()
    );
    assert!(e.body.ends_with("(обрезано)"));
}

#[tokio::test]
async fn retries_429_with_retry_after() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/users/me"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/v1/users/me"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .expect(1)
        .mount(&server)
        .await;

    let started = Instant::now();
    let response = client(&server).execute(get("/v1/users/me")).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"ok": true})));
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "пауза Retry-After не выдержана: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn retry_limits() {
    let server = MockServer::start().await;
    // Без Retry-After: пауза по умолчанию, после исчерпания попыток — ошибка 429.
    Mock::given(path("/v1/always-429"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "error_code": "TOO_MANY_REQUESTS",
            "debug_message": "Too many requests"
        })))
        .expect(3)
        .mount(&server)
        .await;
    // Retry-After больше max_delay: не ждём, сразу возвращаем ошибку с подсказкой.
    Mock::given(path("/v1/long-wait"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
        .expect(1)
        .mount(&server)
        .await;
    // 503 повторяется для GET, но не для POST.
    Mock::given(method("GET"))
        .and(path("/v1/unavailable"))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .expect(3)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/unavailable"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    // Прочие ошибки не повторяются.
    Mock::given(path("/v1/server-error"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let e = http_error(client.execute(get("/v1/always-429")).await.unwrap_err());
    assert_eq!(e.status, 429);
    assert_eq!(e.error_code.as_deref(), Some("TOO_MANY_REQUESTS"));
    assert_eq!(e.retry_after, None);

    let err = client.execute(get("/v1/long-wait")).await.unwrap_err();
    assert!(err.to_string().contains("3600"), "{err}");
    assert_eq!(http_error(err).retry_after, Some(3600));

    let e = http_error(client.execute(get("/v1/unavailable")).await.unwrap_err());
    assert_eq!(e.status, 503);
    let post = ApiRequest::new(HttpMethod::Post, "/v1/unavailable");
    assert_eq!(client.execute(post).await.unwrap_err().status(), Some(503));
    assert_eq!(
        client
            .execute(get("/v1/server-error"))
            .await
            .unwrap_err()
            .status(),
        Some(500)
    );
}

#[tokio::test]
async fn retry_resends_file_body() {
    let server = MockServer::start().await;
    let upload = format!("/v1/upload_sessions/{SESSION}/upload_part");
    Mock::given(path(upload.as_str()))
        .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(path(upload.as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let file = base.join("part.bin");
    std::fs::write(&file, "retry-body-42").unwrap();
    client(&server).execute(upload_part(file)).await.unwrap();

    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 2);
    for req in &received {
        assert_eq!(req.body, b"retry-body-42");
    }
}

#[tokio::test]
async fn large_response_is_rejected_with_hint() {
    let server = MockServer::start().await;
    let big = format!("{{\"data\": \"{}\"}}", "a".repeat(10_000));
    Mock::given(path("/v1/big"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(big, "application/json"))
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.max_response_bytes = 1000;
    let client = ApiClient::new(cfg).unwrap();

    let err = client.execute(get("/v1/big")).await.unwrap_err();
    assert!(
        matches!(err, ApiError::ResponseTooLarge { limit: 1000 }),
        "{err:?}"
    );
    let text = err.to_string();
    assert!(
        text.contains("fields") && text.contains("page_size"),
        "{text}"
    );

    // Лимит отдельного запроса заменяет лимит конфигурации.
    let response = client
        .execute_with_limit(get("/v1/big"), 64 * 1024)
        .await
        .unwrap();
    assert!(matches!(response.body, ResponseBody::Json(_)));
}

#[test]
fn default_response_limit_is_one_mebibyte() {
    let cfg = ClientConfig::new(None, None).unwrap();
    assert_eq!(cfg.max_response_bytes, 1024 * 1024);
}

#[tokio::test]
async fn text_and_untyped_json_responses() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/text"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("hello", "text/plain"))
        .mount(&server)
        .await;
    Mock::given(path("/v1/untyped"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"{\"a\":1}".to_vec()))
        .mount(&server)
        .await;
    Mock::given(path("/v1/empty"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let client = client(&server);
    assert_eq!(
        client.execute(get("/v1/text")).await.unwrap().body,
        ResponseBody::Text("hello".into())
    );
    assert_eq!(
        client.execute(get("/v1/untyped")).await.unwrap().body,
        ResponseBody::Json(json!({"a": 1}))
    );
    assert_eq!(
        client.execute(get("/v1/empty")).await.unwrap().body,
        ResponseBody::Empty
    );
}

#[tokio::test]
async fn binary_response_without_save_to_returns_note() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/blob"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(vec![0u8, 159, 146, 150], "application/octet-stream"),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/sniffed"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0u8, 159, 146, 150]))
        .mount(&server)
        .await;

    let client = client(&server);
    for p in ["/v1/blob", "/v1/sniffed"] {
        match client.execute(get(p)).await.unwrap().body {
            ResponseBody::Text(note) => {
                assert!(note.contains("двоичный"), "{note}");
                assert!(note.contains("save_to"), "{note}");
                assert!(note.contains("4 байт"), "{note}");
            }
            other => panic!("ожидалось пояснение, получено {other:?}"),
        }
    }
}

#[tokio::test]
async fn save_to_downloads_attachment_and_refuses_overwrite() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let download = "/v1/pages/12/attachments/34/download";
    Mock::given(path(download))
        .respond_with(ResponseTemplate::new(200).set_body_raw(payload.clone(), "image/png"))
        .mount(&server)
        .await;
    Mock::given(path("/v1/fail"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error_code": "FORBIDDEN", "debug_message": "no access"
        })))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let target = base.join("picture.png");
    let mut cfg = config(&server.uri());
    // Лимит ответа не распространяется на сохранение в файл.
    cfg.max_response_bytes = 10;
    let client = ApiClient::new(cfg).unwrap();

    let request = ApiRequest {
        save_to: Some(target.clone()),
        ..get(download)
    };
    let response = client.execute(request.clone()).await.unwrap();
    assert_eq!(
        response.body,
        ResponseBody::SavedToFile {
            path: target.clone(),
            bytes: payload.len() as u64,
            content_type: Some("image/png".into()),
        }
    );
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // Повторная запись в существующий файл запрещена.
    let err = client.execute(request).await.unwrap_err();
    assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    assert_eq!(std::fs::read(&target).unwrap(), payload);

    // Каталог должен существовать, путь — быть абсолютным.
    for bad in [
        base.join("no-such-dir").join("x.bin"),
        std::path::PathBuf::from("relative.bin"),
    ] {
        let err = client
            .execute(ApiRequest {
                save_to: Some(bad),
                ..get(download)
            })
            .await
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
    }

    // При ошибке API файл не создаётся.
    let failed = base.join("failed.bin");
    let err = client
        .execute(ApiRequest {
            save_to: Some(failed.clone()),
            ..get("/v1/fail")
        })
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(403));
    assert!(!failed.exists());
}

#[tokio::test]
async fn invalid_paths_never_reach_server() {
    let server = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let client = client(&server);
    let absolute = format!("{}/v1/pages", server.uri());
    for p in [
        "https://evil.example/v1/pages",
        "https://api.wiki.yandex.net/v1/pages",
        absolute.as_str(),
        "//evil.example/v1/pages",
        "/v1/pages/../../admin",
        "/v1/pages/%2e%2e/admin",
        "/v1//pages",
        "/v1/pages?slug=x",
        "/v2/pages",
        "/v3/issues",
        "/admin",
        "v1/pages",
    ] {
        let err = client.execute(get(p)).await.unwrap_err();
        assert!(matches!(err, ApiError::InvalidPath(_)), "{p}: {err:?}");
    }
}

#[tokio::test]
async fn cross_origin_redirect_is_not_followed() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&other)
        .await;
    Mock::given(path("/v1/external"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/steal?sig=secret", other.uri())),
        )
        .mount(&server)
        .await;
    Mock::given(path("/v1/internal"))
        .respond_with(ResponseTemplate::new(301).insert_header("location", "/v1/users/me"))
        .mount(&server)
        .await;
    Mock::given(path("/v1/users/me"))
        .and(header("authorization", format!("OAuth {TOKEN}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"username": "me"})))
        .mount(&server)
        .await;

    let client = client(&server);
    let e = http_error(client.execute(get("/v1/external")).await.unwrap_err());
    assert_eq!(e.status, 302);
    assert!(!e.body.contains("sig=secret"), "{}", e.body);
    let response = client.execute(get("/v1/internal")).await.unwrap();
    assert_eq!(response.body, ResponseBody::Json(json!({"username": "me"})));
}

#[tokio::test]
async fn with_credentials_uses_its_own_token_per_copy() {
    let server = MockServer::start().await;
    for token in ["token-agent-a", "token-agent-b"] {
        Mock::given(path("/v1/users/me"))
            .and(header("authorization", format!("OAuth {token}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"token": token})))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(path("/v1/echo"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(403).set_body_string(format!("denied: {auth}"))
        })
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.credentials = None;
    let base = ApiClient::new(cfg).unwrap();
    let a = base
        .with_credentials(Credentials::oauth(ApiToken::new("token-agent-a")))
        .unwrap();
    let b = base
        .with_credentials(Credentials::oauth(ApiToken::new("token-agent-b")))
        .unwrap();
    a.execute(get("/v1/users/me")).await.unwrap();
    b.execute(get("/v1/users/me")).await.unwrap();
    // Исходный клиент токен не получил.
    assert!(matches!(
        base.execute(get("/v1/users/me")).await.unwrap_err(),
        ApiError::MissingToken
    ));

    // Токен копии вырезается из ошибок и из текстов через scrub_token.
    let err = a.execute(get("/v1/echo")).await.unwrap_err();
    assert!(!format!("{err}").contains("token-agent-a"), "{err}");
    assert_eq!(a.scrub_token("x token-agent-a y".into()), "x *** y");

    // Недопустимые значения отклоняются без отражения в ошибке.
    for bad in ["", "  ", "bad\ntoken"] {
        let err = base
            .with_credentials(Credentials::oauth(ApiToken::new(bad)))
            .unwrap_err();
        assert!(matches!(err, ApiError::InvalidConfig(_)), "{err:?}");
        assert!(!format!("{err:?}").contains("bad\ntoken"));
    }
}

#[tokio::test]
async fn token_never_leaks() {
    // Конфигурация и клиент в Debug не показывают токен и org id.
    let cfg = config("https://api.example");
    let debug = format!("{cfg:?}");
    assert!(!debug.contains(TOKEN) && !debug.contains(ORG), "{debug}");
    let client = ApiClient::new(cfg).unwrap();
    let debug = format!("{client:?}");
    assert!(!debug.contains(TOKEN) && !debug.contains(ORG), "{debug}");

    // Сервер отражает заголовок Authorization в теле ошибки (в error_code, debug_message
    // и details) и в успешном ответе.
    let server = MockServer::start().await;
    Mock::given(path("/v1/echo-error"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(401).set_body_json(json!({
                "error_code": format!("BAD_{auth}"),
                "debug_message": format!("bad credentials: {auth}"),
                "details": {"headers": [{"authorization": auth}], auth: true}
            }))
        })
        .mount(&server)
        .await;
    Mock::given(path("/v1/echo-text"))
        .respond_with(|req: &Request| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(200).set_body_raw(format!("you sent {auth}"), "text/plain")
        })
        .mount(&server)
        .await;

    let client = ApiClient::new(config(&server.uri())).unwrap();
    let mut outputs = Vec::new();

    let err = client.execute(get("/v1/echo-error")).await.unwrap_err();
    outputs.push(format!("{err}"));
    outputs.push(format!("{err:?}"));
    let ok = client.execute(get("/v1/echo-text")).await.unwrap();
    outputs.push(format!("{ok:?}"));

    // Ошибки пути, параметров, ввода-вывода.
    for e in [
        client.execute(get("/v1/../x")).await.unwrap_err(),
        client
            .execute(upload_part("/nonexistent/file".into()))
            .await
            .unwrap_err(),
    ] {
        outputs.push(format!("{e}"));
        outputs.push(format!("{e:?}"));
    }

    // Порт, на котором никто не слушает: соединение отклоняется.
    let closed = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let unreachable = ApiClient::new(config(&format!("http://127.0.0.1:{closed}"))).unwrap();
    let transport = unreachable.execute(get("/v1/users/me")).await.unwrap_err();
    assert!(matches!(transport, ApiError::Transport(_)), "{transport:?}");
    outputs.push(format!("{transport}"));
    outputs.push(format!("{transport:?}"));

    for out in &outputs {
        assert!(!out.contains(TOKEN), "токен утёк: {out}");
        assert!(!out.contains(ORG), "org id утёк: {out}");
    }
    assert!(outputs[0].contains("***"), "{}", outputs[0]);
}

#[tokio::test]
async fn timeout_maps_to_transport_error() {
    let server = MockServer::start().await;
    Mock::given(path("/v1/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;

    let mut cfg = config(&server.uri());
    cfg.timeout = Duration::from_millis(200);
    let client = ApiClient::new(cfg).unwrap();
    match client.execute(get("/v1/slow")).await.unwrap_err() {
        ApiError::Transport(text) => assert!(text.contains("таймаут"), "{text}"),
        other => panic!("ожидалась Transport, получено {other:?}"),
    }
}

#[test]
fn default_config_targets_wiki() {
    let cfg = ClientConfig::new(None, None).unwrap();
    assert_eq!(cfg.base_url.as_str(), "https://api.wiki.yandex.net/");
    assert!(ApiClient::new(cfg).is_ok());
}

#[test]
fn rejects_insecure_or_invalid_config() {
    for base in [
        "http://api.wiki.yandex.net",
        "https://user:pw@api.example",
        "ftp://api.example",
    ] {
        let err = ApiClient::new(config(base)).unwrap_err();
        assert!(matches!(err, ApiError::InvalidConfig(_)), "{base}: {err:?}");
    }
    let mut cfg = config("https://api.example");
    cfg.credentials = Some(Credentials::oauth(ApiToken::new("bad\ntoken")));
    let err = ApiClient::new(cfg).unwrap_err();
    assert!(matches!(err, ApiError::InvalidConfig(_)));
    assert!(!format!("{err:?}").contains("bad\ntoken"));

    let mut cfg = config("https://api.example");
    cfg.org = Some(OrgId::Cloud("bad org\n".into()));
    let err = ApiClient::new(cfg).unwrap_err();
    assert!(matches!(err, ApiError::InvalidConfig(_)));
    assert!(!format!("{err}").contains("bad org"));
}
