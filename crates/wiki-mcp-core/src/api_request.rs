//! Инструмент `api_request`: запрос к произвольному пути Wiki API.
//!
//! Это запасной выход для операций, которых нет среди сгенерированных инструментов или
//! которые отключены наборами (`--toolsets` его не ограничивают). Границы безопасности
//! те же, что у остальных инструментов:
//!
//! - путь только относительный, `/v1/…`, и проходит `validate_relative_path`; query
//!   передаётся отдельно, заголовки задать нельзя, поэтому `Authorization`, `X-Org-Id`
//!   и `X-Cloud-Org-Id` не переопределяются, а запрос не уходит на другой хост;
//! - требуемый уровень доступа определяется сопоставлением с операциями ([`crate::routes`]),
//!   а без совпадения — по методу; выше `max_access` запрос отклоняется до отправки;
//! - тело — только JSON: операции с сырым телом из файла (загрузка части файла) вызываются
//!   своим инструментом;
//! - `save_to` принимается только для операций с двоичным ответом (скачивание вложений), для
//!   них он обязателен и проходит [`FilePolicy`]. JSON-ответ в файл не сохраняется: там
//!   вырезание учётных данных не сработало бы.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use wiki_api::executor::{ApiRequest, RequestBody, validate_relative_path};
use wiki_api::spec::{AccessLevel, BodyKind, HttpMethod, OperationDef, ResponseKind};

use crate::error::ToolError;
use crate::files::{FilePolicy, LOCAL_FILES_DISABLED};
use crate::registry::{access_flag, access_name};
use crate::request::{SAVE_TO_ARG, save_to_path};
use crate::routes::{AccessDecision, default_access, required_access};

/// Имя инструмента.
pub const API_REQUEST_TOOL: &str = "api_request";

const ARGUMENTS: &[&str] = &["method", "path", "query", "body", SAVE_TO_ARG];

const METHODS: [HttpMethod; 5] = [
    HttpMethod::Get,
    HttpMethod::Post,
    HttpMethod::Put,
    HttpMethod::Patch,
    HttpMethod::Delete,
];

/// Подготовленный запрос и самая точная совпавшая операция.
#[derive(Debug)]
pub struct PreparedRequest {
    pub request: ApiRequest,
    pub operation: Option<&'static OperationDef>,
    /// Уровень доступа, который нужен запросу.
    pub access: AccessLevel,
}

