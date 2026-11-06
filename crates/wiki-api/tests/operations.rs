//! Инварианты таблицы операций, сгенерированной `build.rs`, снапшоты списка инструментов
//! и характерных операций, проверки генератора на синтетических спецификациях.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use wiki_api::spec::{
    AccessLevel, BodyKind, HttpMethod, OperationDef, ParamLocation, ParamType, ResponseKind,
    default_toolsets, find, find_toolset, operations, toolsets,
};

/// Модули генератора подключаются как есть, чтобы проверить его ошибки сборки
/// на синтетических спецификациях.
#[allow(dead_code)]
#[path = "../build/generate.rs"]
mod generate;
#[allow(dead_code)]
#[path = "../build/naming.rs"]
mod naming;
#[allow(dead_code)]
#[path = "../build/schema.rs"]
mod schema;
// Модуль и функция `toolsets` живут в разных пространствах имён и не конфликтуют.
#[allow(dead_code)]
#[path = "../build/toolsets.rs"]
mod toolsets;

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];

fn spec() -> Value {
    serde_json::from_str(wiki_api::OPENAPI_JSON).expect("спецификация должна быть JSON")
}

/// Операции спецификации по `operationId`.
fn spec_operations(spec: &Value) -> BTreeMap<String, (String, &'static str, Value)> {
    let mut out = BTreeMap::new();
    for (path, item) in spec["paths"].as_object().expect("paths") {
        for method in METHODS {
            if let Some(op) = item.get(method) {
                let id = op["operationId"].as_str().expect("operationId").to_owned();
                out.insert(id, (path.clone(), method, op.clone()));
            }
        }
    }
    out
}

fn schema(op: &OperationDef) -> Value {
    serde_json::from_str(op.input_schema)
        .unwrap_or_else(|err| panic!("{}: input_schema не JSON: {err}", op.tool_name))
}

fn placeholders(path: &str) -> BTreeSet<&str> {
    path.split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect()
}

fn op(name: &str) -> &'static OperationDef {
    find(name).unwrap_or_else(|| panic!("нет операции {name}"))
}

#[test]
fn every_spec_operation_is_generated() {
    let spec = spec();
    let expected = spec_operations(&spec);
    assert!(
        (40..=150).contains(&expected.len()),
        "неожиданное число операций в спецификации: {}",
        expected.len()
    );
    assert_eq!(operations().len(), expected.len());
    let generated: BTreeSet<&str> = operations().iter().map(|op| op.tool_name).collect();
    let in_spec: BTreeSet<&str> = expected.keys().map(String::as_str).collect();
    assert_eq!(generated, in_spec);

    for op in operations() {
        let (path, method, raw) = &expected[op.tool_name];
        assert_eq!(op.path, path, "{}", op.tool_name);
        assert_eq!(
            op.method.as_str(),
            method.to_uppercase(),
            "{}",
            op.tool_name
        );
        assert_eq!(
            Some(op.access.as_str()),
            raw["x-access-level"].as_str(),
            "{}: уровень доступа должен совпадать с x-access-level",
            op.tool_name
        );
        assert_eq!(
            op.docs_url,
            raw["externalDocs"]["url"].as_str(),
            "{}",
            op.tool_name
        );
        if let Some(url) = op.docs_url {
            assert!(op.tool_description().contains(url), "{}", op.tool_name);
        }
        assert_eq!(
            op.upstream_operation_id,
            raw["x-upstream-operation-id"].as_str(),
            "{}",
            op.tool_name
        );
        assert!(!op.summary.is_empty(), "{}: нет summary", op.tool_name);
        assert_eq!(
            Some(op.toolset),
            raw["tags"][0].as_str(),
            "{}: набор не соответствует тегу",
            op.tool_name
        );
    }
}

