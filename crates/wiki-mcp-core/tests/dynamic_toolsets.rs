//! Динамические наборы инструментов: начальный список, `enable_toolset` с уведомлением
//! `notifications/tools/list_changed` (через сессию с `initialize` и через подписку протокола
//! 2026-07-28), уровень доступа, составные инструменты, сессии и токены отдельных запросов.
//! Wiki API подменён `wiremock`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Extensions, JsonObject, ProtocolVersion,
    ServerNotification, SubscriptionFilter,
};
use rmcp::service::{NotificationContext, RunningService};
use rmcp::{ClientHandler, ClientServiceExt, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::sync::Notify;
use wiki_api::executor::{ApiClient, ClientConfig, Credentials, OrgId, RetryPolicy};
use wiki_api::{AccessLevel, ApiToken};
use wiki_mcp_core::{CustomTool, FilePolicy, RequestAuth, ServerOptions, TokenSource, WikiServer};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CORE_TOOLS: [&str; 5] = [
    "api_request",
    "api_search",
    "disable_toolset",
    "enable_toolset",
    "list_toolsets",
];

/// Клиент, который считает уведомления о смене списка инструментов.
#[derive(Clone, Default)]
struct CountingClient {
    changes: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ClientHandler for CountingClient {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.changes.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

fn config(api: &MockServer, token: Option<&str>) -> ClientConfig {
    let mut config = ClientConfig::new(
        token.map(|token| Credentials::oauth(ApiToken::new(token))),
        Some(OrgId::Yandex360("12345".into())),
    )
    .unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    config
}

fn server_with(api: &MockServer, options: ServerOptions) -> WikiServer {
    let client = ApiClient::new(config(api, Some("test-token-value"))).unwrap();
    WikiServer::new(client, options, FilePolicy::new(&[]).unwrap()).unwrap()
}

fn server(api: &MockServer, max_access: AccessLevel, dynamic: bool) -> WikiServer {
    server_with(
        api,
        ServerOptions {
            max_access,
            dynamic_toolsets: dynamic,
            ..ServerOptions::default()
        },
    )
}

fn spawn(server: WikiServer) -> tokio::io::DuplexStream {
    let (server_io, client_io) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    client_io
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

async fn call<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
    name: &str,
    value: Value,
) -> CallToolResult {
    client
        .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(args(value)))
        .await
        .unwrap()
}

async fn tool_names<S: rmcp::Service<RoleClient>>(
    client: &RunningService<RoleClient, S>,
) -> Vec<String> {
    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect();
    names.sort();
    names
}

fn has(names: &[String], name: &str) -> bool {
    names.iter().any(|n| n == name)
}

/// Главный сценарий: агент включает набор, получает уведомление и сразу вызывает
/// инструмент набора (с фильтром `jq`).
#[tokio::test]
async fn enable_toolset_adds_tools_and_notifies() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 42,
            "slug": "team/a",
            "title": "A",
            "content": "long text"
        })))
        .expect(1)
        .mount(&api)
        .await;

    let handler = CountingClient::default();
    let client = handler
        .clone()
        .serve(spawn(server(&api, AccessLevel::Read, true)))
        .await
        .unwrap();

    let info = client.peer_info().unwrap();
    let tools_capability = info.capabilities.tools.as_ref().unwrap();
    assert_eq!(tools_capability.list_changed, Some(true));
    let instructions = info.instructions.as_deref().unwrap();
    assert!(instructions.contains("`enable_toolset`"), "{instructions}");
    assert!(
        instructions.contains("enabled at start: none"),
        "{instructions}"
    );

    assert_eq!(tool_names(&client).await, CORE_TOOLS);

    // До включения набора инструмент недоступен, ошибка подсказывает `enable_toolset`.
    let result = call(&client, "get_page_by_id", json!({"idx": 42})).await;
    assert_eq!(result.is_error, Some(true));
    assert!(
        text(&result).contains("`enable_toolset`"),
        "{}",
        text(&result)
    );

    let listed: Value =
        serde_json::from_str(&text(&call(&client, "list_toolsets", json!({})).await)).unwrap();
    let pages = listed["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["toolset"] == "pages")
        .unwrap();
    assert_eq!(pages["enabled"], false);

    let result = call(&client, "enable_toolset", json!({"toolset": "pages"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 1);

    let names = tool_names(&client).await;
    assert!(has(&names, "get_page"), "{names:?}");
    assert!(has(&names, "get_page_by_id"), "{names:?}");
    // Составной инструмент набора — тоже.
    assert!(has(&names, "wait_for_operation"), "{names:?}");
    // Уровень доступа действует и для включённого набора.
    assert!(!has(&names, "create_page"), "{names:?}");
    assert!(!has(&names, "delete_page"), "{names:?}");

    let result = call(
        &client,
        "get_page_by_id",
        json!({"idx": 42, "jq": "{slug, title}"}),
    )
    .await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    assert_eq!(text(&result), r#"{"slug":"team/a","title":"A"}"#);

    // Повторное включение не меняет список и не шлёт уведомление.
    call(&client, "enable_toolset", json!({"toolset": "pages"})).await;
    let result = call(&client, "disable_toolset", json!({"toolset": "pages"})).await;
    assert_eq!(result.is_error, Some(false));
    tokio::time::timeout(Duration::from_secs(5), handler.notify.notified())
        .await
        .expect("tools/list_changed");
    assert_eq!(handler.changes.load(Ordering::SeqCst), 2);
    assert_eq!(tool_names(&client).await, CORE_TOOLS);

    client.cancel().await.unwrap();
}

#[tokio::test]
async fn write_tools_appear_only_with_write_access() {
    let api = MockServer::start().await;
    let client = ().serve(spawn(server(&api, AccessLevel::Write, true))).await.unwrap();
    call(
        &client,
        "enable_toolset",
        json!({"toolset": "pages,attachments"}),
    )
    .await;
    let names = tool_names(&client).await;
    assert!(has(&names, "create_page"), "{names:?}");
    assert!(has(&names, "list_attachments"), "{names:?}");
    // Загрузка файла — составной инструмент уровня записи из набора `attachments`.
    assert!(has(&names, "upload_attachment"), "{names:?}");
    // Удаление требует `Destructive`.
    assert!(!has(&names, "delete_page"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn toolsets_from_flags_are_enabled_at_start() {
    let api = MockServer::start().await;
    let server = server_with(
        &api,
        ServerOptions {
            dynamic_toolsets: true,
            toolsets: Some(vec!["grids".into()]),
            ..ServerOptions::default()
        },
    );
    assert!(server.registry().get("get_grid").is_some());
    assert!(server.registry().get("get_page").is_none());
    assert!(
        server.instructions().contains("enabled at start: grids"),
        "{}",
        server.instructions()
    );
}

#[tokio::test]
async fn subscription_receives_list_changed() {
    let api = MockServer::start().await;
    let client = ()
        .serve_with_lifecycle(
            spawn(server(&api, AccessLevel::Read, true)),
            rmcp::ClientLifecycleMode::Discover {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();
    let mut subscription = client
        .listen(SubscriptionFilter::builder().tools_list_changed().build())
        .await
        .unwrap();
    assert_eq!(
        subscription.acknowledged(),
        &SubscriptionFilter::builder().tools_list_changed().build()
    );

    let result = call(&client, "enable_toolset", json!({"toolset": "grids"})).await;
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
    let notification = tokio::time::timeout(Duration::from_secs(5), subscription.next())
        .await
        .unwrap()
        .unwrap()
        .expect("tools/list_changed");
    assert!(matches!(
        notification,
        ServerNotification::ToolListChangedNotification(_)
    ));
    let names = tool_names(&client).await;
    assert!(has(&names, "get_grid"), "{names:?}");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn static_mode_is_unchanged() {
    let api = MockServer::start().await;
    let client = ().serve(spawn(server(&api, AccessLevel::Read, false))).await.unwrap();
    let info = client.peer_info().unwrap();
    assert_ne!(
        info.capabilities.tools.as_ref().unwrap().list_changed,
        Some(true)
    );
    let names = tool_names(&client).await;
    assert!(has(&names, "get_page"));
    assert!(!has(&names, "enable_toolset"));
    let result = client
        .call_tool(CallToolRequestParams::new("enable_toolset").with_arguments(JsonObject::new()))
        .await;
    assert!(result.is_err(), "неизвестный инструмент — ошибка протокола");
    client.cancel().await.unwrap();
}

#[tokio::test]
async fn sessions_are_independent() {
    let api = MockServer::start().await;
    let first = server(&api, AccessLevel::Read, true);
    let second = first.new_session();
    first
        .call("enable_toolset", args(json!({"toolset": "pages"})))
        .await
        .unwrap();
    assert!(first.registry().get("get_page").is_some());
    assert!(second.registry().get("get_page").is_none());
    // Клон делит сессию.
    assert!(first.clone().registry().get("get_page").is_some());
}

#[tokio::test]
async fn toolsets_work_with_per_request_tokens() {
    let api = MockServer::start().await;
    for token in ["agent-one-token", "agent-two-token"] {
        Mock::given(method("GET"))
            .and(path("/v1/users/me"))
            .and(header("authorization", format!("OAuth {token}").as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": token})))
            .expect(1)
            .mount(&api)
            .await;
    }
    let client = ApiClient::new(config(&api, None)).unwrap();
    let server = WikiServer::with_token_source(
        client,
        ServerOptions {
            dynamic_toolsets: true,
            ..ServerOptions::default()
        },
        FilePolicy::new(&[]).unwrap(),
        TokenSource::PerRequest(Arc::new(|ext: &Extensions| {
            ext.get::<RequestAuth>().cloned()
        })),
    )
    .unwrap();
    let with_token = |token: Option<&str>| {
        let mut ext = Extensions::new();
        if let Some(token) = token {
            ext.insert(RequestAuth {
                credentials: Credentials::oauth(ApiToken::new(token)),
                org: None,
            });
        }
        ext
    };

    // Управление наборами к API не обращается: токен ему не нужен и не запоминается.
    for (tool, arguments) in [
        ("list_toolsets", json!({})),
        ("enable_toolset", json!({"toolset": "pages"})),
    ] {
        let result = server
            .call_with_extensions(tool, args(arguments), &with_token(None))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{tool}: {}", text(&result));
    }
    assert!(server.registry().get("get_current_user").is_some());

    // Включённый инструмент берёт токен каждого вызова, а не токен времени включения.
    for token in ["agent-one-token", "agent-two-token"] {
        let result = server
            .call_with_extensions(
                "get_current_user",
                JsonObject::new(),
                &with_token(Some(token)),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        // Мок отвечает только на свой токен (`expect(1)`); отражённый токен вычищается.
        assert!(!text(&result).contains(token), "{}", text(&result));
    }
    let missing = server
        .call_with_extensions("get_current_user", JsonObject::new(), &with_token(None))
        .await
        .unwrap();
    assert_eq!(missing.is_error, Some(true));
}

/// Пользовательские инструменты настроил пользователь: в динамическом режиме они есть сразу,
/// не входят ни в один набор и переживают `enable_toolset`/`disable_toolset`. Уровень
/// доступа действует как обычно.
#[tokio::test]
async fn custom_tools_are_registered_from_start() {
    let custom = |name: &str, operation: &str| {
        CustomTool::compile(
            serde_json::from_value(json!({
                "name": name,
                "description": "Custom tool",
                "operation": operation,
                "args": {"idx": 42}
            }))
            .unwrap(),
            "tools.toml",
        )
        .unwrap()
    };
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/pages/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 42})))
        .expect(1)
        .mount(&api)
        .await;
    let server = server_with(
        &api,
        ServerOptions {
            dynamic_toolsets: true,
            custom_tools: vec![
                custom("home_page", "get_page_by_id"),
                custom("comment_on_home", "create_comment"),
            ],
            ..ServerOptions::default()
        },
    );
    let names = |server: &WikiServer| {
        let mut names: Vec<String> = server
            .registry()
            .iter()
            .map(|t| t.tool.name.to_string())
            .collect();
        names.sort();
        names
    };
    let mut expected: Vec<String> = CORE_TOOLS.iter().map(|s| s.to_string()).collect();
    expected.push("home_page".to_owned());
    expected.sort();
    assert_eq!(names(&server), expected);
    // Выше уровня доступа — не зарегистрирован, вызов объясняет, какой флаг нужен.
    let result = server
        .call("comment_on_home", args(json!({"body": {"body": "x"}})))
        .await
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(text(&result).contains("--allow-write"), "{}", text(&result));

    for tool in ["enable_toolset", "disable_toolset"] {
        let result = server
            .call(tool, args(json!({"toolset": "pages"})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{}", text(&result));
        assert!(server.registry().get("home_page").is_some(), "{tool}");
        // Пользовательские инструменты не попадают в списки добавленных и убранных.
        assert!(!text(&result).contains("home_page"), "{}", text(&result));
    }
    // Новая сессия тоже начинается с пользовательских инструментов.
    assert!(server.new_session().registry().get("home_page").is_some());

    let result = server.call("home_page", JsonObject::new()).await.unwrap();
    assert_eq!(result.is_error, Some(false), "{}", text(&result));
}
