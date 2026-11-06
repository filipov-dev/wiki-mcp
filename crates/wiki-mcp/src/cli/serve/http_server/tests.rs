//! Сквозные тесты HTTP-режима: настоящий HTTP-сервер на loopback, Wiki API подменён
//! `wiremock`, запросы MCP отправляются вручную через `reqwest`.

use std::time::Duration;

use serde_json::{Value, json};
use wiki_api::AccessLevel;
use wiki_api::executor::{ApiClient, ClientConfig};
use wiki_mcp_core::{FilePolicy, ServerOptions, WikiServer};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request as MockRequest, ResponseTemplate};

use super::*;

const PROTOCOL_VERSION: &str = "2025-06-18";

struct TestServer {
    base: String,
    http: reqwest::Client,
    cancel: CancellationToken,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

struct Settings<'a> {
    access: AccessLevel,
    origins: &'a [&'a str],
    org: Option<OrgId>,
    max_request_bytes: usize,
}

impl Default for Settings<'_> {
    fn default() -> Self {
        Self {
            access: AccessLevel::Read,
            origins: &[],
            org: None,
            max_request_bytes: DEFAULT_MAX_REQUEST_BYTES,
        }
    }
}

async fn start(api: &MockServer, settings: Settings<'_>) -> TestServer {
    let mut config = ClientConfig::new(None, None).unwrap();
    config.base_url = api.uri().parse().unwrap();
    config.timeout = Duration::from_secs(10);
    config.retry.max_retries = 0;
    let client = ApiClient::new(config).unwrap();
    // Файловая политика с каталогом передаётся нарочно: без `local_machine` ядро её отключает.
    let dir = std::env::temp_dir();
    let server = WikiServer::with_token_source(
        client,
        ServerOptions {
            max_access: settings.access,
            toolsets: Some(vec!["pages".into(), "attachments".into()]),
            local_machine: false,
            profile: None,
        },
        FilePolicy::new(&[dir]).unwrap(),
        token_source(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let options = HttpOptions {
        allowed_hosts: allowed_hosts(addr, &[]),
        allowed_origins: settings.origins.iter().map(|o| (*o).to_owned()).collect(),
        org: settings.org,
        max_request_bytes: settings.max_request_bytes,
    };
    let cancel = CancellationToken::new();
    let app = router(server, &options, cancel.clone());
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled_owned().await })
            .await;
    });
    TestServer {
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        cancel,
    }
}

impl TestServer {
    /// Запрос MCP без учётных данных.
    fn post(&self, body: &Value) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{}{MCP_PATH}", self.base))
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .body(body.to_string())
    }

    /// Запрос MCP с OAuth-токеном и организацией Yandex 360.
    fn authed(&self, token: &str, body: &Value) -> reqwest::RequestBuilder {
        self.post(body)
            .header("authorization", format!("OAuth {token}"))
            .header("x-org-id", "12345")
    }

    async fn send(request: reqwest::RequestBuilder) -> (u16, String) {
        let response = request.send().await.unwrap();
        (response.status().as_u16(), response.text().await.unwrap())
    }

    async fn rpc(&self, token: &str, body: &Value) -> (u16, String) {
        Self::send(self.authed(token, body)).await
    }

    async fn call_tool(&self, token: &str, name: &str, args: Value) -> Value {
        let (status, body) = self.rpc(token, &call(name, args)).await;
        assert_eq!(status, 200, "{body}");
        let message: Value = serde_json::from_str(&body).unwrap();
        message["result"].clone()
    }
}

fn initialize() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }
    })
}

fn call(name: &str, args: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {"name": name, "arguments": args}
    })
}