#[test]
fn tool_names_are_unique_sorted_and_valid() {
    let names: Vec<&str> = operations().iter().map(|op| op.tool_name).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(names, sorted, "имена должны быть уникальны и отсортированы");
    for name in names {
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "недопустимое имя инструмента {name}"
        );
        assert_eq!(find(name).map(|op| op.tool_name), Some(name));
    }
    assert!(find("no_such_tool").is_none());
}

#[test]
fn toolsets_follow_spec_tags() {
    let spec = spec();
    let tags: Vec<(&str, bool)> = spec["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .map(|tag| {
            (
                tag["name"].as_str().expect("name"),
                tag["x-default-enabled"]
                    .as_bool()
                    .expect("x-default-enabled"),
            )
        })
        .collect();
    let generated: Vec<(&str, bool)> = toolsets()
        .iter()
        .map(|t| (t.slug, t.default_enabled))
        .collect();
    assert_eq!(generated, tags, "наборы должны повторять tags спецификации");

    for toolset in toolsets() {
        assert!(
            operations().iter().any(|op| op.toolset == toolset.slug),
            "пустой набор {}",
            toolset.slug
        );
        assert!(!toolset.description.is_empty(), "{}", toolset.slug);
    }
    for op in operations() {
        assert!(find_toolset(op.toolset).is_some(), "{}", op.tool_name);
    }
    let defaults: Vec<&str> = default_toolsets().map(|t| t.slug).collect();
    assert!(defaults.contains(&"pages"), "{defaults:?}");
    assert!(!defaults.contains(&"access"), "{defaults:?}");
}

#[test]
fn input_schemas_are_objects_with_all_params() {
    for op in operations() {
        let schema = schema(op);
        assert_eq!(schema["type"], "object", "{}", op.tool_name);
        assert_eq!(schema["additionalProperties"], false, "{}", op.tool_name);
        assert!(
            !op.input_schema.contains("$ref"),
            "{}: $ref не раскрыт",
            op.tool_name
        );
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{}: нет properties", op.tool_name));
        let param_names: BTreeSet<&str> = op.params.iter().map(|p| p.name).collect();
        let property_names: BTreeSet<&str> = properties.keys().map(String::as_str).collect();
        assert_eq!(param_names, property_names, "{}", op.tool_name);
        assert_eq!(
            param_names.len(),
            op.params.len(),
            "{}: повтор параметра",
            op.tool_name
        );
        for header in ["Authorization", "X-Org-Id", "X-Cloud-Org-Id"] {
            assert!(
                !property_names
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(header)),
                "{}: заголовок {header} попал в аргументы",
                op.tool_name
            );
        }

        for param in op.params {
            assert!(
                !param.description.is_empty(),
                "{}: {}",
                op.tool_name,
                param.name
            );
            assert_eq!(
                properties[param.name]["description"], param.description,
                "{}: описание {} в схеме и ParamDef различается",
                op.tool_name, param.name
            );
        }

        let required: BTreeSet<&str> = schema["required"]
            .as_array()
            .map(|r| r.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let expected: BTreeSet<&str> = op
            .params
            .iter()
            .filter(|p| p.required)
            .map(|p| p.name)
            .collect();
        assert_eq!(required, expected, "{}", op.tool_name);

        check_schema(op.tool_name, "", &schema);
    }
}

