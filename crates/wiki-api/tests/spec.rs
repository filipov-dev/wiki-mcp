//! Проверки встроенной объединённой OpenAPI-спецификации Wiki API.

use std::collections::BTreeSet;

use serde_json::Value;

const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];
const DOCS_PREFIX: &str = "https://yandex.ru/support/wiki/ru/api-ref/";
const EXTRA_TOOLSET: &str = "extra";
const ORG_HEADERS: [(&str, &str); 2] = [("XOrgId", "X-Org-Id"), ("XCloudOrgId", "X-Cloud-Org-Id")];

fn spec() -> Value {
    serde_json::from_str(wiki_api::OPENAPI_JSON).expect("спецификация должна быть корректным JSON")
}

fn operations(spec: &Value) -> Vec<(String, &'static str, &Value)> {
    let mut out = Vec::new();
    for (path, item) in spec["paths"].as_object().into_iter().flatten() {
        for method in METHODS {
            if let Some(op) = item.get(method) {
                out.push((path.clone(), method, op));
            }
        }
    }
    out
}

fn toolset(op: &Value) -> &str {
    op["tags"][0].as_str().unwrap_or_default()
}

fn resolve<'a>(spec: &'a Value, value: &'a Value) -> &'a Value {
    match value["$ref"].as_str().and_then(|r| r.strip_prefix('#')) {
        Some(pointer) => spec
            .pointer(pointer)
            .unwrap_or_else(|| panic!("ссылка #{pointer} никуда не ведёт")),
        None => value,
    }
}

fn collect_refs(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                // Примеры — данные Wiki, `$ref` в них не ссылка.
                if key == "example" || key == "examples" {
                    continue;
                }
                if key == "$ref" {
                    if let Some(reference) = child.as_str() {
                        out.push(reference.to_owned());
                    }
                } else {
                    collect_refs(child, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_refs(item, out)),
        _ => {}
    }
}

/// Имя MCP-инструмента должно соответствовать `^[a-zA-Z0-9_-]{1,64}$`.
fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[test]
fn spec_is_openapi_3_1() {
    let spec = spec();
    assert!(
        spec["openapi"]
            .as_str()
            .is_some_and(|version| version.starts_with("3.1")),
        "ожидается OpenAPI 3.1"
    );
}

#[test]
fn every_operation_has_name_access_level_and_toolset() {
    let spec = spec();
    let toolsets: BTreeSet<&str> = spec["tags"]
        .as_array()
        .expect("tags должен быть списком наборов")
        .iter()
        .filter_map(|tag| tag["name"].as_str())
        .collect();
    let ops = operations(&spec);
    assert!(!ops.is_empty(), "в спецификации нет операций");
    for (path, method, op) in ops {
        assert!(path.starts_with("/v1/"), "{method} {path}: путь не из /v1/");
        let name = op["operationId"].as_str().unwrap_or_default();
        assert!(!name.is_empty(), "{method} {path}: нет operationId");
        let level = op["x-access-level"].as_str();
        assert!(
            matches!(level, Some("read" | "write" | "destructive")),
            "{method} {path}: неизвестный x-access-level {level:?}"
        );
        let tags: Vec<&str> = op["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        assert!(
            tags.len() == 1 && toolsets.contains(tags[0]),
            "{method} {path}: у операции должен быть ровно один известный набор, а есть {tags:?}"
        );
    }
}

#[test]
fn tool_names_are_unique_and_valid() {
    let spec = spec();
    let mut seen = BTreeSet::new();
    for (path, method, op) in operations(&spec) {
        let name = op["operationId"].as_str().unwrap_or_default();
        assert!(
            is_valid_tool_name(name),
            "{method} {path}: имя {name:?} не подходит под ^[a-zA-Z0-9_-]{{1,64}}$"
        );
        assert!(
            seen.insert(name.to_owned()),
            "{method} {path}: имя {name} повторяется"
        );
    }
}