/// Описание инструмента; аннотации соответствуют максимальному уровню доступа сервера.
/// Без локальных файлов (`local_files == false`, HTTP-режим) аргумента `save_to` в схеме нет.
pub fn api_request_tool(max_access: AccessLevel, local_files: bool) -> Tool {
    let mut properties = json!({
            "method": {
                "type": "string",
                "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
                "description": "HTTP method."
            },
            "path": {
                "type": "string",
                "description": "API path with the version, starting with `/v1/`, e.g. \
                    `/v1/pages/42/comments`. Full URLs, `?` and `#` are rejected: pass query \
                    parameters (such as `slug`) in `query`. Percent-encode special characters \
                    inside a segment."
            },
            "query": {
                "type": "object",
                "description": "Query parameters. Values may be strings, numbers, booleans or \
                    arrays of them; an array repeats the key (`key=a&key=b`).",
                "additionalProperties": {
                    "anyOf": [
                        {"type": ["string", "number", "boolean"]},
                        {"type": "array", "items": {"type": ["string", "number", "boolean"]}}
                    ]
                }
            },
            "body": {
                "description": "JSON request body (any JSON value), sent as application/json. \
                    Not allowed with GET."
            }
    });
    if local_files && let Value::Object(map) = &mut properties {
        map.insert(
            SAVE_TO_ARG.to_owned(),
            json!({
                "type": "string",
                "description": "Required for file downloads (operations that return a file, \
                    such as attachment content), not allowed otherwise: path of a new local \
                    file to save the content to (absolute, or relative to the server's working \
                    directory) inside the directories allowed by the server. Existing files are \
                    never overwritten."
            }),
        );
    }
    let schema = json!({
        "type": "object",
        "properties": properties,
        "required": ["method", "path"],
        "additionalProperties": false
    });
    let schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };

    let allowed = match max_access {
        AccessLevel::Read => {
            "This server is read-only, so only requests that read data are allowed (GET and \
             read-only operations such as POST /v1/search); others are refused with the flag \
             the user would need."
        }
        AccessLevel::Write => {
            "This server allows reading and changing data; deleting and changing access rights \
             need --allow-destructive."
        }
        AccessLevel::Destructive => {
            "This server allows reading, changing and deleting data; confirm destructive \
             requests with the user."
        }
    };
    let description = format!(
        "Send a request to any Yandex Wiki Public API path. Use it only when no dedicated tool \
         covers the operation (or its toolset is not enabled): dedicated tools validate \
         arguments and document the schema. Use `api_search` first to find the operation, its \
         dedicated tool and its reference page.\n\n\
         The path includes the API version (`/v1/...`). Headers cannot be set: the token and \
         the organization are added by the server. The body is JSON only: upload file parts \
         with the dedicated tool.\n\n\
         Access: a request that matches a known API operation needs that operation's access \
         level (for example, POST /v1/search is read-only and changing page access rights is \
         destructive); otherwise GET is read, POST/PUT/PATCH are write and DELETE is \
         destructive. {allowed} Toolsets do not restrict this tool."
    );

    let base = ToolAnnotations::with_title("Custom Wiki API request").open_world(true);
    let annotations = match max_access {
        AccessLevel::Read => base.read_only(true),
        AccessLevel::Write => base.read_only(false).destructive(false),
        AccessLevel::Destructive => base.read_only(false).destructive(true),
    };
    let mut tool = Tool::new(API_REQUEST_TOOL, description, Arc::new(schema));
    tool.title = Some("Custom Wiki API request".to_owned());
    tool.annotations = Some(annotations);
    tool
}

/// Проверяет аргументы, уровень доступа и файлы и собирает запрос.
pub fn prepare(
    args: &JsonObject,
    max_access: AccessLevel,
    files: &FilePolicy,
) -> Result<PreparedRequest, ToolError> {
    let present = |name: &str| args.get(name).is_some_and(|value| !value.is_null());
    reject_unknown(args)?;
    if files.is_disabled() && present(SAVE_TO_ARG) {
        return Err(ToolError::new(format!(
            "argument `{SAVE_TO_ARG}`: {LOCAL_FILES_DISABLED}"
        )));
    }

    let method = parse_method(args.get("method"))?;
    let path = parse_path(args.get("path"))?;

    // Уровень доступа проверяется раньше всего остального: в отказе не раскрываются
    // подробности о локальных файлах.
    let decision = required_access(method, &path);
    check_access(method, &path, &decision, max_access)?;
    check_bodies(method, &path, &decision, present(SAVE_TO_ARG), files)?;

    if method == HttpMethod::Get && present("body") {
        return Err(ToolError::new(
            "GET requests cannot have a body; pass parameters in `query`",
        ));
    }
    let query = parse_query(args.get("query"))?;
    let body = match args.get("body").filter(|value| !value.is_null()) {
        Some(value) => RequestBody::Json(value.clone()),
        None => RequestBody::Empty,
    };

    let save_to = match args.get(SAVE_TO_ARG).filter(|value| !value.is_null()) {
        Some(value) => Some(save_to_path(value, files)?),
        None => None,
    };

    Ok(PreparedRequest {
        operation: decision.operation(),
        access: decision.required,
        request: ApiRequest {
            method,
            path,
            query,
            body,
            save_to,
        },
    })
}

