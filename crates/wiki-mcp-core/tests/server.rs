//! Сквозная проверка: сервер в процессе, клиент `rmcp` через in-memory транспорт,
//! Wiki API подменён `wiremock`. Реальный API, keychain и сеть не используются.

use std::path::Path;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject, Tool};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use wiki_api::executor::{ApiClient, ClientConfig, Credentials, OrgId, RetryPolicy};
use wiki_api::{AccessLevel, ApiToken};
use wiki_mcp_core::{API_REQUEST_TOOL, API_SEARCH_TOOL, FilePolicy, ServerOptions, WikiServer};
use wiremock::matchers::{body_bytes, body_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_TOKEN: &str = "test-token-value";
const TEST_ORG: &str = "test-org";
/// Значения, похожие на токены по форме (не настоящие).
const FAKE_OAUTH: &str = "y0_AgAAAAAexampleexampleexampleexampleexampleexample";
const FAKE_IAM: &str = "t1.9euelZqexample.exampleexampleexampleexampleexampleexample12";

fn client_with(api: &MockServer, credentials: Credentials) -> ApiClient {
    let mut config =
        ClientConfig::new(Some(credentials), Some(OrgId::Cloud(TEST_ORG.into()))).unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.max_response_bytes = 64 * 1024;
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    ApiClient::new(config).unwrap()
}

fn client(api: &MockServer) -> ApiClient {
    client_with(api, Credentials::oauth(ApiToken::new(TEST_TOKEN)))
}

async fn start_with(
    client: ApiClient,
    options: ServerOptions,
) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    start_with_files(client, options, FilePolicy::new(&[]).unwrap()).await
}

async fn start_with_files(
    client: ApiClient,
    options: ServerOptions,
    files: FilePolicy,
) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    let server = WikiServer::new(client, options, files).unwrap();
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    let handle = tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ().serve(client_io).await.unwrap();
    (client, handle)
}

async fn start(
    api: &MockServer,
    options: ServerOptions,
) -> (RunningService<RoleClient, ()>, tokio::task::JoinHandle<()>) {
    start_with(client(api), options).await
}

fn options(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ServerOptions {
    ServerOptions {
        max_access,
        toolsets: toolsets.map(|t| t.iter().map(|s| (*s).to_owned()).collect()),
        ..ServerOptions::default()
    }
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

fn names(tools: &[Tool]) -> Vec<&str> {
    tools.iter().map(|t| t.name.as_ref()).collect()
}

async fn call(
    client: &RunningService<RoleClient, ()>,
    name: &'static str,
    value: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name).with_arguments(args(value)))
        .await
        .unwrap()
}

