//! `serve --http`: MCP-сервер через Streamable HTTP (`POST /mcp`).
//!
//! Профилей и системного хранилища здесь нет. Каждый HTTP-запрос агента несёт его токен
//! Wiki и, если сервер не закреплён за организацией, ID организации:
//!
//! - `Authorization: OAuth <токен>` — OAuth-токен, `Authorization: Bearer <токен>` — IAM-токен
//!   Yandex Cloud (так же их различает Wiki API);
//! - `X-Org-Id: <ID>` (Yandex 360) или `X-Cloud-Org-Id: <ID>` (Yandex Cloud).
//!
//! Слой [`guard`] проверяет запрос до MCP в таком порядке: `Host` и `Origin` (403, защита от
//! DNS rebinding), токен (401 с `WWW-Authenticate`), организация (400; 403 — если сервер
//! закреплён за другой организацией). Затем убирает эти заголовки из запроса и кладёт
//! учётные данные в его расширения ([`RequestAuthExt`]); `rmcp` передаёт части HTTP-запроса
//! (`http::request::Parts`) в расширения запроса MCP, откуда ядро берёт их на каждый вызов
//! инструмента ([`token_source`]). Ядро делает копию клиента API с этими данными только на
//! время вызова: токен уходит только в Wiki API, нигде не сохраняется и не пишется в логи.
//!
//! Сервер без состояния (stateless Streamable HTTP, ответы `application/json`): сессий нет,
//! каждый POST обрабатывается независимо, поэтому токен одного запроса не переживает его.
//! `rmcp` повторно проверяет `Host` и `Origin` с теми же списками и ограничивает размер тела.
//!
//! Одноразовые адреса загрузки файлов (`/uploads/...`, инструмент `create_upload_url`) —
//! в [`uploads`]: тот же слой [`guard`], токен — из заголовка запроса с файлом.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN, WWW_AUTHENTICATE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::model::Extensions;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;
use wiki_api::executor::{Credentials, OrgId};
use wiki_api::{ApiToken, OrgKind, Organization};
use wiki_mcp_core::{RequestAuth, TokenSource, WikiServer};

mod uploads;

pub use uploads::upload_urls;

/// Адрес по умолчанию: только локальные подключения.
pub const DEFAULT_LISTEN: &str = "127.0.0.1:8767";
/// Путь MCP-эндпоинта.
pub const MCP_PATH: &str = "/mcp";
/// Путь проверки готовности (без токена).
pub const HEALTH_PATH: &str = "/health";
/// Имена хостов, разрешённые в `Host` всегда (с любым портом).
pub const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];
/// Лимит тела запроса MCP по умолчанию: 1 MiB. Аргументы инструментов — JSON без файлов.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024;
/// Максимальная длина токена в заголовке: больше — явно не токен (IAM-токены — сотни байт).
const MAX_TOKEN_LEN: usize = 4096;
const OAUTH_CHALLENGE: &str = "OAuth realm=\"wiki-mcp\"";
const BEARER_CHALLENGE: &str = "Bearer realm=\"wiki-mcp\"";
const X_ORG_ID: HeaderName = HeaderName::from_static("x-org-id");
const X_CLOUD_ORG_ID: HeaderName = HeaderName::from_static("x-cloud-org-id");

/// Настройки HTTP-транспорта.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// Допустимые значения `Host` (имя — с любым портом, или `имя:порт`).
    pub allowed_hosts: Vec<String>,
    /// Допустимые `Origin` браузерных клиентов; пусто — любой присланный `Origin` отклоняется.
    pub allowed_origins: Vec<String>,
    /// Организация сервера (`--org-id`, `--cloud-org-id`). Задана — запросам можно не
    /// передавать заголовок организации, а другая организация в заголовке отклоняется.
    pub org: Option<OrgId>,
    /// Лимит тела запроса MCP, байт.
    pub max_request_bytes: usize,
}

/// Учётные данные текущего HTTP-запроса в его расширениях. `Debug` не раскрывает значения.
#[derive(Debug, Clone)]
pub struct RequestAuthExt(RequestAuth);

/// Источник учётных данных для ядра: из расширений HTTP-запроса, которые `rmcp` кладёт
/// в расширения запроса MCP как `http::request::Parts`.
pub fn token_source() -> TokenSource {
    TokenSource::PerRequest(Arc::new(|extensions: &Extensions| {
        extensions
            .get::<Parts>()
            .and_then(|parts| parts.extensions.get::<RequestAuthExt>())
            .map(|auth| auth.0.clone())
    }))
}

