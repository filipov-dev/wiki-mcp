//! Фильтр ответа `jq`: инструменты операций и `api_request`, курсоры постраничной выдачи,
//! вырезание учётных данных до фильтра, ошибки до и после запроса. Wiki API подменён
//! `wiremock`.

use std::time::Duration;

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{Value, json};
use wiki_api::executor::{ApiClient, ClientConfig, Credentials, OrgId, RetryPolicy};
use wiki_api::{AccessLevel, ApiToken};
use wiki_mcp_core::{FilePolicy, ServerOptions, WikiServer};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "test-token-value";

fn server(api: &MockServer, max_access: AccessLevel, max_response_bytes: usize) -> WikiServer {
    let mut config = ClientConfig::new(
        Some(Credentials::oauth(ApiToken::new(TEST_TOKEN))),
        Some(OrgId::Cloud("test-org".into())),
    )
    .unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.max_response_bytes = max_response_bytes;
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    WikiServer::new(
        ApiClient::new(config).unwrap(),
        ServerOptions {
            max_access,
            toolsets: Some(vec!["all".into()]),
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
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

fn descendants(next: Value) -> Value {
    json!({
        "results": [
            {"id": 1, "slug": "team/a", "title": "A", "content": "long text"},
            {"id": 2, "slug": "team/b", "title": "B", "content": "more text"}
        ],
        "next_cursor": next,
        "prev_cursor": null
    })
}

/// Главный сценарий: `.results[] | {slug, title}` на списке подстраниц возвращает только эти
/// поля, а курсоры выдачи остаются рядом с результатом — и на последней странице тоже.
#[tokio::test]
async fn filters_list_and_keeps_cursors() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/descendants"))
        .and(query_param("slug", "team"))
        .and(query_param("page_size", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(descendants(json!("c2"))))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/descendants"))
        .and(query_param("cursor", "c2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(descendants(Value::Null)))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, 64 * 1024);
    let tool = server
        .registry()
        .get("list_page_descendants")
        .unwrap()
        .tool
        .clone();
    assert!(tool.input_schema["properties"].get("jq").is_some());

    let result = call(
        &server,
        "list_page_descendants",
        json!({"slug": "team", "page_size": 2, "jq": ".results[] | {slug, title}"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(
        value,
        json!({
            "result": [{"slug": "team/a", "title": "A"}, {"slug": "team/b", "title": "B"}],
            "pagination": {"next_cursor": "c2", "prev_cursor": null}
        })
    );

    let result = call(
        &server,
        "list_page_descendants",
        json!({"slug": "team", "cursor": "c2", "jq": "[.results[] | .id]"}),
    )
    .await;
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["result"], json!([1, 2]));
    assert_eq!(value["pagination"]["next_cursor"], Value::Null);
}

#[tokio::test]
async fn filters_plain_responses_and_api_request() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42,
            "slug": "team/a",
            "title": "A",
            "attributes": {"created_at": "2026-01-01"}
        })))
        .expect(3)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, 64 * 1024);

    let result = call(
        &server,
        "get_page_by_id",
        json!({"idx": 42, "jq": "{slug, created: .attributes.created_at}"}),
    )
    .await;
    assert_eq!(text(&result), r#"{"slug":"team/a","created":"2026-01-01"}"#);

    // Ни одного результата — пустой массив.
    let result = call(
        &server,
        "get_page_by_id",
        json!({"idx": 42, "jq": "select(.id == 1)", "fields": null}),
    )
    .await;
    assert_eq!(text(&result), "[]");

    let result = call(
        &server,
        "api_request",
        json!({"method": "GET", "path": "/v1/pages/42", "jq": ".title"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#""A""#);
    let schema = server.registry().get("api_request").unwrap().tool.clone();
    assert!(schema.input_schema["properties"].get("jq").is_some());
}

/// Сломанный фильтр и фильтр, читающий окружение, отклоняются до запроса к API.
#[tokio::test]
async fn broken_filter_is_reported_before_the_request() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 7})))
        .expect(0)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Write, 64 * 1024);
    let body = json!({"slug": "team/new", "title": "New", "page_type": "page"});
    for (filter, needle) in [
        (json!(".id |"), "syntax error"),
        (json!("env"), "undefined filter `env/0`"),
        (json!("$ENV"), "undefined variable `$ENV`"),
        (json!("input"), "undefined filter `input/0`"),
        (json!(1), "must be a string"),
    ] {
        let result = call(&server, "create_page", json!({"body": body, "jq": filter})).await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            text(&result).contains(needle),
            "{needle}: {}",
            text(&result)
        );
    }
    let result = call(
        &server,
        "api_request",
        json!({"method": "POST", "path": "/v1/pages", "body": {}, "jq": "{"}),
    )
    .await;
    assert!(text(&result).contains("argument `jq`"), "{}", text(&result));
}