fn result_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn health_needs_no_token() {
    let api = MockServer::start().await;
    let server = start(&api, Settings::default()).await;
    let response = server
        .http
        .get(format!("{}{HEALTH_PATH}", server.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["status"], "ok");
    assert_eq!(body["service"], "wiki-mcp");
    assert_eq!(body["mcp_endpoint"], MCP_PATH);
}

#[tokio::test]
async fn missing_or_malformed_token_is_401_before_mcp() {
    let api = MockServer::start().await;
    let server = start(&api, Settings::default()).await;

    let response = server
        .post(&initialize())
        .header("x-org-id", "12345")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    let challenges: Vec<&str> = response
        .headers()
        .get_all("www-authenticate")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    assert_eq!(
        challenges,
        ["OAuth realm=\"wiki-mcp\"", "Bearer realm=\"wiki-mcp\""]
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("Authorization: OAuth <token>"), "{body}");

    for value in [
        "Basic dXNlcjpwYXNz",
        "OAuth",
        "OAuth   ",
        "oauth a b",
        "Token abc-secret",
        "abc-secret",
    ] {
        let response = server
            .post(&initialize())
            .header("authorization", value)
            .header("x-org-id", "12345")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 401, "{value}");
        let challenge = response.headers()["www-authenticate"].to_str().unwrap();
        assert!(challenge.contains("error=\"invalid_request\""), "{value}");
        let body = response.text().await.unwrap();
        assert!(
            !body.contains("dXNlcjpwYXNz") && !body.contains("abc-secret"),
            "{body}"
        );
    }

    // Два заголовка Authorization — неоднозначно, отклоняется.
    let response = server
        .post(&initialize())
        .header("authorization", "OAuth one")
        .header("authorization", "OAuth two")
        .header("x-org-id", "12345")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);

    // Схема без учёта регистра; Bearer — IAM-токен.
    for value in ["oauth fake-token", "BEARER fake-iam-token"] {
        let response = server
            .post(&initialize())
            .header("authorization", value)
            .header("x-org-id", "12345")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200, "{value}");
    }

    // До API ничего не дошло.
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn foreign_host_and_origin_are_403_even_without_token() {
    let api = MockServer::start().await;
    let server = start(
        &api,
        Settings {
            origins: &["http://localhost:6274"],
            ..Settings::default()
        },
    )
    .await;

    let (status, _) = server.rpc("fake-token", &initialize()).await;
    assert_eq!(status, 200, "без Origin (не браузер) запрос проходит");

    // DNS rebinding: имя злоумышленника в Host. Проверка Host — раньше токена.
    for authed in [true, false] {
        let request = if authed {
            server.authed("fake-token", &initialize())
        } else {
            server.post(&initialize())
        };
        let response = request.header("host", "evil.example").send().await.unwrap();
        assert_eq!(response.status().as_u16(), 403, "authed={authed}");
        assert!(response.headers().get("www-authenticate").is_none());
        let body = response.text().await.unwrap();
        assert!(body.contains("--allowed-host"), "{body}");
    }

    for (origin, expected) in [
        ("http://evil.example", 403),
        ("http://localhost:6275", 403),
        ("https://localhost:6274", 403),
        ("null", 403),
        ("http://localhost:6274", 200),
        ("http://LOCALHOST:6274", 200),
    ] {
        let (status, body) = TestServer::send(
            server
                .authed("fake-token", &initialize())
                .header("origin", origin),
        )
        .await;
        assert_eq!(status, expected, "{origin}: {body}");
    }
    let (status, _) = TestServer::send(
        server
            .post(&initialize())
            .header("origin", "http://evil.example"),
    )
    .await;
    assert_eq!(status, 403);
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn origin_is_rejected_by_default() {
    let api = MockServer::start().await;
    let server = start(&api, Settings::default()).await;
    let (status, body) = TestServer::send(
        server
            .authed("fake-token", &initialize())
            .header("origin", "http://localhost:3000"),
    )
    .await;
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("--allowed-origin"), "{body}");
}