/// Разрешённые `Host`: loopback-имена, конкретный адрес из `--listen` и `--allowed-host`.
pub fn allowed_hosts(listen: SocketAddr, extra: &[String]) -> Vec<String> {
    let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|h| (*h).to_owned()).collect();
    let ip = listen.ip();
    if !ip.is_unspecified() && !ip.is_loopback() {
        hosts.push(ip.to_string());
    }
    for host in extra {
        let host = host.trim().to_owned();
        if !host.is_empty() && !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    hosts
}

/// Маршруты сервера: `/mcp` (с токеном), `/health` (без токена) и, если к серверу подключены
/// адреса загрузки, `/uploads/...` (с токеном).
pub fn router(server: WikiServer, options: &HttpOptions, cancel: CancellationToken) -> Router {
    let upload_server = server.upload_urls().is_some().then(|| server.clone());
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(options.allowed_hosts.clone())
        .with_allowed_origins(options.allowed_origins.clone())
        // Пустой список `Origin` — отклонять любой присланный `Origin`, а не пропускать все.
        .enforce_origin_validation()
        // Без сессий: каждый запрос самостоятельный, сервер ничего не хранит между запросами.
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_max_request_body_bytes(options.max_request_bytes)
        .with_cancellation_token(cancel);
    // `WikiServer` без состояния сессии: копия на запрос делит только неизменяемые данные.
    let mcp = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let guard_state = Arc::new(Guard {
        allowed_hosts: options.allowed_hosts.clone(),
        allowed_origins: options.allowed_origins.clone(),
        org: options.org.clone(),
    });
    let mcp_routes =
        Router::new()
            .route_service(MCP_PATH, mcp)
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&guard_state),
                guard,
            ));
    let mut app = Router::new()
        .route(HEALTH_PATH, get(health))
        .merge(mcp_routes);
    if let Some(server) = upload_server {
        app = app.merge(uploads::routes(server, guard_state));
    }
    app
}

/// Запускает сервер и ждёт SIGINT/SIGTERM.
pub async fn run(
    server: WikiServer,
    listen: SocketAddr,
    options: HttpOptions,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let addr = listener
        .local_addr()
        .context("cannot read the listen address")?;
    if !addr.ip().is_loopback() {
        // Предупреждение для человека, поэтому в stderr напрямую, независимо от уровня логов:
        // токены идут открытым текстом, снаружи сервер должен быть доступен только через TLS.
        eprintln!(
            "warning: listening on {addr}, not only on loopback. Tokens travel in plain HTTP: \
             expose the server only through a TLS reverse proxy or a private network."
        );
    }
    tracing::info!(
        %addr,
        allowed_hosts = ?options.allowed_hosts,
        allowed_origins = ?options.allowed_origins,
        // Только тип организации: ID в логи не пишется.
        org = options.org.as_ref().map_or("from requests", OrgId::header_name),
        max_request_bytes = options.max_request_bytes,
        "запуск MCP-сервера Wiki по HTTP"
    );
    eprintln!("wiki-mcp: MCP endpoint http://{addr}{MCP_PATH}");

    let cancel = CancellationToken::new();
    let app = router(server, &options, cancel.clone());
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("получен сигнал завершения, останавливаю сервер");
            cancel.cancel();
        })
        .await
        .context("HTTP server failed")?;
    Ok(())
}

/// Ждёт SIGINT (Ctrl+C) или SIGTERM (`docker stop`, systemd).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(%err, "не удалось подписаться на SIGINT");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "не удалось подписаться на SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

async fn health() -> Response {
    json_response(
        StatusCode::OK,
        serde_json::json!({
            "status": "ok",
            "service": "wiki-mcp",
            "version": env!("CARGO_PKG_VERSION"),
            "mcp_endpoint": MCP_PATH,
        }),
    )
}

/// Настройки слоя проверки запросов `/mcp`.
#[derive(Debug)]
struct Guard {
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    org: Option<OrgId>,
}

/// Почему запрос отклонён до MCP. Значения заголовков в ответ и в логи не попадают.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rejection {
    HostMissing,
    HostNotAllowed,
    OriginNotAllowed,
    TokenMissing,
    TokenMalformed,
    OrgMissing,
    OrgAmbiguous,
    OrgMalformed,
    OrgNotAllowed,
}

/// Слой `/mcp` и `/uploads/...`: без допустимых `Host`/`Origin`, токена и организации запрос
/// не доходит до обработчика.
///
/// `Authorization` и заголовки организации убираются из запроса, а их значения кладутся в
/// расширения как [`RequestAuthExt`]: так исходные заголовки не попадут ни в какой отладочный
/// вывод частей запроса.
async fn guard(State(guard): State<Arc<Guard>>, mut request: Request, next: Next) -> Response {
    let auth = match check_request(&guard, request.headers()) {
        Ok(auth) => auth,
        Err(rejection) => {
            tracing::debug!(?rejection, "запрос к MCP отклонён до обработки");
            return reject(rejection);
        }
    };
    let headers = request.headers_mut();
    headers.remove(AUTHORIZATION);
    headers.remove(X_ORG_ID);
    headers.remove(X_CLOUD_ORG_ID);
    request.extensions_mut().insert(RequestAuthExt(auth));
    next.run(request).await
}