#[test]
fn every_operation_has_summary_description_and_docs() {
    let spec = spec();
    for (path, method, op) in operations(&spec) {
        for field in ["summary", "description"] {
            assert!(
                op[field]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()),
                "{method} {path}: нет {field}"
            );
        }
        let url = op["externalDocs"]["url"].as_str().unwrap_or_default();
        let page = url.strip_prefix(DOCS_PREFIX).unwrap_or_else(|| {
            panic!("{method} {path}: externalDocs.url {url:?} не ведёт в справочник Wiki API")
        });
        let description = op["description"].as_str().unwrap_or_default();
        if toolset(op) == EXTRA_TOOLSET {
            assert!(
                description.contains("нет в справочнике"),
                "{method} {path}: в описании операции набора extra не сказано, что её нет в справочнике"
            );
        } else {
            // Операция из справочника ссылается на свою страницу, а не на обзор раздела.
            assert!(
                page.contains("__"),
                "{method} {path}: externalDocs.url {url} ведёт на обзор, а не на страницу операции"
            );
        }
    }
}

#[test]
fn all_refs_resolve() {
    let spec = spec();
    let mut refs = Vec::new();
    collect_refs(&spec, &mut refs);
    assert!(!refs.is_empty(), "в спецификации нет $ref");
    for reference in refs {
        let pointer = reference
            .strip_prefix('#')
            .unwrap_or_else(|| panic!("внешняя ссылка {reference}"));
        assert!(
            spec.pointer(pointer).is_some(),
            "ссылка {reference} никуда не ведёт"
        );
    }
}

