//! Пользовательские инструменты через MCP: список инструментов, фиксированные аргументы,
//! встроенный фильтр и фильтр агента, уровень доступа. Wiki API подменён `wiremock`.

use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult, JsonObject};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use serde_json::{Value, json};
use wiki_api::executor::{ApiClient, ClientConfig, Credentials, OrgId, RetryPolicy};
use wiki_api::{AccessLevel, ApiToken};
use wiki_mcp_core::{CustomTool, FilePolicy, ServerOptions, WikiServer};
use wiremock::matchers::{body_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(api: &MockServer) -> ApiClient {
    let mut config = ClientConfig::new(
        Some(Credentials::oauth(ApiToken::new("test-token-value"))),
        Some(OrgId::Yandex360("12345".into())),
    )
    .unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    ApiClient::new(config).unwrap()
}

fn custom(value: Value) -> CustomTool {
    CustomTool::compile(serde_json::from_value(value).unwrap(), "tools.toml").unwrap()
}

fn team_pages() -> CustomTool {
    custom(json!({
        "name": "team_pages",
        "description": "Pages of the team section",
        "operation": "list_page_descendants",
        "args": {"slug": "team/project", "page_size": 50},
        "jq": "[.results[] | {slug, title}]"
    }))
}

fn comment_on_home() -> CustomTool {
    custom(json!({
        "name": "comment_on_home",
        "description": "Comment on the home page",
        "operation": "create_comment",
        "args": {"idx": 42}
    }))
}

async fn start(
    api: &MockServer,
    max_access: AccessLevel,
) -> (RunningService<RoleClient, ()>, WikiServer) {
    let server = WikiServer::new(
        client(api),
        ServerOptions {
            max_access,
            custom_tools: vec![team_pages(), comment_on_home()],
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
    )
    .unwrap();
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    let running = server.clone();
    tokio::spawn(async move {
        let running = running.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    (().serve(client_io).await.unwrap(), server)
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

async fn call(client: &RunningService<RoleClient, ()>, name: &str, value: Value) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(value)))
        .await
        .unwrap()
}

async fn mount_descendants(api: &MockServer, cursor: Option<&str>) {
    let mut mock = Mock::given(method("GET"))
        .and(path("/v1/pages/descendants"))
        .and(query_param("slug", "team/project"))
        .and(query_param("page_size", "50"));
    if let Some(cursor) = cursor {
        mock = mock.and(query_param("cursor", cursor));
    }
    mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
        "results": [
            {"id": 1, "slug": "team/project/a", "title": "Plan"},
            {"id": 2, "slug": "team/project/b", "title": "Notes"}
        ],
        "next_cursor": "c2",
        "prev_cursor": null
    })))
    .mount(api)
    .await;
}

#[tokio::test]
async fn custom_tool_is_listed_without_fixed_arguments() {
    let api = MockServer::start().await;
    let (client, server) = start(&api, AccessLevel::Read).await;
    let tools = client.list_all_tools().await.unwrap();
    let tool = tools.iter().find(|t| t.name == "team_pages").unwrap();
    let properties = tool.input_schema["properties"].as_object().unwrap();
    assert!(!properties.contains_key("slug"), "{properties:?}");
    assert!(!properties.contains_key("page_size"), "{properties:?}");
    assert!(properties.contains_key("cursor"), "{properties:?}");
    assert!(properties.contains_key("jq"), "{properties:?}");
    assert_eq!(
        tool.annotations.as_ref().unwrap().read_only_hint,
        Some(true)
    );
    // Инструмент записи выше уровня доступа не зарегистрирован.
    assert!(!tools.iter().any(|t| t.name == "comment_on_home"));
    assert!(
        server.instructions().contains("- Custom tools:"),
        "{}",
        server.instructions()
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn custom_tool_calls_operation_with_fixed_arguments_and_filter() {
    let api = MockServer::start().await;
    mount_descendants(&api, Some("c1")).await;
    let (client, _server) = start(&api, AccessLevel::Read).await;

    let result = call(&client, "team_pages", json!({"cursor": "c1"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(
        value,
        json!({
            "result": [
                {"slug": "team/project/a", "title": "Plan"},
                {"slug": "team/project/b", "title": "Notes"}
            ],
            "pagination": {"next_cursor": "c2", "prev_cursor": null}
        })
    );
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn agent_filter_runs_after_the_tool_filter() {
    let api = MockServer::start().await;
    mount_descendants(&api, None).await;
    let (client, _server) = start(&api, AccessLevel::Read).await;

    // Фильтр агента видит результат фильтра инструмента (массив), а не исходный ответ.
    let result = call(&client, "team_pages", json!({"jq": "map(.slug)"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let value: Value = serde_json::from_str(&text(&result)).unwrap();
    assert_eq!(value["result"], json!(["team/project/a", "team/project/b"]));
    assert_eq!(value["pagination"]["next_cursor"], "c2");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn fixed_arguments_cannot_be_overridden() {
    let api = MockServer::start().await;
    let (client, _server) = start(&api, AccessLevel::Read).await;
    let result = call(&client, "team_pages", json!({"slug": "other/section"})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("slug"), "{}", text(&result));
    // Запрос к API не отправлялся.
    assert!(api.received_requests().await.unwrap().is_empty());
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn write_tool_needs_write_access() {
    let api = MockServer::start().await;
    let (client, _server) = start(&api, AccessLevel::Read).await;
    let result = call(&client, "comment_on_home", json!({"body": {"body": "Hi"}})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`write` access"),
        "{}",
        text(&result)
    );
    client.cancel().await.unwrap();

    Mock::given(method("POST"))
        .and(path("/v1/pages/42/comments"))
        .and(body_json(json!({"body": "Hi"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 7})))
        .expect(1)
        .mount(&api)
        .await;
    let (client, _server) = start(&api, AccessLevel::Write).await;
    let result = call(&client, "comment_on_home", json!({"body": {"body": "Hi"}})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"id":7}"#);
    client.cancel().await.unwrap();
}