/// Ошибка выполнения фильтра после записи: агент узнаёт, что запрос выполнен.
#[tokio::test]
async fn runtime_error_says_the_request_succeeded() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .and(body_json(
            json!({"slug": "team/new", "title": "New", "page_type": "page"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 7})))
        .expect(1)
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Write, 64 * 1024);
    let result = call(
        &server,
        "create_page",
        json!({
            "body": {"slug": "team/new", "title": "New", "page_type": "page"},
            "jq": ".id[]"
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("the API request succeeded (HTTP 200)"),
        "{message}"
    );
    assert!(message.contains("`jq` filter failed"), "{message}");
    assert!(message.contains("apply the change again"), "{message}");
}

/// Лимит ответа действует на результат фильтра: большой ответ можно сузить фильтром, а
/// слишком большой результат — ошибка с подсказкой.
#[tokio::test]
async fn size_limit_applies_to_the_filtered_result() {
    let api = MockServer::start().await;
    let big: Vec<Value> = (0..200)
        .map(|i| json!({"id": i, "slug": format!("team/{i}"), "content": "x".repeat(100)}))
        .collect();
    Mock::given(method("GET"))
        .and(path("/v1/pages/descendants"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"results": big, "next_cursor": null})),
        )
        .mount(&api)
        .await;
    // Ответ около 25 КБ при лимите 4 КБ.
    let server = server(&api, AccessLevel::Read, 4 * 1024);
    let request =
        json!({"method": "GET", "path": "/v1/pages/descendants", "query": {"slug": "team"}});

    let unfiltered = call(&server, "api_request", request.clone()).await;
    assert_eq!(unfiltered.is_error, Some(true), "{}", text(&unfiltered));

    let mut narrowed = request.clone();
    narrowed["jq"] = json!(".results | length");
    let result = call(&server, "api_request", narrowed).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["result"], 200);

    let mut too_big = request;
    too_big["jq"] = json!(".results[] | .content");
    let result = call(&server, "api_request", too_big).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("larger than 4096 bytes"), "{message}");
    assert!(message.contains("HTTP 200"), "{message}");
}

/// Фильтр видит только очищенный ответ: ни поиском строк, ни преобразованием нельзя
/// достать вырезанные значения и токен клиента.
#[tokio::test]
async fn filter_sees_only_redacted_values() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1,
            "password": "super-secret-password",
            "echo": format!("token is {TEST_TOKEN}"),
            "content": "see https://user:pa55word@hooks.example.com/x"
        })))
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, 64 * 1024);
    for filter in ["[.. | strings]", "[.. | strings | @base64]", "tojson"] {
        let result = call(
            &server,
            "api_request",
            json!({"method": "GET", "path": "/v1/pages/1", "jq": filter}),
        )
        .await;
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        let out = text(&result);
        for secret in ["super-secret-password", TEST_TOKEN, "pa55word"] {
            assert!(!out.contains(secret), "{filter}: {out}");
            let encoded = base64_encode(secret);
            assert!(!out.contains(&encoded), "{filter}: {out}");
        }
        assert!(out.contains("[redacted]"), "{filter}: {out}");
    }
}

/// Не-JSON ответ возвращается без фильтра, с пояснением.
#[tokio::test]
async fn non_json_response_is_returned_unfiltered() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/1/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("plain text", "text/plain"))
        .mount(&api)
        .await;
    let server = server(&api, AccessLevel::Read, 64 * 1024);
    let result = call(
        &server,
        "api_request",
        json!({"method": "GET", "path": "/v1/pages/1/comments", "jq": ".a"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let out = text(&result);
    assert!(out.starts_with("plain text"), "{out}");
    assert!(out.contains("filter was not applied"), "{out}");
}

/// Операции с двоичным ответом сохраняют файл, у составных инструментов свои аргументы:
/// аргумента `jq` у них нет; с `save_to` в `api_request` он запрещён.
#[tokio::test]
async fn downloads_and_composites_have_no_jq() {
    let api = MockServer::start().await;
    let server = server(&api, AccessLevel::Write, 64 * 1024);
    let registry = server.registry();
    let download = &registry.get("download_attachment").unwrap().tool;
    assert!(download.input_schema["properties"].get("jq").is_none());
    assert!(download.input_schema["properties"].get("save_to").is_some());
    for composite in ["upload_attachment", "wait_for_operation"] {
        let tool = &registry.get(composite).unwrap().tool;
        assert!(
            tool.input_schema["properties"].get("jq").is_none(),
            "{composite}"
        );
    }
    let result = call(
        &server,
        "wait_for_operation",
        json!({"operation_type": "clone", "operation_id": "x", "jq": "."}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("`jq`"), "{}", text(&result));

    let result = call(
        &server,
        "api_request",
        json!({
            "method": "GET",
            "path": "/v1/pages/1/attachments/download",
            "save_to": "/tmp/x",
            "jq": "."
        }),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
}

/// Простой base64 без зависимостей: проверяем, что фильтр не вернул закодированный секрет.
fn base64_encode(text: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = text.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