#[test]
fn path_parameters_match_templates() {
    let spec = spec();
    for (path, method, op) in operations(&spec) {
        let in_template: BTreeSet<String> = path
            .split('{')
            .skip(1)
            .filter_map(|part| part.split_once('}').map(|(name, _)| name.to_owned()))
            .collect();
        let path_item = &spec["paths"][path.as_str()];
        let declared: BTreeSet<String> = [&path_item["parameters"], &op["parameters"]]
            .into_iter()
            .filter_map(Value::as_array)
            .flatten()
            .map(|param| resolve(&spec, param))
            .filter(|param| param["in"] == "path")
            .filter_map(|param| param["name"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(
            in_template, declared,
            "{method} {path}: параметры пути не совпадают с шаблоном"
        );
    }
}

#[test]
fn every_parameter_is_described() {
    let spec = spec();
    for (path, method, op) in operations(&spec) {
        for param in op["parameters"].as_array().into_iter().flatten() {
            let param = resolve(&spec, param);
            let name = param["name"].as_str().unwrap_or_default();
            assert!(
                param["description"]
                    .as_str()
                    .is_some_and(|text| !text.trim().is_empty()),
                "{method} {path}: у параметра {name} нет описания"
            );
        }
    }
}

#[test]
fn request_bodies_have_single_media_type() {
    let spec = spec();
    for (path, method, op) in operations(&spec) {
        let Some(body) = op.get("requestBody") else {
            continue;
        };
        let content = resolve(&spec, body)["content"]
            .as_object()
            .unwrap_or_else(|| panic!("{method} {path}: requestBody без content"));
        assert_eq!(
            content.len(),
            1,
            "{method} {path}: у requestBody должен быть ровно один тип содержимого"
        );
    }
}

#[test]
fn access_levels_follow_rules() {
    let spec = spec();
    for (path, method, op) in operations(&spec) {
        let level = op["x-access-level"].as_str().unwrap_or_default();
        match method {
            "get" => assert_eq!(level, "read", "{method} {path}: чтение должно быть read"),
            "delete" => assert_eq!(
                level, "destructive",
                "{method} {path}: удаление должно быть destructive"
            ),
            _ => {}
        }
        if toolset(op) == "access" {
            assert_eq!(
                level, "destructive",
                "{method} {path}: изменение прав доступа должно быть destructive"
            );
        }
    }
    for (method, path) in [
        ("post", "/v1/search"),
        ("post", "/v1/grids/{idx}/columns/suggest"),
    ] {
        assert_eq!(
            spec["paths"][path][method]["x-access-level"], "read",
            "{method} {path}: операция только читает данные"
        );
    }
}

#[test]
fn extra_toolset_is_disabled_by_default() {
    let spec = spec();
    let tags = spec["tags"].as_array().expect("tags должен быть списком");
    let extra = tags
        .iter()
        .find(|tag| tag["name"] == EXTRA_TOOLSET)
        .expect("нет набора extra");
    assert_eq!(extra["x-default-enabled"], false);
    let used: BTreeSet<&str> = operations(&spec)
        .into_iter()
        .map(|(_, _, op)| toolset(op))
        .collect();
    for tag in tags {
        let name = tag["name"].as_str().unwrap_or_default();
        assert!(used.contains(name), "набор {name} пуст");
    }
}

#[test]
fn auth_headers_are_described_but_not_passed_by_agent() {
    let spec = spec();
    let schemes = &spec["components"]["securitySchemes"];
    assert_eq!(schemes["OAuth"]["type"], "apiKey");
    assert_eq!(schemes["OAuth"]["in"], "header");
    assert_eq!(schemes["OAuth"]["name"], "Authorization");
    assert_eq!(schemes["IamToken"]["type"], "http");
    assert_eq!(schemes["IamToken"]["scheme"], "bearer");
    let security: BTreeSet<&str> = spec["security"]
        .as_array()
        .expect("нет верхнеуровневого security")
        .iter()
        .filter_map(Value::as_object)
        .flat_map(|requirement| requirement.keys().map(String::as_str))
        .collect();
    assert_eq!(security, BTreeSet::from(["IamToken", "OAuth"]));
    for (component, header) in ORG_HEADERS {
        let param = &spec["components"]["parameters"][component];
        assert_eq!(param["name"], header, "components.parameters.{component}");
        assert_eq!(param["in"], "header", "components.parameters.{component}");
        assert!(
            param["description"]
                .as_str()
                .is_some_and(|text| text.contains("ровно")),
            "components.parameters.{component}: нужно сказать, что нужен ровно один заголовок"
        );
    }
    // Заголовки подставляет исполнитель: в параметрах операций их быть не должно.
    for (path, method, op) in operations(&spec) {
        for param in op["parameters"].as_array().into_iter().flatten() {
            let param = resolve(&spec, param);
            assert_ne!(
                param["in"], "header",
                "{method} {path}: заголовок {} в параметрах операции",
                param["name"]
            );
        }
    }
}

#[test]
fn upload_part_takes_binary_body() {
    let spec = spec();
    let op = &spec["paths"]["/v1/upload_sessions/{session_id}/upload_part"]["put"];
    let body = &op["requestBody"];
    assert_eq!(body["required"], true);
    let content = body["content"].as_object().expect("нет content");
    assert_eq!(
        content.keys().map(String::as_str).collect::<Vec<_>>(),
        ["application/octet-stream"]
    );
    let schema = &content["application/octet-stream"]["schema"];
    assert_eq!(schema["type"], "string");
    assert_eq!(schema["format"], "binary");
    let part_number = op["parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|param| param["name"] == "part_number")
        .expect("нет параметра part_number");
    assert_eq!(part_number["in"], "query");
    assert_eq!(part_number["required"], true);
}

#[test]
fn downloads_return_binary() {
    let spec = spec();
    for path in [
        "/v1/pages/attachments/download_by_url",
        "/v1/pages/{idx}/attachments/{file_id}/download",
        "/v1/pages/{idx}/attachments/{file_id}/preview",
    ] {
        let content = &spec["paths"][path]["get"]["responses"]["200"]["content"];
        assert_eq!(
            content["application/octet-stream"]["schema"]["format"], "binary",
            "GET {path}: ответ должен быть бинарным"
        );
    }
}
