//! Сборка запроса к API из аргументов сгенерированного инструмента.
//!
//! Порядок описан в документации `wiki_api::spec`: неизвестные аргументы отклоняются,
//! обязательные проверяются, значения приводятся по [`ParamType`]. Значение `null` считается
//! непереданным аргументом: некоторые клиенты так передают необязательные параметры.
//!
//! Тело запроса — аргумент `body` (JSON) или, у загрузки части файла, путь к локальному файлу
//! (параметр [`ParamType::File`]), содержимое которого исполнитель отправит сырым телом.
//!
//! Локальные файлы проходят [`FilePolicy`]: файл для загрузки и файл для сохранения двоичного
//! ответа (аргумент [`SAVE_TO_ARG`]) должны лежать внутри разрешённых каталогов.

use std::collections::HashMap;

use rmcp::model::JsonObject;
use serde_json::Value;
use wiki_api::executor::{ApiRequest, RequestBody, render_path};
use wiki_api::spec::{OperationDef, ParamDef, ParamLocation, ParamType, ResponseKind};

use crate::error::ToolError;
use crate::files::FilePolicy;

/// Аргумент инструментов с двоичным ответом: куда сохранить файл.
pub const SAVE_TO_ARG: &str = "save_to";

/// Принимает ли инструмент операции аргумент [`SAVE_TO_ARG`].
pub fn takes_save_to(op: &OperationDef) -> bool {
    op.response == ResponseKind::Binary
}

/// Собирает [`ApiRequest`] для операции `op` из аргументов инструмента.
pub fn build_request(
    op: &OperationDef,
    args: &JsonObject,
    files: &FilePolicy,
) -> Result<ApiRequest, ToolError> {
    let present = |name: &str| args.get(name).is_some_and(|value| !value.is_null());

    reject_unknown(op, args)?;
    let mut missing: Vec<String> = op
        .params
        .iter()
        .filter(|param| param.required && !present(param.name))
        .map(|param| format!("`{}`", param.name))
        .collect();
    if takes_save_to(op) && !present(SAVE_TO_ARG) {
        missing.push(format!("`{SAVE_TO_ARG}`"));
    }
    if !missing.is_empty() {
        return Err(ToolError::new(format!(
            "missing required argument(s): {}",
            missing.join(", ")
        )));
    }

    let mut path_values: HashMap<&str, String> = HashMap::new();
    let mut query = Vec::new();
    let mut body = RequestBody::Empty;

    for param in op.params {
        let Some(value) = args.get(param.name).filter(|value| !value.is_null()) else {
            continue;
        };
        match param.location {
            ParamLocation::Path => {
                path_values.insert(param.name, scalar(param, value)?);
            }
            ParamLocation::Query => {
                for item in query_values(param, value)? {
                    query.push((param.name.to_owned(), item));
                }
            }
            ParamLocation::Body if param.value_type == ParamType::File => {
                body = RequestBody::File(file_body(param, value, files)?);
            }
            ParamLocation::Body => body = RequestBody::Json(json_body(param, value)?),
        }
    }

    let path = render_path(op.path, |name| path_values.get(name).cloned())?;
    let save_to = match args.get(SAVE_TO_ARG).filter(|value| !value.is_null()) {
        Some(value) if takes_save_to(op) => Some(save_to_path(value, files)?),
        _ => None,
    };
    Ok(ApiRequest {
        method: op.method,
        path,
        query,
        body,
        save_to,
    })
}

/// Путь к файлу для сырого тела запроса: существующий обычный файл в разрешённых каталогах.
fn file_body(
    param: &ParamDef,
    value: &Value,
    files: &FilePolicy,
) -> Result<std::path::PathBuf, ToolError> {
    let Value::String(path) = value else {
        return Err(ToolError::new(format!(
            "argument `{}` must be a path to a local file, got {}",
            param.name,
            kind(value)
        )));
    };
    files
        .check_read(path)
        .map_err(|err| ToolError::new(format!("argument `{}`: {err}", param.name)))
}

