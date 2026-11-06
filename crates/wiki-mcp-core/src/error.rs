//! Ошибки вызова инструмента.
//!
//! Любая ошибка вызова возвращается MCP-клиенту как `CallToolResult` с `isError: true`
//! и текстом для агента на английском: агент должен понять, что исправить в аргументах
//! или что попросить у пользователя. Токен в тексты ошибок не попадает: [`ApiError`]
//! его не содержит, аргументы инструмента токена не принимают, а сервер дополнительно
//! вырезает токен клиента из итогового текста.

use std::fmt;

use serde_json::Value;
use wiki_api::executor::{ApiError, HttpError, MAX_FILE_BODY_BYTES, MIN_UPLOAD_PART_BYTES};

/// Ошибка вызова инструмента с текстом для агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    pub fn message(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ToolError {}

impl From<ApiError> for ToolError {
    fn from(err: ApiError) -> Self {
        Self(describe_api_error(&err))
    }
}

/// Текст ошибки исполнителя запросов для агента.
///
/// Тексты `ApiError` написаны для логов на русском, поэтому здесь своя формулировка на
/// английском; подробности от Wiki (`error_code`, `debug_message`, `details`, тело ответа)
/// добавляются как есть.
pub fn describe_api_error(err: &ApiError) -> String {
    match err {
        ApiError::Http(http) => describe_http_error(http),
        ApiError::MissingPathParam(name) => format!("missing path argument `{name}`"),
        ApiError::InvalidPath(_) => "an identifier argument does not form a valid API path: \
             empty values and values like `.` or `..` are rejected"
            .to_owned(),
        ApiError::InvalidRequest(detail) => {
            // Подробности — на русском, для лога; агенту — общий текст.
            tracing::debug!(%detail, "некорректный запрос к Wiki API");
            "invalid request: a local file argument (`file` or `save_to`) cannot be used; \
             check the path and that the target file does not exist yet"
                .to_owned()
        }
        ApiError::InvalidConfig(detail) => {
            // Подробности — для лога: они на русском и относятся к настройке сервера.
            tracing::warn!(%detail, "некорректная конфигурация клиента Wiki API");
            "the server is misconfigured (invalid token, organization or API address); ask \
             the user to check the server log and the profile settings"
                .to_owned()
        }
        ApiError::MissingToken => "no Wiki token is available for this request".to_owned(),
        ApiError::MissingOrg => "no Wiki organization is configured for this server; ask the \
             user to set it with `wiki-mcp auth login --org-id <ID>` (Yandex 360) or \
             `--cloud-org-id <ID>` (Yandex Cloud)"
            .to_owned(),
        ApiError::ResponseTooLarge { limit } => format!(
            "the Wiki API response is larger than {limit} bytes and was not returned. Narrow \
             the request: ask only for the blocks you need (`fields`), use a smaller page \
             (`page_size`) or stricter filters"
        ),
        ApiError::FileBodySize { path, size, max } => format!(
            "local file `{path}` has {size} bytes, but a request body from a file must have \
             1 to {max} bytes, so nothing was sent. An upload part is at most {} MiB, and \
             every part except the last one is at least {} MiB: split larger files into \
             parts and number them from 1",
            MAX_FILE_BODY_BYTES / (1024 * 1024),
            MIN_UPLOAD_PART_BYTES / (1024 * 1024),
        ),
        // Подробности транспорта и ввода-вывода исполнитель пишет по-русски: агенту — общий
        // текст на английском, подробности — в лог.
        ApiError::Transport(detail) => {
            tracing::warn!(%detail, "ошибка соединения с Wiki API");
            "could not reach the Wiki API (network error, TLS error or timeout); retry later. \
             If it keeps failing, ask the user to check the network and the server log"
                .to_owned()
        }
        ApiError::Io(detail) => {
            tracing::warn!(%detail, "ошибка локального файла");
            "local file error: the file could not be opened, read or written; check the path \
             and the permissions"
                .to_owned()
        }
        ApiError::LocalFileChanged(path) => format!(
            "local file `{path}` was replaced after its path was checked (for example, it or a \
             parent directory became a symbolic link), so it was not used and nothing was \
             written to it. Use a regular file inside the allowed directories"
        ),
    }
}