/// Проверяет, что схема понятна MCP-клиентам: известные типы и форматы, имена свойств по
/// шаблону `^[a-zA-Z0-9_.-]{1,64}$` (ограничение API Claude), непустой `required` из имён
/// свойств, без `title`, `pattern` с синтаксисом Python и расширений.
fn check_schema(tool: &str, at: &str, schema: &Value) {
    const TYPES: [&str; 7] = [
        "string", "number", "integer", "boolean", "array", "object", "null",
    ];
    let object = schema
        .as_object()
        .unwrap_or_else(|| panic!("{tool}{at}: схема не объект"));
    for key in object.keys() {
        assert!(
            !key.starts_with("x-")
                && !matches!(key.as_str(), "example" | "examples" | "$ref" | "title"),
            "{tool}{at}: лишний ключ {key}"
        );
    }
    if let Some(format) = object.get("format") {
        assert!(
            matches!(
                format.as_str(),
                Some("date-time" | "date" | "uuid" | "uri" | "email")
            ),
            "{tool}{at}: формат {format}"
        );
    }
    if let Some(pattern) = object.get("pattern").and_then(Value::as_str) {
        assert!(!pattern.contains("(?P<"), "{tool}{at}: pattern Python");
    }
    match object.get("type") {
        None => {}
        Some(Value::String(kind)) => {
            assert!(TYPES.contains(&kind.as_str()), "{tool}{at}: тип {kind}")
        }
        Some(Value::Array(kinds)) => {
            for kind in kinds {
                assert!(
                    kind.as_str().is_some_and(|k| TYPES.contains(&k)),
                    "{tool}{at}: тип {kind}"
                );
            }
        }
        Some(other) => panic!("{tool}{at}: тип {other}"),
    }
    let is_array = match object.get("type") {
        Some(Value::String(kind)) => kind == "array",
        Some(Value::Array(kinds)) => kinds.iter().any(|k| k == "array"),
        _ => false,
    };
    assert!(
        !is_array || object.contains_key("items"),
        "{tool}{at}: массив без items"
    );
    let properties = object.get("properties").and_then(Value::as_object);
    if let Some(required) = object.get("required") {
        let required = required
            .as_array()
            .unwrap_or_else(|| panic!("{tool}{at}: required не массив"));
        assert!(!required.is_empty(), "{tool}{at}: пустой required");
        for name in required {
            let name = name.as_str().expect("имя в required");
            assert!(
                properties.is_some_and(|p| p.contains_key(name)),
                "{tool}{at}: required {name} нет в properties"
            );
        }
    }
    for (name, property) in properties.into_iter().flatten() {
        assert!(
            !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')),
            "{tool}{at}: недопустимое имя свойства {name}"
        );
        check_schema(tool, &format!("{at}.{name}"), property);
    }
    if let Some(items) = object.get("items") {
        check_schema(tool, &format!("{at}[]"), items);
    }
    if let Some(additional) = object.get("additionalProperties")
        && additional.is_object()
    {
        check_schema(tool, &format!("{at}{{}}"), additional);
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(variants) = object.get(key) {
            for variant in variants.as_array().expect("варианты") {
                check_schema(tool, &format!("{at}|"), variant);
            }
        }
    }
}

#[test]
fn path_params_match_templates() {
    for op in operations() {
        let declared: BTreeSet<&str> = op.params_in(ParamLocation::Path).map(|p| p.name).collect();
        assert_eq!(placeholders(op.path), declared, "{}", op.tool_name);
        assert!(
            op.params_in(ParamLocation::Path).all(|p| p.required),
            "{}: path-параметр необязателен",
            op.tool_name
        );
        assert!(op.path.starts_with("/v1/"), "{}: {}", op.tool_name, op.path);
    }
}

#[test]
fn simple_params_take_type_from_nullable_any_of() {
    // `anyOf: [{type: integer}, {type: null}]` — целое, а не строка.
    let revision = op("get_page").param("revision_id").expect("revision_id");
    assert_eq!(revision.value_type, ParamType::Integer);
    let is_personal = op("list_spaces").param("is_personal").expect("is_personal");
    assert_eq!(is_personal.value_type, ParamType::Boolean);
    // `allOf` с перечислением и `anyOf` с `$ref` на перечисление раскрываются.
    let direction = &schema(op("list_attachments"))["properties"]["order_direction"];
    assert_eq!(direction["enum"], json!(["asc", "desc"]));
    assert_eq!(direction["default"], "asc");
    let actuality = &schema(op("list_page_descendants"))["properties"]["actuality"];
    assert_eq!(actuality["type"], "string");
    assert!(actuality["enum"].is_array(), "{actuality}");
}