#[tokio::test]
async fn read_only_server_end_to_end() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages"))
        .and(query_param("slug", "users/name/page"))
        .and(query_param("fields", "content"))
        .and(header(
            "authorization",
            format!("OAuth {TEST_TOKEN}").as_str(),
        ))
        .and(header("x-cloud-org-id", TEST_ORG))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42,
            "slug": "users/name/page",
            "title": "Page"
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/404"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "error_code": "NOT_FOUND",
            "debug_message": "Page not found",
            "details": {}
        })))
        .mount(&api)
        .await;

    let (client, server) = start(&api, options(AccessLevel::Read, None)).await;

    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "wiki-mcp");
    let instructions = info.instructions.as_deref().unwrap();
    assert!(instructions.contains("read-only"), "{instructions}");

    let tools = client.list_all_tools().await.unwrap();
    let listed = names(&tools);
    assert!(listed.contains(&"get_page"), "{listed:?}");
    assert!(listed.contains(&"search"), "{listed:?}");
    assert!(tools.iter().all(|t| {
        t.annotations
            .as_ref()
            .is_some_and(|a| a.read_only_hint == Some(true))
    }));
    for absent in [
        "create_page",
        "delete_page",
        "list_spaces",
        "create_page_access",
    ] {
        assert!(!listed.contains(&absent), "{absent}");
    }
    // Описание — из спецификации.
    let get_page = tools.iter().find(|t| t.name == "get_page").unwrap();
    let op = wiki_api::spec::find("get_page").unwrap();
    assert!(
        get_page
            .description
            .as_deref()
            .unwrap()
            .starts_with(&op.tool_description())
    );

    // Успешный вызов: путь, query, Authorization и организацию проверяет mock.
    let result = call(
        &client,
        "get_page",
        json!({"slug": "users/name/page", "fields": "content"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(
        text(&result),
        r#"{"id":42,"slug":"users/name/page","title":"Page"}"#
    );

    // 404 Wiki — управляемая ошибка, не ошибка протокола.
    let result = call(&client, "get_page_by_id", json!({"idx": 404})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 404"), "{message}");
    assert!(message.contains("Error code: NOT_FOUND"), "{message}");
    assert!(message.contains("Page not found"), "{message}");
    assert!(!message.contains(TEST_TOKEN));

    // Инструмент записи не зарегистрирован, вызов объясняет, какой флаг нужен.
    let result = call(
        &client,
        "create_page",
        json!({"body": {"slug": "users/name/new", "title": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`write` access"),
        "{}",
        text(&result)
    );
    let result = call(&client, "delete_page", json!({"idx": 1})).await;
    assert!(
        text(&result).contains("`destructive` access"),
        "{}",
        text(&result)
    );
    // Набор выключен — подсказка про --toolsets.
    let result = call(&client, "list_spaces", json!({})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("toolset `extra`"),
        "{}",
        text(&result)
    );

    // Ошибка аргументов не доходит до API.
    let result = call(&client, "get_page_by_id", json!({"id": 1})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("unknown argument(s) `id`"),
        "{}",
        text(&result)
    );
    let result = call(&client, "get_page_by_id", json!({})).await;
    assert!(text(&result).contains("missing required argument(s): `idx`"));

    // Неизвестный инструмент — ошибка протокола, сервер продолжает работать.
    assert!(
        client
            .call_tool(CallToolRequestParams::new("no_such_tool"))
            .await
            .is_err()
    );
    assert!(!client.list_all_tools().await.unwrap().is_empty());

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn wiki_errors_are_tool_errors_with_details() {
    let api = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .and(body_json(json!({"slug": "users/name/new", "title": "T"})))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error_code": "VALIDATION_ERROR",
            "debug_message": "Validation failed",
            "message": "page_type: Поле обязательно",
            "details": {"page_type": ["required"]}
        })))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/5"))
        .respond_with(ResponseTemplate::new(503).set_body_string("Service Unavailable"))
        .mount(&api)
        .await;

    let (client, server) = start(&api, options(AccessLevel::Write, None)).await;
    let result = call(
        &client,
        "create_page",
        json!({"body": {"slug": "users/name/new", "title": "T"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 400"), "{message}");
    assert!(message.contains("check the arguments"), "{message}");
    assert!(
        message.contains("Error code: VALIDATION_ERROR"),
        "{message}"
    );
    assert!(
        message.contains("Debug message: Validation failed"),
        "{message}"
    );
    assert!(
        message.contains("Message: page_type: Поле обязательно"),
        "{message}"
    );
    assert!(
        message.contains(r#"Details: {"page_type":["required"]}"#),
        "{message}"
    );

    let result = call(&client, "get_page_by_id", json!({"idx": 5})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("HTTP 503"), "{message}");
    assert!(message.contains("retry later"), "{message}");
    assert!(
        message.contains("Response body: Service Unavailable"),
        "{message}"
    );

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn iam_token_uses_bearer_scheme() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/users/me"))
        .and(header(
            "authorization",
            format!("Bearer {TEST_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "robot"})))
        .expect(1)
        .mount(&api)
        .await;
    let (client, server) = start_with(
        client_with(&api, Credentials::iam(ApiToken::new(TEST_TOKEN))),
        options(AccessLevel::Read, None),
    )
    .await;
    let result = call(&client, "get_current_user", json!({})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"login":"robot"}"#);
    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn access_levels_change_the_tool_list() {
    let api = MockServer::start().await;

    let (write, server) = start(&api, options(AccessLevel::Write, Some(&["all"]))).await;
    let tools = write.list_all_tools().await.unwrap();
    let create = tools.iter().find(|t| t.name == "create_page").unwrap();
    let annotations = create.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    let listed = names(&tools);
    assert!(listed.contains(&"update_grid_cells"));
    assert!(!listed.contains(&"delete_page"));
    assert!(!listed.contains(&"create_page_access"));
    assert!(tools.iter().all(|t| {
        t.annotations
            .as_ref()
            .is_some_and(|a| a.destructive_hint != Some(true))
    }));
    write.cancel().await.unwrap();
    server.await.unwrap();

    let (all, server) = start(&api, options(AccessLevel::Destructive, Some(&["all"]))).await;
    let tools = all.list_all_tools().await.unwrap();
    let delete = tools.iter().find(|t| t.name == "delete_page").unwrap();
    assert_eq!(
        delete.annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
    let access = tools
        .iter()
        .find(|t| t.name == "create_page_access")
        .unwrap();
    assert_eq!(
        access.annotations.as_ref().unwrap().destructive_hint,
        Some(true)
    );
    let expected = wiki_api::spec::operations().len();
    // Все операции (локальный сервер работает с файлами), составные `upload_attachment`
    // и `wait_for_operation` и встроенные `api_request` и `api_search`.
    assert_eq!(tools.len(), expected + 2 + 2);
    let upload = tools
        .iter()
        .find(|t| t.name == "upload_attachment")
        .unwrap();
    let annotations = upload.annotations.as_ref().unwrap();
    assert_eq!(annotations.read_only_hint, Some(false));
    assert_eq!(annotations.destructive_hint, Some(false));
    let wait = tools
        .iter()
        .find(|t| t.name == "wait_for_operation")
        .unwrap();
    assert_eq!(
        wait.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    all.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn toolsets_narrow_the_tool_list() {
    let api = MockServer::start().await;

    let (grids, server) = start(&api, options(AccessLevel::Destructive, Some(&["grids"]))).await;
    let tools = grids.list_all_tools().await.unwrap();
    assert!(!tools.is_empty());
    for tool in &tools {
        if tool.name == API_REQUEST_TOOL || tool.name == API_SEARCH_TOOL {
            continue;
        }
        let op = wiki_api::spec::find(&tool.name).unwrap();
        assert_eq!(op.toolset, "grids", "{}", tool.name);
    }
    let listed = names(&tools);
    assert!(listed.contains(&"delete_grid"));
    // Встроенные инструменты регистрируются при любых наборах.
    assert!(listed.contains(&API_REQUEST_TOOL));
    assert!(listed.contains(&API_SEARCH_TOOL));
    grids.cancel().await.unwrap();
    server.await.unwrap();

    // Выключенные по умолчанию наборы включаются явно.
    let (access, server) = start(
        &api,
        options(AccessLevel::Destructive, Some(&["pages", "access"])),
    )
    .await;
    let tools = access.list_all_tools().await.unwrap();
    let listed = names(&tools);
    assert!(listed.contains(&"create_page_access"));
    assert!(listed.contains(&"get_page"));
    assert!(!listed.contains(&"get_grid"));
    let instructions = access.peer_info().unwrap().instructions.clone().unwrap();
    assert!(
        instructions.contains("Enabled toolsets: pages, access;"),
        "{instructions}"
    );
    access.cancel().await.unwrap();
    server.await.unwrap();

    assert!(
        WikiServer::new(
            client(&api),
            options(AccessLevel::Read, Some(&["nope"])),
            FilePolicy::new(&[]).unwrap()
        )
        .is_err()
    );
}

#[tokio::test]
async fn api_request_respects_access() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/42/comments"))
        .and(query_param("page_size", "5"))
        .and(header(
            "authorization",
            format!("OAuth {TEST_TOKEN}").as_str(),
        ))
        .and(header("x-cloud-org-id", TEST_ORG))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
        .expect(1)
        .mount(&api)
        .await;
    // Поиск — POST, но уровень чтения.
    Mock::given(method("POST"))
        .and(path("/v1/search"))
        .and(body_json(json!({"query": "wiki"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [{"id": 1}]})))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/pages/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 7})))
        .expect(1)
        .mount(&api)
        .await;

    // Режим чтения: `api_request` зарегистрирован и только читает.
    let (read, server) = start(&api, options(AccessLevel::Read, None)).await;
    let tools = read.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == API_REQUEST_TOOL).unwrap();
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );

    let result = call(
        &read,
        API_REQUEST_TOOL,
        json!({"method": "GET", "path": "/v1/pages/42/comments", "query": {"page_size": 5}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"results":[]}"#);

    let result = call(
        &read,
        API_REQUEST_TOOL,
        json!({"method": "POST", "path": "/v1/search", "body": {"query": "wiki"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));

    // Запись без уровня записи отклоняется до отправки (mock POST вызывается один раз ниже).
    let result = call(
        &read,
        API_REQUEST_TOOL,
        json!({"method": "POST", "path": "/v1/pages/42/comments", "body": {"body": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(message.contains("needs write access"), "{message}");
    assert!(message.contains("`create_comment`"), "{message}");
    assert!(message.contains("`write` access"), "{message}");

    // Изменение прав доступа — уровень удаления, даже для POST.
    let result = call(
        &read,
        API_REQUEST_TOOL,
        json!({"method": "POST", "path": "/v1/pages/42/access", "body": {}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`destructive` access"),
        "{}",
        text(&result)
    );

    // Заголовки и полные URL не принимаются.
    let result = call(
        &read,
        API_REQUEST_TOOL,
        json!({"method": "GET", "path": "/v1/pages/42", "headers": {"Authorization": "OAuth x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("unknown argument(s) `headers`"),
        "{}",
        text(&result)
    );
    let result = call(
        &read,
        API_REQUEST_TOOL,
        json!({"method": "GET", "path": "https://example.invalid/v1/pages/42"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("full URLs"), "{}", text(&result));
    read.cancel().await.unwrap();
    server.await.unwrap();

    // С уровнем записи тот же запрос уходит в API.
    let (write, server) = start(&api, options(AccessLevel::Write, None)).await;
    let result = call(
        &write,
        API_REQUEST_TOOL,
        json!({"method": "POST", "path": "/v1/pages/42/comments", "body": {"body": "x"}}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"id":7}"#);
    write.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn api_search_finds_disabled_operations() {
    let api = MockServer::start().await;
    let (client, server) = start(&api, options(AccessLevel::Read, None)).await;

    let result = call(&client, API_SEARCH_TOOL, json!({"query": "строки таблицы"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let found: Value = serde_json::from_str(&text(&result)).unwrap();
    let rows = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool_name"] == "add_grid_rows")
        .unwrap_or_else(|| panic!("{found}"));
    assert_eq!(rows["registered"], false);
    assert_eq!(
        rows["enable_with"],
        "`write` access for the server's profile (`wiki-mcp ui`)"
    );

    let result = call(&client, API_SEARCH_TOOL, json!({"query": "вложения"})).await;
    let found: Value = serde_json::from_str(&text(&result)).unwrap();
    let list = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool_name"] == "list_attachments")
        .unwrap_or_else(|| panic!("{found}"));
    assert_eq!(list["registered"], true);

    let result = call(
        &client,
        API_SEARCH_TOOL,
        json!({"query": "доступ к странице"}),
    )
    .await;
    let found: Value = serde_json::from_str(&text(&result)).unwrap();
    let access = found["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tool_name"] == "create_page_access")
        .unwrap_or_else(|| panic!("{found}"));
    assert_eq!(access["registered"], false);
    assert_eq!(
        access["enable_with"],
        "`destructive` access for the server's profile (`wiki-mcp ui`) and --toolsets access"
    );

    let result = call(&client, API_SEARCH_TOOL, json!({"tool_name": "get_grid"})).await;
    let found: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(found["registered"], true);
    assert_eq!(found["input_schema"]["type"], "object");

    client.cancel().await.unwrap();
    server.await.unwrap();
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

#[tokio::test]
async fn credentials_are_redacted_in_responses_and_errors() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1,
            "content": format!("Bot token: OAuth {FAKE_OAUTH}"),
            "password": "p@ss-1",
            "next_cursor": FAKE_OAUTH
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/2"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error_code": "VALIDATION_ERROR",
            "debug_message": format!("bad header Bearer {FAKE_IAM}"),
            "message": format!("token {FAKE_OAUTH} is invalid"),
            "details": {"api_key": "k-2"}
        })))
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/3"))
        .respond_with(ResponseTemplate::new(500).set_body_string(format!(
            "<html>proxy https://bot:pw-3@internal.example.test {FAKE_IAM}</html>"
        )))
        .mount(&api)
        .await;
    // Свой токен сервера, отражённый API, тоже не попадает к агенту.
    Mock::given(method("GET"))
        .and(path("/v1/users/me"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"echo": format!("OAuth {TEST_TOKEN}")})),
        )
        .mount(&api)
        .await;

    let (client, server) = start(&api, options(AccessLevel::Read, None)).await;

    let result = call(&client, "get_page_by_id", json!({"idx": 1})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let body = text(&result);
    assert!(!body.contains("p@ss-1"), "{body}");
    assert!(body.contains("Bot token: OAuth [redacted]"), "{body}");
    assert!(body.contains(r#""password":"[redacted]""#), "{body}");
    // Курсор выдачи нужен агенту как есть.
    assert!(
        body.contains(&format!(r#""next_cursor":"{FAKE_OAUTH}""#)),
        "{body}"
    );
    assert!(body.contains("password."), "{body}");
    assert!(body.contains("1 value(s)"), "{body}");

    let result = call(&client, "get_page_by_id", json!({"idx": 2})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    for secret in [FAKE_IAM, FAKE_OAUTH, "k-2"] {
        assert!(!message.contains(secret), "{secret}: {message}");
    }
    assert!(
        message.contains("Error code: VALIDATION_ERROR"),
        "{message}"
    );
    assert!(
        message.contains("Debug message: bad header Bearer [redacted]"),
        "{message}"
    );
    assert!(
        message.contains("Message: token [redacted] is invalid"),
        "{message}"
    );
    assert!(message.contains(r#""api_key":"[redacted]""#), "{message}");
    assert!(message.contains("[redacted]\": api_key."), "{message}");

    let result = call(&client, "get_page_by_id", json!({"idx": 3})).await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        !message.contains("pw-3") && !message.contains(FAKE_IAM),
        "{message}"
    );
    assert!(message.contains("2 value(s)"), "{message}");

    let result = call(&client, "get_current_user", json!({})).await;
    assert!(!text(&result).contains(TEST_TOKEN), "{}", text(&result));

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn files_are_uploaded_and_downloaded_inside_allowed_directories() {
    let root_dir = tempfile::tempdir().unwrap();
    let root = root_dir.path().canonicalize().unwrap();
    let outside_dir = tempfile::tempdir().unwrap();
    let outside = outside_dir.path().canonicalize().unwrap();
    let part = root.join("part.bin");
    std::fs::write(&part, b"part-bytes").unwrap();
    std::fs::write(outside.join("secret.txt"), b"secret").unwrap();

    let api = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/v1/upload_sessions/s-1/upload_part"))
        .and(query_param("part_number", "1"))
        .and(header("content-type", "application/octet-stream"))
        .and(body_bytes(b"part-bytes".to_vec()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"part_number": 1})))
        .expect(1)
        .mount(&api)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/42/attachments/7/download"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .set_body_bytes(b"%PDF-1.7 content".to_vec()),
        )
        .expect(2)
        .mount(&api)
        .await;

    let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
    let (client, server) =
        start_with_files(self::client(&api), options(AccessLevel::Write, None), files).await;

    let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
    assert!(instructions.contains(&display(&root)), "{instructions}");
    let tools = client.list_all_tools().await.unwrap();
    let listed = names(&tools);
    for name in ["upload_file_part", "download_attachment"] {
        assert!(listed.contains(&name), "{name}");
    }

    // Загрузка части: тело — байты файла.
    let result = call(
        &client,
        "upload_file_part",
        json!({"session_id": "s-1", "part_number": 1, "file": display(&part)}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"part_number":1}"#);

    // Файл вне разрешённых каталогов не читается, запрос не отправляется.
    let result = call(
        &client,
        "upload_file_part",
        json!({"session_id": "s-1", "part_number": 1, "file": display(&outside.join("secret.txt"))}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    let message = text(&result);
    assert!(
        message.contains("outside the allowed directories"),
        "{message}"
    );
    assert!(message.contains("allow_path"), "{message}");

    // Символическая ссылка наружу отклоняется.
    #[cfg(unix)]
    {
        let link = root.join("link.txt");
        std::os::unix::fs::symlink(outside.join("secret.txt"), &link).unwrap();
        let result = call(
            &client,
            "upload_file_part",
            json!({"session_id": "s-1", "part_number": 1, "file": display(&link)}),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            text(&result).contains("outside the allowed directories"),
            "{}",
            text(&result)
        );
        let dir_link = root.join("out");
        std::os::unix::fs::symlink(&outside, &dir_link).unwrap();
        let result = call(
            &client,
            "download_attachment",
            json!({"idx": 42, "file_id": 7, "save_to": display(&dir_link.join("x.pdf"))}),
        )
        .await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            text(&result).contains("outside the allowed directories"),
            "{}",
            text(&result)
        );
    }

    // Скачивание: файл сохраняется, в ответе путь и размер.
    let target = root.join("file.pdf");
    let result = call(
        &client,
        "download_attachment",
        json!({"idx": 42, "file_id": 7, "save_to": display(&target)}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(std::fs::read(&target).unwrap(), b"%PDF-1.7 content");
    let saved: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(saved["saved_to"], display(&target));
    assert_eq!(saved["bytes"], 16);

    // Существующий файл не перезаписывается.
    let result = call(
        &client,
        "download_attachment",
        json!({"idx": 42, "file_id": 7, "save_to": display(&target)}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("already exists"),
        "{}",
        text(&result)
    );

    // `api_request` скачивает тоже только в `save_to`.
    let result = call(
        &client,
        API_REQUEST_TOOL,
        json!({"method": "GET", "path": "/v1/pages/42/attachments/7/download"}),
    )
    .await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("pass `save_to`"),
        "{}",
        text(&result)
    );
    let second = root.join("second.pdf");
    let result = call(
        &client,
        API_REQUEST_TOOL,
        json!({"method": "GET", "path": "/v1/pages/42/attachments/7/download", "save_to": display(&second)}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert!(second.exists());

    client.cancel().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn remote_server_has_no_file_tools() {
    let api = MockServer::start().await;
    let server = WikiServer::with_token_source(
        client(&api),
        ServerOptions {
            local_machine: false,
            ..options(AccessLevel::Write, None)
        },
        // Политика игнорируется: удалённый сервер с файлами не работает.
        FilePolicy::new(&[std::env::temp_dir()]).unwrap(),
        wiki_mcp_core::TokenSource::Client,
    )
    .unwrap();
    assert!(server.files().is_disabled());
    assert!(server.registry().get("upload_file_part").is_none());
    let result = server
        .call(
            "download_attachment",
            args(json!({"idx": 1, "file_id": 2, "save_to": "x.pdf"})),
        )
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("HTTP mode"), "{}", text(&result));
}