#[tokio::test]
async fn organization_comes_from_request_or_server() {
    let api = MockServer::start().await;
    let server = start(&api, Settings::default()).await;
    let with = |headers: &[(&str, &str)]| {
        let mut request = server
            .post(&initialize())
            .header("authorization", "OAuth fake-token");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request
    };

    let (status, body) = TestServer::send(with(&[])).await;
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("missing_organization"), "{body}");
    assert!(body.contains("X-Org-Id") && body.contains("X-Cloud-Org-Id"));

    for headers in [
        &[("x-org-id", "1"), ("x-cloud-org-id", "2")][..],
        &[("x-org-id", "a b")][..],
        &[("x-org-id", "")][..],
        &[("x-cloud-org-id", "x/../y")][..],
    ] {
        let (status, body) = TestServer::send(with(headers)).await;
        assert_eq!(status, 400, "{headers:?}: {body}");
        assert!(body.contains("invalid_organization"), "{body}");
        assert!(!body.contains("x/../y"), "{body}");
    }
    let (status, _) = TestServer::send(with(&[("x-cloud-org-id", "bpf-cloud_1")])).await;
    assert_eq!(status, 200);

    // Сервер закреплён за организацией: заголовок не нужен, та же — можно, другая — 403.
    let pinned = start(
        &api,
        Settings {
            org: Some(OrgId::Yandex360("12345".into())),
            ..Settings::default()
        },
    )
    .await;
    for (headers, expected) in [
        (&[][..], 200),
        (&[("x-org-id", "12345")][..], 200),
        (&[("x-org-id", "999")][..], 403),
        (&[("x-cloud-org-id", "12345")][..], 403),
    ] {
        let mut request = pinned
            .post(&initialize())
            .header("authorization", "OAuth fake-token");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let (status, body) = TestServer::send(request).await;
        assert_eq!(status, expected, "{headers:?}: {body}");
        if expected == 403 {
            assert!(body.contains("organization_not_allowed"), "{body}");
        }
    }
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn request_body_is_limited() {
    let api = MockServer::start().await;
    let server = start(
        &api,
        Settings {
            max_request_bytes: 1024,
            ..Settings::default()
        },
    )
    .await;
    let big = call("get_page", json!({"slug": "x".repeat(4096)}));
    let (status, _) = server.rpc("fake-token", &big).await;
    assert_eq!(status, 413);
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn initialize_and_tools_have_no_local_machine_features() {
    let api = MockServer::start().await;
    let server = start(
        &api,
        Settings {
            access: AccessLevel::Destructive,
            ..Settings::default()
        },
    )
    .await;

    let (status, body) = server.rpc("fake-token", &initialize()).await;
    assert_eq!(status, 200, "{body}");
    let init: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "wiki-mcp");
    let instructions = init["result"]["instructions"].as_str().unwrap();
    for absent in ["auth login", "--allow-path", "allowed directories"] {
        assert!(!instructions.contains(absent), "{absent}");
    }
    assert!(instructions.contains("HTTP mode"), "{instructions}");

    let (status, body) = server
        .rpc(
            "fake-token",
            &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let list: Value = serde_json::from_str(&body).unwrap();
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"get_page"), "{names:?}");
    assert!(names.contains(&"get_current_user"), "{names:?}");
    assert!(names.contains(&"api_request"), "{names:?}");
    // Загрузка файлов и скачивание (`file`, `save_to`) без локальных файлов не регистрируются:
    // сравниваем с тем же сервером на машине агента.
    let local = wiki_mcp_core::ToolRegistry::new(&ServerOptions {
        max_access: AccessLevel::Destructive,
        toolsets: Some(vec!["pages".into(), "attachments".into()]),
        local_machine: true,
        profile: None,
    })
    .unwrap();
    let local_only: Vec<String> = local
        .tools()
        .into_iter()
        .map(|t| t.name.to_string())
        .filter(|name| !names.contains(&name.as_str()))
        .collect();
    assert!(
        local_only.iter().any(|name| name == "upload_file_part"),
        "{local_only:?}"
    );
    assert!(!body.contains("\"save_to\""), "save_to");

    // Файловый инструмент, вызванный по имени, — ошибка инструмента, к API запрос не идёт.
    let result = server
        .call_tool(
            "fake-token",
            "upload_file_part",
            json!({"session_id": "s-1", "part_number": 1, "file": "/etc/hosts"}),
        )
        .await;
    assert_eq!(result["isError"], true, "{result}");
    for name in [
        "download_attachment",
        "download_attachment_by_url",
        "download_attachment_preview",
    ] {
        assert!(!names.contains(&name), "{name}");
    }
    // Скачивание через `api_request` в файл сервера тоже отклоняется до запроса к API.
    let result = server
        .call_tool(
            "fake-token",
            "api_request",
            json!({
                "method": "GET",
                "path": "/v1/pages/1/attachments/2/download",
                "save_to": "/tmp/wiki-mcp-http-test.bin"
            }),
        )
        .await;
    assert_eq!(result["isError"], true, "{result}");
    assert!(api.received_requests().await.unwrap_or_default().is_empty());
}