/// Путь для сохранения двоичного ответа ([`SAVE_TO_ARG`]): новый файл в разрешённых каталогах.
pub fn save_to_path(value: &Value, files: &FilePolicy) -> Result<std::path::PathBuf, ToolError> {
    let Value::String(target) = value else {
        return Err(ToolError::new(format!(
            "argument `{SAVE_TO_ARG}` must be a path to a new local file, got {}",
            kind(value)
        )));
    };
    files
        .check_write(target)
        .map_err(|err| ToolError::new(format!("argument `{SAVE_TO_ARG}`: {err}")))
}

fn reject_unknown(op: &OperationDef, args: &JsonObject) -> Result<(), ToolError> {
    let allowed = |key: &str| {
        op.params.iter().any(|param| param.name == key) || (takes_save_to(op) && key == SAVE_TO_ARG)
    };
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !allowed(key))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let mut names: Vec<String> = op
        .params
        .iter()
        .map(|param| format!("`{}`", param.name))
        .collect();
    if takes_save_to(op) {
        names.push(format!("`{SAVE_TO_ARG}`"));
    }
    let names = if names.is_empty() {
        "none".to_owned()
    } else {
        names.join(", ")
    };
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: {names}",
        unknown.join(", ")
    )))
}

/// Значение path- или query-параметра строкой.
fn scalar(param: &ParamDef, value: &Value) -> Result<String, ToolError> {
    let invalid = |expected: &str| {
        ToolError::new(format!(
            "argument `{}` must be {expected}, got {}",
            param.name,
            kind(value)
        ))
    };
    match param.value_type {
        ParamType::Integer => match value {
            Value::Number(n) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
            Value::String(s) if is_integer(s.trim()) => Ok(s.trim().to_owned()),
            _ => Err(invalid("an integer (a number or a string of digits)")),
        },
        ParamType::Number => match value {
            Value::Number(n) => Ok(n.to_string()),
            Value::String(s) if s.trim().parse::<f64>().is_ok_and(f64::is_finite) => {
                Ok(s.trim().to_owned())
            }
            _ => Err(invalid("a number")),
        },
        ParamType::Boolean => match value {
            Value::Bool(b) => Ok(b.to_string()),
            Value::String(s) if s.trim().eq_ignore_ascii_case("true") => Ok("true".to_owned()),
            Value::String(s) if s.trim().eq_ignore_ascii_case("false") => Ok("false".to_owned()),
            _ => Err(invalid("a boolean")),
        },
        // Строки, а также элементы массивов: числа и логические значения принимаем как их
        // текстовую запись — идентификаторы в Wiki бывают и числовыми.
        ParamType::String | ParamType::Array => match value {
            Value::String(s) => Ok(s.clone()),
            Value::Number(n) => Ok(n.to_string()),
            Value::Bool(b) => Ok(b.to_string()),
            _ => Err(invalid("a string")),
        },
        ParamType::Object | ParamType::File => Err(invalid("a string")),
    }
}

/// Значения query-параметра: массив передаётся повторением ключа.
fn query_values(param: &ParamDef, value: &Value) -> Result<Vec<String>, ToolError> {
    match (param.value_type, value) {
        (ParamType::Array, Value::Array(items)) => {
            items.iter().map(|item| scalar(param, item)).collect()
        }
        _ => scalar(param, value).map(|item| vec![item]),
    }
}

/// JSON-тело. Строка с JSON допускается: исполнитель разберёт её и отправит объектом
/// (некоторые клиенты передают тело строкой).
fn json_body(param: &ParamDef, value: &Value) -> Result<Value, ToolError> {
    let parsed = match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        other => Some(other.clone()),
    };
    let ok = match (param.value_type, &parsed) {
        (ParamType::Object, Some(Value::Object(_))) => true,
        (ParamType::Array, Some(Value::Array(_))) => true,
        (ParamType::Object | ParamType::Array, _) => false,
        _ => parsed.is_some(),
    };
    if !ok {
        let expected = match param.value_type {
            ParamType::Array => "a JSON array",
            _ => "a JSON object",
        };
        return Err(ToolError::new(format!(
            "argument `{}` must be {expected} (the request body), got {}",
            param.name,
            kind(value)
        )));
    }
    Ok(value.clone())
}