fn check_request(guard: &Guard, headers: &HeaderMap) -> Result<RequestAuth, Rejection> {
    check_host(headers, &guard.allowed_hosts)?;
    check_origin(headers, &guard.allowed_origins)?;
    let credentials = request_credentials(headers)?;
    let org = resolve_org(request_org(headers)?, guard.org.as_ref())?;
    Ok(RequestAuth {
        credentials,
        org: Some(org),
    })
}

/// Учётные данные из единственного заголовка `Authorization`: `OAuth <токен>` — OAuth-токен,
/// `Bearer <токен>` — IAM-токен. Схема — без учёта регистра.
fn request_credentials(headers: &HeaderMap) -> Result<Credentials, Rejection> {
    let value = single_header(headers, &AUTHORIZATION)
        .map_err(|()| Rejection::TokenMalformed)?
        .ok_or(Rejection::TokenMissing)?;
    let (scheme, token) = value
        .trim()
        .split_once(' ')
        .ok_or(Rejection::TokenMalformed)?;
    let token = token.trim();
    if token.is_empty()
        || token.len() > MAX_TOKEN_LEN
        || !token.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(Rejection::TokenMalformed);
    }
    let token = ApiToken::new(token);
    if scheme.eq_ignore_ascii_case("oauth") {
        Ok(Credentials::oauth(token))
    } else if scheme.eq_ignore_ascii_case("bearer") {
        Ok(Credentials::iam(token))
    } else {
        Err(Rejection::TokenMalformed)
    }
}

/// Организация из заголовков запроса: `X-Org-Id` или `X-Cloud-Org-Id`, не оба сразу.
fn request_org(headers: &HeaderMap) -> Result<Option<OrgId>, Rejection> {
    let yandex360 = single_header(headers, &X_ORG_ID).map_err(|()| Rejection::OrgMalformed)?;
    let cloud = single_header(headers, &X_CLOUD_ORG_ID).map_err(|()| Rejection::OrgMalformed)?;
    let (kind, id) = match (yandex360, cloud) {
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => return Err(Rejection::OrgAmbiguous),
        (Some(id), None) => (OrgKind::Yandex360, id),
        (None, Some(id)) => (OrgKind::Cloud, id),
    };
    let org = Organization::new(kind, id).map_err(|_| Rejection::OrgMalformed)?;
    Ok(Some(OrgId::from(&org)))
}

/// Организация вызова. Если сервер закреплён за организацией (`--org-id`), запрос может её
/// не указывать или указать ту же; другая организация отклоняется: флаг сервера — решение
/// оператора, которое агент не обходит заголовком.
fn resolve_org(request: Option<OrgId>, server: Option<&OrgId>) -> Result<OrgId, Rejection> {
    match (request, server) {
        (Some(request), Some(server)) if &request != server => Err(Rejection::OrgNotAllowed),
        (Some(request), _) => Ok(request),
        (None, Some(server)) => Ok(server.clone()),
        (None, None) => Err(Rejection::OrgMissing),
    }
}

/// Значение заголовка, если он есть ровно один раз и это ASCII; `Err` — повтор или не ASCII.
fn single_header<'a>(headers: &'a HeaderMap, name: &HeaderName) -> Result<Option<&'a str>, ()> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    value.to_str().map(Some).map_err(|_| ())
}

/// Хост и порт без скобок IPv6 и в нижнем регистре.
#[derive(Debug, PartialEq, Eq)]
struct Authority {
    host: String,
    port: Option<u16>,
}

fn normalize_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase()
}

fn parse_authority(value: &str) -> Option<Authority> {
    let authority = axum::http::uri::Authority::try_from(value.trim()).ok()?;
    Some(Authority {
        host: normalize_host(authority.host()),
        port: authority.port_u16(),
    })
}

/// Защита от DNS rebinding: имя в `Host` должно быть в списке. Запись без порта разрешает
/// любой порт, запись `имя:порт` — только этот порт. Те же правила у `rmcp`.
fn check_host(headers: &HeaderMap, allowed: &[String]) -> Result<(), Rejection> {
    let host = single_header(headers, &HOST)
        .ok()
        .flatten()
        .and_then(parse_authority)
        .ok_or(Rejection::HostMissing)?;
    let allowed = allowed.iter().any(|entry| {
        // `::1` без скобок не разбирается как authority: такие записи — имя без порта.
        let entry = parse_authority(entry).unwrap_or_else(|| Authority {
            host: normalize_host(entry.trim()),
            port: None,
        });
        entry.host == host.host && entry.port.is_none_or(|port| host.port == Some(port))
    });
    if allowed {
        Ok(())
    } else {
        Err(Rejection::HostNotAllowed)
    }
}