#[tokio::test]
async fn each_request_forwards_its_own_token_and_organization() {
    let api = MockServer::start().await;
    for (scheme, token, org_header, org) in [
        ("OAuth", "agent-alpha-token", "x-org-id", "111"),
        ("Bearer", "agent-beta-iam", "x-cloud-org-id", "cloud-222"),
    ] {
        Mock::given(method("GET"))
            .and(path("/v1/users/me"))
            .and(header(
                "authorization",
                format!("{scheme} {token}").as_str(),
            ))
            .and(header(org_header, org))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": token})))
            .expect(1)
            .mount(&api)
            .await;
    }
    let server = start(&api, Settings::default()).await;

    for (scheme, token, org_header, org) in [
        ("OAuth", "agent-alpha-token", "x-org-id", "111"),
        ("Bearer", "agent-beta-iam", "x-cloud-org-id", "cloud-222"),
    ] {
        let request = server
            .post(&call("get_current_user", json!({})))
            .header("authorization", format!("{scheme} {token}"))
            .header(org_header, org);
        let (status, body) = TestServer::send(request).await;
        assert_eq!(status, 200, "{body}");
        let message: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(message["result"]["isError"], false, "{body}");
    }
    // Ожидания wiremock (`expect(1)` на каждый токен) проверяются здесь.
    api.verify().await;
    // В API ушли только заголовки клиента API: без лишних копий организации.
    for request in api.received_requests().await.unwrap() {
        let orgs = request.headers.get_all("x-org-id").iter().count()
            + request.headers.get_all("x-cloud-org-id").iter().count();
        assert_eq!(orgs, 1);
    }
}

#[tokio::test]
async fn server_organization_is_sent_when_request_has_none() {
    let api = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/users/me"))
        .and(header("authorization", "OAuth pinned-token"))
        .and(header("x-cloud-org-id", "cloud-org"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "robot"})))
        .expect(1)
        .mount(&api)
        .await;
    let server = start(
        &api,
        Settings {
            org: Some(OrgId::Cloud("cloud-org".into())),
            ..Settings::default()
        },
    )
    .await;
    let request = server
        .post(&call("get_current_user", json!({})))
        .header("authorization", "OAuth pinned-token");
    let (status, body) = TestServer::send(request).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"isError\":false"), "{body}");
    api.verify().await;
}