#[test]
fn body_kind_matches_params() {
    for op in operations() {
        let bodies: Vec<_> = op.params_in(ParamLocation::Body).collect();
        match op.body {
            None => assert!(bodies.is_empty(), "{}", op.tool_name),
            Some(BodyKind::Json) => assert!(
                matches!(bodies.as_slice(), [body] if body.name == "body" && body.value_type == ParamType::Object),
                "{}",
                op.tool_name
            ),
            Some(BodyKind::Binary) => {
                assert!(
                    matches!(bodies.as_slice(), [file] if file.name == "file" && file.value_type == ParamType::File && file.required),
                    "{}",
                    op.tool_name
                );
                assert!(bodies[0].description.contains("16 МиБ"), "{}", op.tool_name);
            }
        }
        for param in op.params {
            assert_eq!(
                param.value_type == ParamType::File,
                param.location == ParamLocation::Body && op.body == Some(BodyKind::Binary),
                "{}: {}",
                op.tool_name,
                param.name
            );
        }
    }
    // Загрузка части файла, если тело описано, отправляет файл сырым телом.
    if let Some(body) = op("upload_file_part").body {
        assert_eq!(body, BodyKind::Binary);
    }
}

#[test]
fn access_levels_and_kinds() {
    for op in operations() {
        if op.method == HttpMethod::Get {
            assert_eq!(op.access, AccessLevel::Read, "{}", op.tool_name);
        }
    }
    assert_eq!(op("get_page").access, AccessLevel::Read);
    assert_eq!(op("search").access, AccessLevel::Read);
    assert_eq!(op("create_page").access, AccessLevel::Write);
    assert_eq!(op("delete_page").access, AccessLevel::Destructive);

    let binary: BTreeSet<&str> = operations()
        .iter()
        .filter(|op| op.response == ResponseKind::Binary)
        .map(|op| op.tool_name)
        .collect();
    assert_eq!(
        binary,
        BTreeSet::from([
            "download_attachment",
            "download_attachment_by_url",
            "download_attachment_preview",
        ])
    );
}

/// Снапшот с сортировкой ключей, чтобы порядок не зависел от фичи `preserve_order`
/// у `serde_json`, которую может включить любая зависимость.
macro_rules! snapshot {
    ($name:expr) => {
        insta::with_settings!({ sort_maps => true }, {
            insta::assert_json_snapshot!(snapshot_view(op($name)));
        })
    };
}

/// Представление операции для снапшота: все поля плюс разобранная JSON Schema.
fn snapshot_view(op: &OperationDef) -> Value {
    let params: Vec<Value> = op
        .params
        .iter()
        .map(|p| {
            json!({
                "name": p.name,
                "location": format!("{:?}", p.location),
                "required": p.required,
                "value_type": format!("{:?}", p.value_type),
                "description": p.description,
            })
        })
        .collect();
    json!({
        "tool_name": op.tool_name,
        "upstream_operation_id": op.upstream_operation_id,
        "toolset": op.toolset,
        "summary": op.summary,
        "description": op.description,
        "tool_description": op.tool_description(),
        "docs_url": op.docs_url,
        "method": op.method.as_str(),
        "path": op.path,
        "access": format!("{:?}", op.access),
        "params": params,
        "body": op.body.map(|kind| format!("{kind:?}")),
        "response": format!("{:?}", op.response),
        "input_schema": schema(op),
    })
}