/// Текст ответа Wiki со статусом не 2xx: статус, подсказка, код и пояснение Wiki.
fn describe_http_error(err: &HttpError) -> String {
    let status = err.status;
    let hint = match status {
        300..=399 => {
            " (a redirect outside the Wiki API was not followed; the resource is probably \
             served from another host)"
        }
        400 | 422 => " (the request was rejected: check the arguments against the tool schema)",
        // Как заменить токен, зависит от режима сервера (хранилище или заголовок
        // HTTP-запроса), поэтому подробности — в инструкциях сервера.
        401 => {
            " (the token was rejected: it may be expired or revoked; ask the user to replace it \
             as described in the server instructions)"
        }
        403 => {
            " (the token's user has no access to this page or operation, or the organization \
             ID does not match the token)"
        }
        404 => {
            " (not found: check the page slug (a path like `users/name/page`, not a URL) or \
             the numeric `idx`, and that the token's user can see the page)"
        }
        409 => {
            " (conflict with the current state of the object; re-read it and retry with its \
             current revision)"
        }
        429 => " (rate limited: retry later)",
        500..=599 => " (Wiki server error: retry later)",
        _ => "",
    };
    let mut text = format!("Wiki API returned HTTP {status}{hint}.");
    let mut detailed = false;
    if let Some(code) = err
        .error_code
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
    {
        text.push_str(&format!(" Error code: {code}."));
        detailed = true;
    }
    if let Some(message) = err
        .debug_message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        text.push_str(&format!(" Debug message: {message}"));
        detailed = true;
    }
    // `message` — пояснение для пользователя на его языке (например, у `VALIDATION_ERROR`
    // — какое поле и почему не прошло проверку).
    if let Some(message) = err
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        text.push_str(&format!(" Message: {message}"));
        detailed = true;
    }
    if let Some(details) = err.details.as_ref().filter(|d| !is_empty_value(d)) {
        text.push_str(&format!(" Details: {details}"));
        detailed = true;
    }
    if err.error_code.as_deref() == Some("REDIRECT_CHAIN") {
        // Wiki отвечает так на страницу с перенаправлением при `raise_on_redirect=true`:
        // исходная и целевая страницы — в `details.source_page` и `details.target_page`.
        text.push_str(
            " The page redirects to another page (`details.target_page`): read the target \
             page, or repeat the call without `raise_on_redirect`.",
        );
    }
    if !detailed && !err.body.trim().is_empty() {
        text.push_str(" Response body: ");
        text.push_str(err.body.trim());
    }
    if let Some(secs) = err.retry_after {
        text.push_str(&format!(" Retry after {secs} s."));
    }
    text
}

fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::String(text) => text.trim().is_empty(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn http(
        status: u16,
        code: Option<&str>,
        debug_message: Option<&str>,
        details: Option<Value>,
        body: &str,
    ) -> ApiError {
        ApiError::Http(Box::new(HttpError {
            status,
            error_code: code.map(ToOwned::to_owned),
            debug_message: debug_message.map(ToOwned::to_owned),
            message: None,
            details,
            body: body.to_owned(),
            retry_after: None,
        }))
    }

    #[test]
    fn http_errors_have_status_hint_code_and_details() {
        let text = describe_api_error(&http(
            404,
            Some("NOT_FOUND"),
            Some("Страница не найдена"),
            None,
            r#"{"error_code":"NOT_FOUND","debug_message":"Страница не найдена"}"#,
        ));
        assert!(text.contains("HTTP 404"), "{text}");
        assert!(text.contains("`users/name/page`"), "{text}");
        assert!(text.contains("Error code: NOT_FOUND."), "{text}");
        assert!(
            text.contains("Debug message: Страница не найдена"),
            "{text}"
        );
        assert!(!text.contains("Response body"), "{text}");

        let text = describe_api_error(&http(
            400,
            Some("VALIDATION_ERROR"),
            None,
            Some(json!({"title": ["required"]})),
            "{}",
        ));
        assert!(
            text.contains(r#"Details: {"title":["required"]}"#),
            "{text}"
        );

        // Пояснение для пользователя (`message`) выводится рядом с кодом.
        let validation = ApiError::Http(Box::new(HttpError {
            status: 400,
            error_code: Some("VALIDATION_ERROR".into()),
            debug_message: Some("Validation failed".into()),
            message: Some("url: Строка не соответствует шаблону".into()),
            details: None,
            body: "{}".into(),
            retry_after: None,
        }));
        let text = describe_api_error(&validation);
        assert!(
            text.contains(
                "Error code: VALIDATION_ERROR. Debug message: Validation failed \
                 Message: url: Строка не соответствует шаблону"
            ),
            "{text}"
        );

        let text = describe_api_error(&http(
            400,
            Some("REDIRECT_CHAIN"),
            None,
            Some(json!({"source_page": {"id": 1}, "target_page": {"id": 2}})),
            "{}",
        ));
        assert!(text.contains("target_page"), "{text}");
        assert!(text.contains("raise_on_redirect"), "{text}");

        let text = describe_api_error(&http(401, None, None, Some(json!({})), " "));
        assert!(text.contains("server instructions"), "{text}");
        assert!(!text.contains("Details"), "{text}");
        assert!(!text.contains("Response body"), "{text}");

        let text = describe_api_error(&http(502, None, None, None, "Bad Gateway"));
        assert!(text.contains("retry later"), "{text}");
        assert!(text.contains("Response body: Bad Gateway"), "{text}");

        let mut limited = HttpError {
            status: 429,
            error_code: None,
            debug_message: None,
            message: None,
            details: None,
            body: String::new(),
            retry_after: Some(40),
        };
        let text = describe_api_error(&ApiError::Http(Box::new(limited.clone())));
        assert!(text.contains("Retry after 40 s"), "{text}");
        limited.retry_after = None;
        assert!(!describe_api_error(&ApiError::Http(Box::new(limited))).contains("Retry after"));
    }

    #[test]
    fn other_errors_are_english() {
        assert_eq!(
            describe_api_error(&ApiError::MissingPathParam("idx".into())),
            "missing path argument `idx`"
        );
        assert!(
            describe_api_error(&ApiError::InvalidPath("детали".into())).contains("valid API path")
        );
        let text = describe_api_error(&ApiError::InvalidConfig("пустой токен".into()));
        assert!(!text.contains("пустой"), "{text}");
        let text = describe_api_error(&ApiError::InvalidRequest("файл уже есть".into()));
        assert!(!text.contains("файл"), "{text}");
        assert!(describe_api_error(&ApiError::MissingOrg).contains("--org-id"));
        let text = describe_api_error(&ApiError::ResponseTooLarge { limit: 1024 });
        assert!(
            text.contains("1024 bytes") && text.contains("`page_size`"),
            "{text}"
        );
        let text = describe_api_error(&ApiError::FileBodySize {
            path: "/tmp/big.bin".into(),
            size: 20 * 1024 * 1024,
            max: MAX_FILE_BODY_BYTES,
        });
        assert!(text.contains("`/tmp/big.bin` has 20971520 bytes"), "{text}");
        assert!(
            text.contains("at most 16 MiB") && text.contains("5 MiB"),
            "{text}"
        );
        assert!(text.is_ascii(), "{text}");
        let text = describe_api_error(&ApiError::Transport("превышен таймаут".into()));
        assert!(text.contains("could not reach the Wiki API"), "{text}");
        assert!(text.is_ascii(), "{text}");
        let text = describe_api_error(&ApiError::Io("не удалось открыть файл".into()));
        assert!(text.starts_with("local file error"), "{text}");
        assert!(text.is_ascii(), "{text}");
        let text = describe_api_error(&ApiError::LocalFileChanged("/tmp/a".into()));
        assert!(text.contains("`/tmp/a` was replaced"), "{text}");
    }
}