#[tokio::test]
async fn token_never_appears_in_results() {
    const TOKEN: &str = "leaky-token-7d1e";
    let api = MockServer::start().await;
    Mock::given(path("/v1/users/me"))
        .respond_with(|req: &MockRequest| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(401).set_body_string(format!(
                r#"{{"error_code":"NOT_AUTHENTICATED","debug_message":"invalid token: {auth}"}}"#
            ))
        })
        .mount(&api)
        .await;
    Mock::given(path("/v1/pages/1"))
        .respond_with(|req: &MockRequest| {
            let auth = req.headers.get("authorization").unwrap().to_str().unwrap();
            ResponseTemplate::new(200).set_body_json(json!({"title": format!("you are {auth}")}))
        })
        .mount(&api)
        .await;
    let server = start(&api, Settings::default()).await;

    let (status, body) = server
        .rpc(TOKEN, &call("get_current_user", json!({})))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"isError\":true"), "{body}");
    assert!(body.contains("401"), "{body}");
    assert!(!body.contains(TOKEN), "{body}");

    let result = server
        .call_tool(TOKEN, "get_page_by_id", json!({"idx": 1}))
        .await;
    let text = result_text(&result);
    assert!(text.contains("you are"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
}

#[test]
fn credentials_parsing() {
    let headers = |values: &[&str]| {
        let mut map = HeaderMap::new();
        for value in values {
            map.append(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        }
        map
    };
    let oauth = request_credentials(&headers(&["OAuth y0_abc.DEF-123_~+/="])).unwrap();
    assert_eq!(
        oauth,
        Credentials::oauth(ApiToken::new("y0_abc.DEF-123_~+/="))
    );
    let iam = request_credentials(&headers(&["  BEARER   t1.iam  "])).unwrap();
    assert_eq!(iam, Credentials::iam(ApiToken::new("t1.iam")));
    assert_eq!(
        request_credentials(&headers(&[])).unwrap_err(),
        Rejection::TokenMissing
    );
    let long = format!("OAuth {}", "a".repeat(MAX_TOKEN_LEN + 1));
    for bad in [
        "OAuth",
        "Basic abc",
        "OAuth a b",
        "Token abc",
        long.as_str(),
    ] {
        assert_eq!(
            request_credentials(&headers(&[bad])).unwrap_err(),
            Rejection::TokenMalformed,
            "{bad}"
        );
    }
    assert_eq!(
        request_credentials(&headers(&["OAuth a", "OAuth b"])).unwrap_err(),
        Rejection::TokenMalformed
    );
}

#[test]
fn organization_resolution() {
    let y = |id: &str| OrgId::Yandex360(id.into());
    assert_eq!(resolve_org(Some(y("1")), None), Ok(y("1")));
    assert_eq!(resolve_org(None, Some(&y("1"))), Ok(y("1")));
    assert_eq!(resolve_org(Some(y("1")), Some(&y("1"))), Ok(y("1")));
    assert_eq!(
        resolve_org(Some(y("2")), Some(&y("1"))),
        Err(Rejection::OrgNotAllowed)
    );
    assert_eq!(
        resolve_org(Some(OrgId::Cloud("1".into())), Some(&y("1"))),
        Err(Rejection::OrgNotAllowed)
    );
    assert_eq!(resolve_org(None, None), Err(Rejection::OrgMissing));
}

#[test]
fn host_matching() {
    let check = |host: &str, allowed: &[&str]| {
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_str(host).unwrap());
        let allowed: Vec<String> = allowed.iter().map(|h| (*h).to_owned()).collect();
        check_host(&headers, &allowed)
    };
    let loopback = LOOPBACK_HOSTS;
    for host in [
        "localhost:8767",
        "127.0.0.1:1",
        "[::1]:8767",
        "[::1]",
        "LOCALHOST",
    ] {
        assert_eq!(check(host, loopback), Ok(()), "{host}");
    }
    assert_eq!(
        check("evil.example:8767", loopback),
        Err(Rejection::HostNotAllowed)
    );
    assert_eq!(
        check("mcp.example.com:443", &["mcp.example.com:443"]),
        Ok(())
    );
    assert_eq!(
        check("mcp.example.com:8443", &["mcp.example.com:443"]),
        Err(Rejection::HostNotAllowed)
    );
    assert_eq!(check("", loopback), Err(Rejection::HostMissing));
    assert_eq!(
        check_host(&HeaderMap::new(), &["localhost".to_owned()]),
        Err(Rejection::HostMissing)
    );
}

#[test]
fn origin_matching() {
    let check = |origin: &str, allowed: &[&str]| {
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(origin).unwrap());
        let allowed: Vec<String> = allowed.iter().map(|h| (*h).to_owned()).collect();
        check_origin(&headers, &allowed).is_ok()
    };
    assert!(check_origin(&HeaderMap::new(), &[]).is_ok());
    assert!(!check("http://localhost:6274", &[]));
    assert!(check("http://localhost:6274", &["http://localhost:6274"]));
    assert!(check("https://app.example", &["https://app.example:443"]));
    assert!(check("https://app.example:443", &["https://app.example"]));
    assert!(!check("https://app.example:8443", &["https://app.example"]));
    assert!(check("http://localhost:1234", &["http://localhost:*"]));
    assert!(!check("https://localhost:1234", &["http://localhost:*"]));
    assert!(check("null", &["null"]));
    assert!(!check("null", &["http://localhost:*"]));
    assert!(!check("not an origin", &["http://localhost:*"]));
}

#[test]
fn allowed_hosts_include_specific_listen_address() {
    let loopback = allowed_hosts("127.0.0.1:8767".parse().unwrap(), &[]);
    assert_eq!(loopback, LOOPBACK_HOSTS);
    let any = allowed_hosts(
        "0.0.0.0:8767".parse().unwrap(),
        &["wiki-mcp".into(), " ".into(), "localhost".into()],
    );
    assert_eq!(any, ["localhost", "127.0.0.1", "::1", "wiki-mcp"]);
    let specific = allowed_hosts("10.0.0.5:8767".parse().unwrap(), &[]);
    assert_eq!(specific, ["localhost", "127.0.0.1", "::1", "10.0.0.5"]);
}