/// `Origin` браузера: (схема, хост, порт); `None` — `Origin: null`.
#[derive(Debug, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    }
}

fn parse_origin(value: &str) -> Option<Origin> {
    let uri = axum::http::Uri::try_from(value.trim()).ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let authority = uri.authority()?;
    let port = authority.port_u16().or_else(|| default_port(&scheme));
    Some(Origin {
        host: normalize_host(authority.host()),
        scheme,
        port,
    })
}

/// Запрос без `Origin` (обычный MCP-клиент, не браузер) проходит. С `Origin` — только если
/// он в списке: `схема://хост[:порт]` (без порта — порт схемы по умолчанию), `схема://хост:*`
/// (любой порт) или `null`.
fn check_origin(headers: &HeaderMap, allowed: &[String]) -> Result<(), Rejection> {
    let value = match single_header(headers, &ORIGIN) {
        Ok(None) => return Ok(()),
        Ok(Some(value)) => value.trim(),
        Err(()) => return Err(Rejection::OriginNotAllowed),
    };
    let matches = |entry: &String| {
        let entry = entry.trim();
        if entry.eq_ignore_ascii_case("null") || value.eq_ignore_ascii_case("null") {
            return entry.eq_ignore_ascii_case("null") && value.eq_ignore_ascii_case("null");
        }
        let Some(origin) = parse_origin(value) else {
            return false;
        };
        match entry.strip_suffix(":*") {
            Some(base) => parse_origin(base)
                .is_some_and(|base| base.scheme == origin.scheme && base.host == origin.host),
            None => parse_origin(entry) == Some(origin),
        }
    };
    if allowed.iter().any(matches) {
        Ok(())
    } else {
        Err(Rejection::OriginNotAllowed)
    }
}

fn reject(rejection: Rejection) -> Response {
    let (status, error, message) = match rejection {
        Rejection::HostMissing => (
            StatusCode::BAD_REQUEST,
            "bad_request",
            "missing or malformed Host header",
        ),
        Rejection::HostNotAllowed => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Host header is not allowed: start the server with --allowed-host <name> for this \
             name",
        ),
        Rejection::OriginNotAllowed => (
            StatusCode::FORBIDDEN,
            "forbidden",
            "Origin header is not allowed: start the server with --allowed-origin <origin> for \
             this browser client",
        ),
        Rejection::TokenMissing => (
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing Wiki token: send `Authorization: OAuth <token>` (OAuth token) or \
             `Authorization: Bearer <token>` (Yandex Cloud IAM token)",
        ),
        Rejection::TokenMalformed => (
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "malformed Authorization header: expected exactly one `Authorization: OAuth <token>` \
             or `Authorization: Bearer <token>`",
        ),
        Rejection::OrgMissing => (
            StatusCode::BAD_REQUEST,
            "missing_organization",
            "missing Wiki organization: send `X-Org-Id: <ID>` (Yandex 360 for Business) or \
             `X-Cloud-Org-Id: <ID>` (Yandex Cloud Organization)",
        ),
        Rejection::OrgAmbiguous => (
            StatusCode::BAD_REQUEST,
            "invalid_organization",
            "send either X-Org-Id or X-Cloud-Org-Id, not both",
        ),
        Rejection::OrgMalformed => (
            StatusCode::BAD_REQUEST,
            "invalid_organization",
            "malformed organization header: expected one X-Org-Id or X-Cloud-Org-Id with 1-64 \
             characters from A-Z, a-z, 0-9, `-`, `_`",
        ),
        Rejection::OrgNotAllowed => (
            StatusCode::FORBIDDEN,
            "organization_not_allowed",
            "this server is bound to another Wiki organization (--org-id or --cloud-org-id); \
             remove the organization header or use the server's organization",
        ),
    };
    let mut response = json_response(
        status,
        serde_json::json!({ "error": error, "message": message }),
    );
    if status == StatusCode::UNAUTHORIZED {
        let suffix = if rejection == Rejection::TokenMalformed {
            ", error=\"invalid_request\""
        } else {
            ""
        };
        // Две схемы — два вызова (challenge), каждый в своём заголовке.
        for challenge in [OAUTH_CHALLENGE, BEARER_CHALLENGE] {
            if let Ok(value) = HeaderValue::from_str(&format!("{challenge}{suffix}")) {
                response.headers_mut().append(WWW_AUTHENTICATE, value);
            }
        }
    }
    response
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [(CONTENT_TYPE, HeaderValue::from_static("application/json"))],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests;