fn is_integer(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiki_api::HttpMethod;
    use wiki_api::spec::{find, operations};

    use super::*;

    fn op(name: &str) -> &'static OperationDef {
        find(name).unwrap_or_else(|| panic!("нет операции {name}"))
    }

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

    fn build(name: &str, value: Value) -> Result<ApiRequest, ToolError> {
        build_request(op(name), &args(value), &policy())
    }

    /// Первая операция с параметром в `location` типа `value_type`.
    fn op_with(
        location: ParamLocation,
        value_type: ParamType,
    ) -> (&'static OperationDef, &'static ParamDef) {
        operations()
            .iter()
            .find_map(|op| {
                op.params
                    .iter()
                    .find(|p| p.location == location && p.value_type == value_type)
                    .map(|p| (op, p))
            })
            .unwrap_or_else(|| panic!("нет параметра {location:?} {value_type:?}"))
    }

    #[test]
    fn slug_goes_to_query_and_idx_to_path() {
        let request = build(
            "get_page",
            json!({"slug": "users/name/page", "fields": "content", "revision_id": null}),
        )
        .unwrap();
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/v1/pages");
        assert_eq!(
            request.query,
            vec![
                ("slug".into(), "users/name/page".into()),
                ("fields".into(), "content".into())
            ]
        );
        assert!(matches!(request.body, RequestBody::Empty));
        assert!(request.save_to.is_none());

        let request = build("get_page_by_id", json!({"idx": 42})).unwrap();
        assert_eq!(request.path, "/v1/pages/42");
        let request = build("get_page_by_id", json!({"idx": "42"})).unwrap();
        assert_eq!(request.path, "/v1/pages/42");
    }

    #[test]
    fn path_values_are_checked() {
        let err = build("get_page_by_id", json!({"idx": "users/name"})).unwrap_err();
        assert!(err.message().contains("must be an integer"), "{err}");
        let request = build("get_grid", json!({"idx": "a/b c"})).unwrap();
        assert_eq!(request.path, "/v1/grids/a%2Fb%20c");
        let err = build("get_grid", json!({"idx": ".."})).unwrap_err();
        assert!(err.message().contains("valid API path"), "{err}");
        let err = build("get_grid", json!({"idx": ""})).unwrap_err();
        assert!(err.message().contains("missing path argument"), "{err}");
        let err = build("get_grid", json!({"idx": {"a": 1}})).unwrap_err();
        assert!(
            err.message().contains("must be a string, got an object"),
            "{err}"
        );
    }

    #[test]
    fn missing_and_unknown_arguments() {
        let err = build("get_page_by_id", json!({})).unwrap_err();
        assert_eq!(err.message(), "missing required argument(s): `idx`");
        // `null` — то же, что непереданный аргумент.
        let err = build("get_page_by_id", json!({"idx": null})).unwrap_err();
        assert!(err.message().contains("`idx`"), "{err}");

        let err = build(
            "get_page_by_id",
            json!({"idx": 1, "page_id": "x", "token": "t"}),
        )
        .unwrap_err();
        assert!(
            err.message()
                .starts_with("unknown argument(s) `page_id`, `token`"),
            "{err}"
        );
        assert!(err.message().contains("`fields`"), "{err}");
    }

    #[test]
    fn scalar_types() {
        let (_, integer) = op_with(ParamLocation::Query, ParamType::Integer);
        assert_eq!(scalar(integer, &json!(5)).unwrap(), "5");
        assert_eq!(scalar(integer, &json!(" 7 ")).unwrap(), "7");
        for bad in [json!(1.5), json!("ten"), json!(true)] {
            let err = scalar(integer, &bad).unwrap_err();
            assert!(err.message().contains("must be an integer"), "{err}");
        }

        let (_, boolean) = op_with(ParamLocation::Query, ParamType::Boolean);
        assert_eq!(scalar(boolean, &json!(true)).unwrap(), "true");
        assert_eq!(scalar(boolean, &json!("FALSE")).unwrap(), "false");
        assert!(scalar(boolean, &json!(1)).is_err());

        let (_, string) = op_with(ParamLocation::Query, ParamType::String);
        assert_eq!(scalar(string, &json!(42)).unwrap(), "42");
    }

    #[test]
    fn json_body() {
        let request = build(
            "create_page",
            json!({"body": {"slug": "users/name/new", "title": "T", "page_type": "page"}}),
        )
        .unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.path, "/v1/pages");
        assert!(matches!(request.body, RequestBody::Json(ref v) if v["slug"] == "users/name/new"));
        // Тело строкой с JSON допустимо, исполнитель отправит его объектом.
        let request = build("create_page", json!({"body": "{\"title\": \"T\"}"})).unwrap();
        assert!(matches!(request.body, RequestBody::Json(Value::String(_))));
        for bad in [json!("not json"), json!([1]), json!(5)] {
            let err = build("create_page", json!({ "body": bad })).unwrap_err();
            assert!(err.message().contains("must be a JSON object"), "{err}");
        }
    }

    #[test]
    fn file_part_is_read_inside_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let part = root.join("part.bin");
        std::fs::write(&part, b"bytes").unwrap();
        let files = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        let upload = op("upload_file_part");

        let value =
            json!({"session_id": "s-1", "part_number": 1, "file": part.display().to_string()});
        let request = build_request(upload, &args(value), &files).unwrap();
        assert_eq!(request.method, HttpMethod::Put);
        assert_eq!(request.path, "/v1/upload_sessions/s-1/upload_part");
        assert_eq!(request.query, vec![("part_number".into(), "1".into())]);
        assert!(matches!(&request.body, RequestBody::File(path) if *path == part));
        assert!(request.save_to.is_none());

        // Вне разрешённых каталогов — отказ до запроса.
        let value =
            json!({"session_id": "s-1", "part_number": 1, "file": part.display().to_string()});
        let err = build_request(upload, &args(value), &policy()).unwrap_err();
        assert!(err.message().starts_with("argument `file`: "), "{err}");
        assert!(
            err.message().contains("outside the allowed directories"),
            "{err}"
        );
        let value = json!({"session_id": "s-1", "part_number": 1, "file": 5});
        let err = build_request(upload, &args(value), &files).unwrap_err();
        assert!(err.message().contains("must be a path"), "{err}");
        // HTTP-режим: локальные файлы выключены.
        let value =
            json!({"session_id": "s-1", "part_number": 1, "file": part.display().to_string()});
        let err = build_request(upload, &args(value), &FilePolicy::disabled()).unwrap_err();
        assert!(err.message().contains("HTTP mode"), "{err}");
    }

    #[test]
    fn binary_response_needs_save_to_inside_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root)).unwrap();
        let downloads: Vec<&str> = operations()
            .iter()
            .filter(|op| takes_save_to(op))
            .map(|op| op.tool_name)
            .collect();
        assert_eq!(
            downloads,
            [
                "download_attachment",
                "download_attachment_by_url",
                "download_attachment_preview"
            ]
        );

        let download = op("download_attachment");
        let err =
            build_request(download, &args(json!({"idx": 1, "file_id": 7})), &policy).unwrap_err();
        assert_eq!(err.message(), "missing required argument(s): `save_to`");

        let target = root.join("file.pdf");
        let value = json!({"idx": 1, "file_id": 7, "save_to": target.display().to_string()});
        let request = build_request(download, &args(value), &policy).unwrap();
        assert_eq!(request.save_to, Some(target.clone()));

        // Существующий файл не перезаписывается.
        std::fs::write(&target, b"old").unwrap();
        let value = json!({"idx": 1, "file_id": 7, "save_to": target.display().to_string()});
        let err = build_request(download, &args(value), &policy).unwrap_err();
        assert!(err.message().contains("argument `save_to`"), "{err}");
        assert!(err.message().contains("already exists"), "{err}");

        // `save_to` есть только у операций с двоичным ответом.
        let err = build("get_page_by_id", json!({"idx": 1, "save_to": "/tmp/x"})).unwrap_err();
        assert!(
            err.message().contains("unknown argument(s) `save_to`"),
            "{err}"
        );
    }

    #[test]
    fn no_operation_parameter_clashes_with_save_to() {
        for op in operations() {
            assert!(
                op.params.iter().all(|param| param.name != SAVE_TO_ARG),
                "{}",
                op.tool_name
            );
        }
    }

    #[test]
    fn search_is_a_post_with_body() {
        let request = build("search", json!({"body": {"query": "отчёт"}})).unwrap();
        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.path, "/v1/search");
        assert!(matches!(request.body, RequestBody::Json(_)));
    }
}