/// Список всех инструментов и наборов с аргументами (имя, тип, обязательность).
#[test]
fn snapshot_tool_list() {
    let tools: Vec<Value> = operations()
        .iter()
        .map(|op| {
            let schema = schema(op);
            let args: BTreeMap<&str, String> = op
                .params
                .iter()
                .map(|p| {
                    let kind = schema["properties"][p.name]["type"]
                        .as_str()
                        .unwrap_or("any")
                        .to_owned();
                    let mark = if p.required { "" } else { "?" };
                    (p.name, format!("{kind}{mark}"))
                })
                .collect();
            json!({
                "name": op.tool_name,
                "toolset": op.toolset,
                "access": op.access.as_str(),
                "request": format!("{} {}", op.method.as_str(), op.path),
                "body": op.body.map(|kind| format!("{kind:?}")),
                "response": format!("{:?}", op.response),
                "args": args,
            })
        })
        .collect();
    let sets: Vec<Value> = toolsets()
        .iter()
        .map(|t| {
            json!({
                "slug": t.slug,
                "description": t.description,
                "default_enabled": t.default_enabled,
            })
        })
        .collect();
    insta::with_settings!({ sort_maps => true }, {
        insta::assert_json_snapshot!(json!({"toolsets": sets, "tools": tools}));
    });
}

#[test]
fn snapshot_get_page() {
    // Чтение по query-параметрам, необязательные параметры с anyOf и null.
    snapshot!("get_page");
}

#[test]
fn snapshot_create_page() {
    // JSON-тело с раскрытыми $ref.
    snapshot!("create_page");
}

#[test]
fn snapshot_list_page_descendants() {
    // Разбивка выдачи: cursor и page_size.
    snapshot!("list_page_descendants");
}

#[test]
fn snapshot_search() {
    // POST, который только читает.
    snapshot!("search");
}

#[test]
fn snapshot_upload_file_part() {
    // Сырое тело из локального файла.
    snapshot!("upload_file_part");
}

#[test]
fn snapshot_download_attachment() {
    // Скачивание файла: двоичный ответ.
    snapshot!("download_attachment");
}

#[test]
fn snapshot_delete_page() {
    snapshot!("delete_page");
}

// Генератор на синтетических спецификациях.

/// Минимальная корректная спецификация с одной операцией; `edit` меняет операцию.
fn synthetic(edit: impl FnOnce(&mut Value)) -> Value {
    let mut spec = json!({
        "openapi": "3.1.0",
        "tags": [
            {"name": "files", "description": "Files", "x-default-enabled": true}
        ],
        "paths": {
            "/v1/upload/{session_id}": {
                "put": {
                    "operationId": "upload_part",
                    "summary": "Upload part",
                    "tags": ["files"],
                    "x-access-level": "write",
                    "externalDocs": {"url": "https://example.test/upload"},
                    "parameters": [
                        {"in": "path", "name": "session_id", "required": true,
                         "schema": {"type": "string", "format": "uuid4"}},
                        {"in": "query", "name": "part_number", "required": true,
                         "schema": {"type": "integer", "minimum": 1}},
                        {"in": "query", "name": "revision",
                         "schema": {"anyOf": [{"type": "integer"}, {"type": "null"}]}},
                        {"in": "header", "name": "x-org-id", "schema": {"type": "string"}},
                        {"in": "header", "name": "AUTHORIZATION", "schema": {"type": "string"}}
                    ],
                    "requestBody": {
                        "required": true,
                        "content": {"application/octet-stream": {
                            "schema": {"type": "string", "format": "binary"}
                        }}
                    },
                    "responses": {"200": {"description": "OK", "content": {
                        "application/json": {"schema": {"type": "object"}}
                    }}}
                }
            }
        }
    });
    edit(&mut spec["paths"]["/v1/upload/{session_id}"]["put"]);
    spec
}