/// Файлы и `api_request`: тело — только JSON (сырое тело из файла — у своего инструмента),
/// двоичный ответ сохраняется только в `save_to`, а `save_to` бывает только у двоичного ответа.
fn check_bodies(
    method: HttpMethod,
    path: &str,
    decision: &AccessDecision,
    save_to: bool,
    files: &FilePolicy,
) -> Result<(), ToolError> {
    if let Some(op) = decision
        .matches
        .iter()
        .map(|m| m.op)
        .find(|op| op.body == Some(BodyKind::Binary))
    {
        return Err(ToolError::new(format!(
            "{} {path} sends the content of a local file, which `api_request` cannot do; use \
             the `{}` tool",
            method.as_str(),
            op.tool_name
        )));
    }
    let download = decision
        .matches
        .iter()
        .map(|m| m.op)
        .find(|op| op.response == ResponseKind::Binary);
    match download {
        Some(op) if files.is_disabled() => Err(ToolError::new(format!(
            "{} {path} returns a file (`{}`), and {LOCAL_FILES_DISABLED}. Ask the user to \
             download the file in Wiki",
            method.as_str(),
            op.tool_name
        ))),
        Some(op) if !save_to => Err(ToolError::new(format!(
            "{} {path} returns a file (`{}`): pass `{SAVE_TO_ARG}` with a path of a new local \
             file to save it to",
            method.as_str(),
            op.tool_name
        ))),
        None if save_to => Err(ToolError::new(format!(
            "`{SAVE_TO_ARG}` is only for file downloads, and {} {path} matches no API operation \
             that returns a file. Call it without `{SAVE_TO_ARG}`: the JSON response is \
             returned in the result",
            method.as_str()
        ))),
        _ => Ok(()),
    }
}

/// Уровень доступа запроса против уровня сервера.
fn check_access(
    method: HttpMethod,
    path: &str,
    decision: &AccessDecision,
    max_access: AccessLevel,
) -> Result<(), ToolError> {
    if max_access.allows(decision.required) {
        return Ok(());
    }
    let reason = match decision.strictest() {
        Some(op) => format!(
            "it matches the `{}` operation ({})",
            op.tool_name,
            op.summary.trim()
        ),
        None => format!(
            "{} requests to paths that match no known operation default to {} access",
            method.as_str(),
            access_name(default_access(method))
        ),
    };
    Err(ToolError::new(format!(
        "{} {path} needs {} access ({reason}), but the server runs with {} access. Ask the user \
         to restart the server with {} if this change is intended",
        method.as_str(),
        access_name(decision.required),
        access_name(max_access),
        access_flag(decision.required),
    )))
}

fn reject_unknown(args: &JsonObject) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !ARGUMENTS.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let allowed: Vec<String> = ARGUMENTS.iter().map(|name| format!("`{name}`")).collect();
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: {} (headers cannot be set: the token and \
         the organization are added by the server)",
        unknown.join(", "),
        allowed.join(", ")
    )))
}

fn parse_method(value: Option<&Value>) -> Result<HttpMethod, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Err(ToolError::new("missing required argument(s): `method`"));
    };
    let method = string_arg("method", value)?.trim();
    METHODS
        .into_iter()
        .find(|candidate| candidate.as_str().eq_ignore_ascii_case(method))
        .ok_or_else(|| {
            ToolError::new(format!(
                "argument `method` must be one of GET, POST, PUT, PATCH, DELETE; got `{method}`"
            ))
        })
}

fn parse_path(value: Option<&Value>) -> Result<String, ToolError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Err(ToolError::new("missing required argument(s): `path`"));
    };
    let path = string_arg("path", value)?.trim();
    if path.contains("://") || path.starts_with("//") {
        return Err(ToolError::new(
            "argument `path` must be a path of the Wiki API, e.g. `/v1/pages/42`; full URLs \
             are not accepted (to read a page by its URL, pass its slug to `get_page`)",
        ));
    }
    if path.contains(['?', '#']) {
        return Err(ToolError::new(
            "argument `path` must not contain `?` or `#`; pass query parameters in `query`",
        ));
    }
    let versioned = wiki_api::path::API_VERSIONS.iter().any(|version| {
        path.strip_prefix('/')
            .and_then(|rest| rest.strip_prefix(version))
            .is_some_and(|rest| rest.starts_with('/'))
    });
    if !versioned {
        return Err(ToolError::new(
            "argument `path` must start with the API version `/v1/`, e.g. `/v1/pages/42`",
        ));
    }
    validate_relative_path(path).map_err(|_| {
        ToolError::new(
            "argument `path` is not a valid API path: name a resource after the version, use \
             only URL path characters (percent-encode others as %XX), and no empty, `.` or `..` \
             segments",
        )
    })?;
    Ok(path.to_owned())
}

