//! Приём файла на одноразовый адрес (`PUT /uploads/<id>/<секрет>`, можно и `POST`), который
//! создал инструмент `create_upload_url`.
//!
//! Запрос проходит тот же слой [`guard`], что `/mcp`: `Host` и `Origin` (403), токен (401),
//! организация (400; 403 — другая организация у закреплённого сервера). Затем адрес
//! сверяется с учётными данными запроса ([`UploadUrls::redeem`]): неизвестный адрес или
//! неверный секрет — 404, другой токен или организация — 403, файл по адресу уже
//! принимается — 409, адрес использован или просрочен — 410, `Content-Length` больше
//! заявленного размера — 413, меньше — 400. Токен не хранится: загрузка идёт с учётными
//! данными этого же запроса и только пока он обрабатывается.
//!
//! Тело читается потоком и передаётся в загрузку ядра ([`WikiServer::upload_stream`]): в
//! памяти — одна часть файла (до 16 МиБ), на диск ничего не пишется. Загрузка выполняется в
//! отдельной задаче: если отправитель оборвал соединение, она всё равно доходит до конца
//! или отменяет сессию загрузки Wiki. Путь запроса (в нём секрет) в логи не попадает.

use std::io;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::header::{CONTENT_LENGTH, HOST};
use axum::http::request::Parts;
use axum::http::{HeaderName, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::put;
use futures_util::TryStreamExt;
use rmcp::model::Extensions;
use tokio_util::io::StreamReader;
use wiki_mcp_core::{
    RedeemError, StreamFailure, UPLOAD_PATH, UploadUrlLimits, UploadUrls, WikiServer,
};

use super::{Guard, HttpOptions, Rejection, RequestAuthExt, guard, json_response};

const X_FORWARDED_PROTO: HeaderName = HeaderName::from_static("x-forwarded-proto");

/// Адреса загрузки HTTP-сервера. Заголовок организации в команду `curl` попадает, только если
/// сервер не закреплён за организацией: иначе её подставит сервер.
pub fn upload_urls(options: &HttpOptions) -> anyhow::Result<Arc<UploadUrls>> {
    let urls = UploadUrls::new(
        UploadUrlLimits::default(),
        Box::new(base_url),
        options.org.is_none(),
    )
    .map_err(anyhow::Error::msg)?;
    Ok(Arc::new(urls))
}

/// Базовый адрес сервера для ссылки: `Host` MCP-запроса (его уже проверил [`guard`]) и схема
/// `https`, если reverse proxy передал `X-Forwarded-Proto: https`. Хост собирается заново из
/// разобранного значения и допускает только буквы, цифры, `.`, `-` и `:` (IPv6): в команду
/// `curl` не попадут кавычки и пробелы.
pub(super) fn base_url(extensions: &Extensions) -> Option<String> {
    let parts = extensions.get::<Parts>()?;
    let host = parts.headers.get(HOST)?.to_str().ok()?;
    let authority = super::parse_authority(host)?;
    let name = authority.host;
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':'))
    {
        return None;
    }
    let name = if name.contains(':') {
        format!("[{name}]")
    } else {
        name
    };
    let authority = match authority.port {
        Some(port) => format!("{name}:{port}"),
        None => name,
    };
    let https = parts
        .headers
        .get(X_FORWARDED_PROTO)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("https"));
    let scheme = if https { "https" } else { "http" };
    Some(format!("{scheme}://{authority}"))
}

/// Маршрут адресов загрузки под слоем [`guard`].
pub(super) fn routes(server: WikiServer, guard_state: Arc<Guard>) -> Router {
    Router::new()
        .route(
            &format!("{UPLOAD_PATH}/{{id}}/{{secret}}"),
            put(upload).post(upload),
        )
        .route_layer(middleware::from_fn_with_state(guard_state, guard))
        .with_state(server)
}

async fn upload(
    State(server): State<WikiServer>,
    Path((id, secret)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some(urls) = server.upload_urls().cloned() else {
        return error(
            StatusCode::NOT_FOUND,
            "not_found",
            "upload URLs are disabled",
        );
    };
    // `guard` уже положил учётные данные запроса в расширения.
    let Some(auth) = request
        .extensions()
        .get::<RequestAuthExt>()
        .map(|auth| auth.0.clone())
    else {
        return super::reject(Rejection::TokenMissing);
    };
    let content_length = match request.headers().get(CONTENT_LENGTH) {
        None => None,
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            Some(len) => Some(len),
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    "malformed Content-Length header",
                );
            }
        },
    };
    let redeemed = match urls.redeem(&id, &secret, &auth, content_length) {
        Ok(redeemed) => redeemed,
        Err(err) => {
            tracing::debug!(?err, "отправка на адрес загрузки отклонена");
            return redeem_error(err);
        }
    };
    let size = redeemed.target().file_size;
    let body = request.into_body();
    let task = tokio::spawn(async move {
        let stream = body.into_data_stream().map_err(io::Error::other);
        let mut reader = StreamReader::new(stream);
        let result = server
            .upload_stream(auth, redeemed.target(), &mut reader)
            .await;
        // Адрес помечается использованным в любом исходе.
        drop(redeemed);
        result
    });
    let result = match task.await {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!(%err, "задача загрузки файла завершилась аварийно");
            return error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "the upload failed unexpectedly; nothing may have been attached",
            );
        }
    };
    match result {
        Ok(text) => {
            tracing::debug!(size, "файл по адресу загрузки прикреплён");
            (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    axum::http::HeaderValue::from_static("application/json"),
                )],
                text,
            )
                .into_response()
        }
        Err(err) => {
            tracing::debug!(failure = ?err.failure, size, "загрузка по адресу не удалась");
            let (status, code) = match err.failure {
                StreamFailure::Incomplete => (StatusCode::BAD_REQUEST, "incomplete_upload"),
                StreamFailure::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
                StreamFailure::Wiki => (StatusCode::BAD_GATEWAY, "upload_failed"),
            };
            json_response(
                status,
                serde_json::json!({ "error": code, "message": err.message }),
            )
        }
    }
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    json_response(
        status,
        serde_json::json!({ "error": code, "message": message }),
    )
}

fn redeem_error(err: RedeemError) -> Response {
    match err {
        RedeemError::NotFound => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "unknown upload URL: create one with the create_upload_url tool",
        ),
        RedeemError::Forbidden => error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "this upload URL was created with another Wiki token or organization: send the \
             same Authorization and organization headers as the MCP client",
        ),
        RedeemError::InProgress => error(
            StatusCode::CONFLICT,
            "conflict",
            "a file is already being uploaded to this URL; each upload URL accepts one upload",
        ),
        RedeemError::Used => error(
            StatusCode::GONE,
            "gone",
            "this upload URL was already used; create a new one with create_upload_url",
        ),
        RedeemError::Expired => error(
            StatusCode::GONE,
            "gone",
            "this upload URL has expired; create a new one with create_upload_url",
        ),
        RedeemError::TooLarge { expected } => error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            &format!(
                "the request body is larger than {expected} bytes, the file size this upload \
                 URL was created for"
            ),
        ),
        RedeemError::TooShort { expected } => error(
            StatusCode::BAD_REQUEST,
            "incomplete_upload",
            &format!(
                "the request body is smaller than {expected} bytes, the file size this upload \
                 URL was created for"
            ),
        ),
    }
}