#[test]
fn generator_supports_raw_file_body_and_skips_auth_headers() {
    let code = generate::generate(&synthetic(|_| {}));
    assert!(code.contains("body: Some(BodyKind::Binary)"), "{code}");
    assert!(
        code.contains("ParamDef { name: \"file\", location: ParamLocation::Body, required: true"),
        "{code}"
    );
    assert!(code.contains("value_type: ParamType::File"), "{code}");
    assert!(
        code.contains("ParamDef { name: \"revision\", location: ParamLocation::Query, required: false, description: \"revision\", value_type: ParamType::Integer }"),
        "{code}"
    );
    assert!(
        code.contains("docs_url: Some(\"https://example.test/upload\")"),
        "{code}"
    );
    assert!(code.contains("default_enabled: true"), "{code}");
    assert!(!code.to_ascii_lowercase().contains("x-org-id"), "{code}");
    assert!(!code.contains("AUTHORIZATION"), "{code}");
    assert!(code.contains(r#"\"format\":\"uuid\""#), "{code}");
}

#[test]
fn generator_allows_missing_external_docs() {
    let code = generate::generate(&synthetic(|op| {
        op.as_object_mut().unwrap().remove("externalDocs");
    }));
    assert!(code.contains("docs_url: None"), "{code}");
}

#[test]
fn generator_marks_undescribed_get_and_file_responses_as_binary() {
    let code = generate::generate(&synthetic(|op| {
        op["responses"] = json!({"200": {"description": "OK"}});
    }));
    // PUT без описания ответа — не скачивание.
    assert!(code.contains("response: ResponseKind::Json"), "{code}");
    let code = generate::generate(&synthetic(|op| {
        op["responses"] = json!({"200": {"description": "OK", "content": {
            "application/octet-stream": {"schema": {"type": "string", "format": "binary"}}
        }}});
    }));
    assert!(code.contains("response: ResponseKind::Binary"), "{code}");
}

#[test]
#[should_panic(expected = "набор «unknown» не объявлен в tags")]
fn generator_rejects_unknown_toolset() {
    generate::generate(&synthetic(|op| op["tags"] = json!(["unknown"])));
}

#[test]
#[should_panic(expected = "нет operationId")]
fn generator_rejects_missing_operation_id() {
    generate::generate(&synthetic(|op| {
        op.as_object_mut().unwrap().remove("operationId");
    }));
}

#[test]
#[should_panic(expected = "нет x-access-level")]
fn generator_rejects_missing_access_level() {
    generate::generate(&synthetic(|op| {
        op.as_object_mut().unwrap().remove("x-access-level");
    }));
}

#[test]
#[should_panic(expected = "неизвестный x-access-level «admin»")]
fn generator_rejects_unknown_access_level() {
    generate::generate(&synthetic(|op| op["x-access-level"] = json!("admin")));
}

#[test]
#[should_panic(expected = "не подходит как имя MCP-инструмента")]
fn generator_rejects_invalid_tool_name() {
    generate::generate(&synthetic(|op| op["operationId"] = json!("upload part")));
}

#[test]
#[should_panic(expected = "не подходит как имя MCP-инструмента")]
fn generator_rejects_too_long_tool_name() {
    generate::generate(&synthetic(|op| op["operationId"] = json!("a".repeat(65))));
}

#[test]
#[should_panic(expected = "должна быть type: string, format: binary")]
fn generator_rejects_non_binary_octet_stream_body() {
    generate::generate(&synthetic(|op| {
        op["requestBody"]["content"]["application/octet-stream"]["schema"] =
            json!({"type": "object"});
    }));
}

#[test]
#[should_panic(expected = "неподдерживаемый заголовок X-Custom")]
fn generator_rejects_other_headers() {
    generate::generate(&synthetic(|op| {
        op["parameters"]
            .as_array_mut()
            .unwrap()
            .push(json!({"in": "header", "name": "X-Custom", "schema": {"type": "string"}}));
    }));
}

#[test]
#[should_panic(expected = "нет булева x-default-enabled")]
fn generator_requires_default_enabled_flag() {
    let mut spec = synthetic(|_| {});
    spec["tags"][0]
        .as_object_mut()
        .unwrap()
        .remove("x-default-enabled");
    generate::generate(&spec);
}