fn parse_query(value: Option<&Value>) -> Result<Vec<(String, String)>, ToolError> {
    let map = match value {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(ToolError::new(
                "argument `query` must be an object of parameter names to values",
            ));
        }
    };
    let mut query = Vec::new();
    for (key, value) in map {
        if key.trim().is_empty() {
            return Err(ToolError::new(
                "argument `query` has an empty parameter name",
            ));
        }
        match value {
            Value::Null => {}
            Value::Array(items) => {
                for item in items {
                    query.push((key.clone(), query_scalar(key, item)?));
                }
            }
            other => query.push((key.clone(), query_scalar(key, other)?)),
        }
    }
    Ok(query)
}

fn query_scalar(key: &str, value: &Value) -> Result<String, ToolError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(ToolError::new(format!(
            "query parameter `{key}` must be a string, number, boolean or an array of them"
        ))),
    }
}

fn string_arg<'a>(name: &str, value: &'a Value) -> Result<&'a str, ToolError> {
    value
        .as_str()
        .ok_or_else(|| ToolError::new(format!("argument `{name}` must be a string")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            other => panic!("не объект: {other}"),
        }
    }

    /// Политика без разрешённых каталогов: любой локальный путь отклоняется.
    fn policy() -> FilePolicy {
        FilePolicy::new(&[]).unwrap()
    }

    fn prep(value: Value, max_access: AccessLevel) -> Result<PreparedRequest, ToolError> {
        prepare(&args(value), max_access, &policy())
    }

    fn err(value: Value, max_access: AccessLevel) -> String {
        prep(value, max_access).unwrap_err().message().to_owned()
    }

    #[test]
    fn get_with_query() {
        let prepared = prep(
            json!({
                "method": "get",
                "path": "/v1/pages/descendants",
                "query": {"slug": "users/name", "page_size": 5, "include_self": true, "cursor": null}
            }),
            AccessLevel::Read,
        )
        .unwrap();
        assert_eq!(prepared.access, AccessLevel::Read);
        assert_eq!(
            prepared.operation.map(|op| op.tool_name),
            Some("list_page_descendants")
        );
        let request = prepared.request;
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/v1/pages/descendants");
        let mut query = request.query.clone();
        query.sort();
        assert_eq!(
            query,
            vec![
                ("include_self".into(), "true".into()),
                ("page_size".into(), "5".into()),
                ("slug".into(), "users/name".into()),
            ]
        );
        assert!(matches!(request.body, RequestBody::Empty));
        assert!(request.save_to.is_none());

        let prepared = prep(
            json!({"method": "GET", "path": "/v1/pages", "query": {"ids": ["a", 2]}}),
            AccessLevel::Read,
        )
        .unwrap();
        assert_eq!(
            prepared.request.query,
            vec![("ids".into(), "a".into()), ("ids".into(), "2".into())]
        );
    }

    #[test]
    fn unknown_path_has_no_operation() {
        let prepared = prep(
            json!({"method": "GET", "path": "/v1/some/new/endpoint"}),
            AccessLevel::Read,
        )
        .unwrap();
        assert!(prepared.operation.is_none());
        assert_eq!(prepared.access, AccessLevel::Read);
    }

    #[test]
    fn validation_errors() {
        let read = AccessLevel::Read;
        assert!(err(json!({"path": "/v1/users/me"}), read).contains("`method`"));
        assert!(err(json!({"method": "GET"}), read).contains("`path`"));
        assert!(
            err(json!({"method": "HEAD", "path": "/v1/users/me"}), read).contains("must be one of")
        );
        assert!(
            err(json!({"method": 1, "path": "/v1/users/me"}), read).contains("must be a string")
        );
        for url in [
            "https://evil.example/v1/users/me",
            "//evil.example/v1/users/me",
            "/v1/http://evil.example",
            "https://wiki.yandex.ru/users/name/page",
        ] {
            assert!(
                err(json!({"method": "GET", "path": url}), read).contains("full URLs"),
                "{url}"
            );
        }
        assert!(
            err(json!({"method": "GET", "path": "/v1/pages?slug=x"}), read).contains("`query`")
        );
        assert!(err(json!({"method": "GET", "path": "/v1/pages#x"}), read).contains("`query`"));
        for path in [
            "v1/pages",
            "/pages",
            "/v2/pages",
            "/v1",
            "/v1x/pages",
            "users/name/page",
        ] {
            assert!(
                err(json!({"method": "GET", "path": path}), read).contains("API version"),
                "{path}"
            );
        }
        for path in [
            "/v1/pages/../users/me",
            "/v1/pages/%2e%2e/x",
            "/v1/a b",
            "/v1/",
            "/v1//pages",
        ] {
            assert!(
                err(json!({"method": "GET", "path": path}), read).contains("valid API path"),
                "{path}"
            );
        }

        let unknown = err(
            json!({"method": "GET", "path": "/v1/users/me", "headers": {"Authorization": "x"}}),
            read,
        );
        assert!(
            unknown.contains("unknown argument(s) `headers`"),
            "{unknown}"
        );
        assert!(unknown.contains("headers cannot be set"), "{unknown}");
        let unknown = err(
            json!({"method": "GET", "path": "/v1/users/me", "X-Org-Id": "1"}),
            read,
        );
        assert!(unknown.contains("`X-Org-Id`"), "{unknown}");

        assert!(
            err(
                json!({"method": "GET", "path": "/v1/users/me", "query": "a=1"}),
                read
            )
            .contains("must be an object")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "/v1/users/me", "query": {"a": {"b": 1}}}),
                read
            )
            .contains("query parameter `a`")
        );
        assert!(
            err(
                json!({"method": "GET", "path": "/v1/users/me", "body": {}}),
                read
            )
            .contains("GET requests cannot have a body")
        );
    }

    #[test]
    fn read_only_server_refuses_writes_and_deletes() {
        let read = AccessLevel::Read;
        let message = err(
            json!({"method": "POST", "path": "/v1/pages", "body": {"slug": "x", "title": "T"}}),
            read,
        );
        assert!(message.contains("needs write access"), "{message}");
        assert!(message.contains("`create_page`"), "{message}");
        assert!(message.contains("read-only access"), "{message}");
        assert!(message.contains("--allow-write"), "{message}");

        let message = err(json!({"method": "DELETE", "path": "/v1/pages/42"}), read);
        assert!(message.contains("needs destructive access"), "{message}");
        assert!(message.contains("`delete_page`"), "{message}");
        assert!(message.contains("--allow-destructive"), "{message}");

        // Изменение прав доступа — уровень удаления даже с правом записи.
        let message = err(
            json!({"method": "POST", "path": "/v1/pages/42/access", "body": {}}),
            AccessLevel::Write,
        );
        assert!(message.contains("needs destructive access"), "{message}");
        assert!(message.contains("`create_page_access`"), "{message}");

        // Неизвестный путь — по методу.
        let message = err(json!({"method": "PATCH", "path": "/v1/unknown/1"}), read);
        assert!(message.contains("match no known operation"), "{message}");
        assert!(message.contains("--allow-write"), "{message}");
        let message = err(
            json!({"method": "DELETE", "path": "/v1/unknown/1"}),
            AccessLevel::Write,
        );
        assert!(message.contains("needs destructive access"), "{message}");

        // Поиск и проверка slug столбца через POST — чтение.
        for path in ["/v1/search", "/v1/grids/abc/columns/suggest"] {
            let prepared = prep(
                json!({"method": "POST", "path": path, "body": {"query": "x"}}),
                read,
            )
            .unwrap();
            assert_eq!(prepared.access, AccessLevel::Read, "{path}");
            assert!(matches!(prepared.request.body, RequestBody::Json(_)));
        }

        // С правами записи — можно, удаление — нет.
        assert!(
            prep(
                json!({"method": "POST", "path": "/v1/pages/42/comments", "body": {"body": "x"}}),
                AccessLevel::Write
            )
            .is_ok()
        );
        assert!(
            prep(
                json!({"method": "DELETE", "path": "/v1/pages/42/comments/5"}),
                AccessLevel::Destructive
            )
            .is_ok()
        );
    }

    #[test]
    fn file_operations_need_their_tools() {
        let message = err(
            json!({"method": "PUT", "path": "/v1/upload_sessions/s1/upload_part", "body": "x"}),
            AccessLevel::Write,
        );
        assert!(message.contains("`upload_file_part`"), "{message}");
        let message = err(
            json!({"method": "GET", "path": "/v1/pages/42/attachments/7/download"}),
            AccessLevel::Read,
        );
        assert!(message.contains("returns a file"), "{message}");
        assert!(message.contains("`download_attachment`"), "{message}");
        assert!(message.contains("pass `save_to`"), "{message}");
        // Уровень доступа проверяется раньше: отказ называет флаг.
        let message = err(
            json!({"method": "PUT", "path": "/v1/upload_sessions/s1/upload_part"}),
            AccessLevel::Read,
        );
        assert!(message.contains("--allow-write"), "{message}");
    }

    #[test]
    fn save_to_only_for_downloads_inside_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        let target = root.join("file.pdf");
        let prepared = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v1/pages/42/attachments/7/download",
                "save_to": target.display().to_string()
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap();
        assert_eq!(
            prepared.operation.map(|op| op.tool_name),
            Some("download_attachment")
        );
        assert_eq!(prepared.request.save_to, Some(target.clone()));

        // JSON-ответ в файл не сохраняется.
        let message = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v1/pages/42",
                "save_to": root.join("page.json").display().to_string()
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(
            message.message().contains("only for file downloads"),
            "{message}"
        );

        // Вне разрешённых каталогов и поверх существующего файла — отказ.
        let other = tempfile::tempdir().unwrap();
        let message = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v1/pages/42/attachments/7/download",
                "save_to": other.path().join("x.pdf").display().to_string()
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(
            message
                .message()
                .contains("outside the allowed directories"),
            "{message}"
        );
        std::fs::write(&target, b"old").unwrap();
        let message = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v1/pages/42/attachments/7/download",
                "save_to": target.display().to_string()
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(message.message().contains("already exists"), "{message}");
    }

    #[test]
    fn save_to_is_rejected_without_local_files() {
        let files = FilePolicy::disabled();
        let message = prepare(
            &args(json!({
                "method": "GET",
                "path": "/v1/pages/42/attachments/7/download",
                "save_to": "a.pdf"
            })),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(message.message().contains("HTTP mode"), "{message}");
        let message = prepare(
            &args(json!({"method": "GET", "path": "/v1/pages/42/attachments/7/download"})),
            AccessLevel::Read,
            &files,
        )
        .unwrap_err();
        assert!(message.message().contains("HTTP mode"), "{message}");
        let schema =
            serde_json::to_string(&api_request_tool(AccessLevel::Read, false).input_schema)
                .unwrap();
        assert!(!schema.contains(SAVE_TO_ARG), "{schema}");
        let schema =
            serde_json::to_string(&api_request_tool(AccessLevel::Read, true).input_schema).unwrap();
        assert!(schema.contains(SAVE_TO_ARG), "{schema}");
    }

    #[test]
    fn tool_annotations_follow_max_access() {
        let read = api_request_tool(AccessLevel::Read, true);
        let a = read.annotations.as_ref().unwrap();
        assert_eq!(a.read_only_hint, Some(true));
        assert_eq!(a.destructive_hint, None);
        let write = api_request_tool(AccessLevel::Write, true);
        let a = write.annotations.as_ref().unwrap();
        assert_eq!(
            (a.read_only_hint, a.destructive_hint),
            (Some(false), Some(false))
        );
        let all = api_request_tool(AccessLevel::Destructive, true);
        let a = all.annotations.as_ref().unwrap();
        assert_eq!(
            (a.read_only_hint, a.destructive_hint),
            (Some(false), Some(true))
        );
        let description = read.description.unwrap_or_default();
        assert!(description.contains("api_search"));
        assert!(description.contains("Toolsets do not restrict"));
        assert!(description.contains("read-only"));
        assert_eq!(read.input_schema["additionalProperties"], false);
    }
}
