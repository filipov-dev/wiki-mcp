//! Построение таблицы операций из спецификации и генерация Rust-кода.
//!
//! Этапы: наборы инструментов из `tags` → разбор операций (параметры, тело, ответ) →
//! JSON Schema аргументов → сортировка по имени инструмента → проверки → вывод
//! `TOOLSETS` и `OPERATIONS`.
//!
//! Любое расхождение спецификации с тем, что понимает генератор (неизвестный набор,
//! операция без `operationId` или `x-access-level`, недопустимое имя, неподдерживаемый
//! тип тела или параметра), — `panic!` с понятным текстом: сборка падает, а не
//! генерирует инструмент, который поведёт себя не так, как описано.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use crate::naming::is_valid_tool_name;
use crate::schema::{Resolver, deref, description_of, lookup, normalize_format};
use crate::toolsets::{Toolset, parse_toolsets};

const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

/// Заголовки авторизации и организации. Их подставляет исполнитель из профиля или
/// HTTP-запроса агента, в аргументы инструмента они не попадают. Сравнение без учёта
/// регистра.
const AUTH_HEADERS: &[&str] = &["Authorization", "X-Org-Id", "X-Cloud-Org-Id"];

/// Ключи элемента `paths`, кроме методов, которые генератор понимает.
const PATH_ITEM_KEYS: &[&str] = &["parameters", "summary", "description"];

const JSON_MEDIA: &str = "application/json";
const BINARY_MEDIA: &str = "application/octet-stream";

/// Имя аргумента с путём к локальному файлу для сырого тела запроса.
const FILE_BODY_ARG: &str = "file";

/// Ключевые слова схемы простого параметра, которые переносятся в JSON Schema.
/// `pattern` не переносится: в официальной спецификации это регулярные выражения Python
/// (`(?P<name>...)`), которые не понимают валидаторы MCP-клиентов.
const SIMPLE_KEYS: &[&str] = &[
    "enum",
    "const",
    "default",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Location {
    Path,
    Query,
    Body,
}

impl Location {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Path => "ParamLocation::Path",
            Self::Query => "ParamLocation::Query",
            Self::Body => "ParamLocation::Body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Json,
    Binary,
}

impl BodyKind {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Json => "BodyKind::Json",
            Self::Binary => "BodyKind::Binary",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseKind {
    Json,
    Binary,
}

impl ResponseKind {
    fn rust_name(self) -> &'static str {
        match self {
            Self::Json => "ResponseKind::Json",
            Self::Binary => "ResponseKind::Binary",
        }
    }
}

#[derive(Debug, Clone)]
struct Param {
    name: String,
    location: Location,
    required: bool,
    description: String,
    /// Имя варианта `ParamType` в `spec.rs`.
    value_type: &'static str,
    /// JSON Schema значения без описания.
    schema: Value,
}

#[derive(Debug)]
struct Operation {
    tool_name: String,
    upstream_operation_id: Option<String>,
    toolset: String,
    summary: String,
    description: String,
    docs_url: Option<String>,
    method: &'static str,
    path: String,
    access: &'static str,
    params: Vec<Param>,
    body: Option<BodyKind>,
    response: ResponseKind,
    input_schema: String,
}

/// Генерирует исходный код `operations.rs` из разобранной спецификации.
pub fn generate(spec: &Value) -> String {
    let toolsets = parse_toolsets(spec);
    let mut resolver = Resolver::new(spec);
    let mut operations = parse_operations(spec, &toolsets, &mut resolver);
    operations.sort_by(|a, b| a.tool_name.cmp(&b.tool_name));
    check_invariants(&operations, &toolsets);
    emit(&toolsets, &operations)
}

fn parse_operations(
    spec: &Value,
    toolsets: &[Toolset],
    resolver: &mut Resolver<'_>,
) -> Vec<Operation> {
    let paths = spec
        .get("paths")
        .and_then(Value::as_object)
        .expect("в спецификации нет paths");
    let mut operations = Vec::new();
    for (path, item) in paths {
        let item = item
            .as_object()
            .expect("элемент paths не является объектом");
        for key in item.keys() {
            assert!(
                METHODS.contains(&key.as_str()) || PATH_ITEM_KEYS.contains(&key.as_str()),
                "неподдерживаемый ключ {key} в пути {path}"
            );
        }
        let shared: Vec<&Value> = item
            .get("parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect();
        for method in METHODS {
            if let Some(op) = item.get(*method) {
                operations.push(parse_operation(
                    spec, toolsets, resolver, path, method, op, &shared,
                ));
            }
        }
    }
    operations
}

fn parse_operation(
    spec: &Value,
    toolsets: &[Toolset],
    resolver: &mut Resolver<'_>,
    path: &str,
    method: &'static str,
    op: &Value,
    shared_params: &[&Value],
) -> Operation {
    let tool_name =
        str_field(op, "operationId").unwrap_or_else(|| panic!("у {method} {path} нет operationId"));
    assert!(
        is_valid_tool_name(&tool_name),
        "{method} {path}: operationId «{tool_name}» не подходит как имя MCP-инструмента \
         (нужно ^[a-zA-Z0-9_-]{{1,64}}$)"
    );
    let tags: Vec<&str> = op
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| tags.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let [tag] = tags.as_slice() else {
        panic!("у {tool_name} должен быть ровно один тег (набор инструментов), а не {tags:?}");
    };
    let toolset = toolsets
        .iter()
        .find(|t| t.slug == *tag)
        .unwrap_or_else(|| panic!("{tool_name}: набор «{tag}» не объявлен в tags спецификации"))
        .slug
        .clone();
    let access = match str_field(op, "x-access-level").as_deref() {
        Some("read") => "AccessLevel::Read",
        Some("write") => "AccessLevel::Write",
        Some("destructive") => "AccessLevel::Destructive",
        None => panic!("{tool_name}: нет x-access-level (read, write или destructive)"),
        Some(other) => panic!("{tool_name}: неизвестный x-access-level «{other}»"),
    };
    let docs_url = op
        .get("externalDocs")
        .and_then(|docs| str_field(docs, "url"))
        .filter(|url| !url.is_empty());

    let own_params = op
        .get("parameters")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    let mut params = Vec::new();
    for raw in shared_params.iter().copied().chain(own_params) {
        if let Some(param) = parse_param(spec, &tool_name, raw) {
            params.push(param);
        }
    }
    let body = op
        .get("requestBody")
        .map(|raw| parse_body(spec, resolver, &tool_name, raw, &mut params));
    let response = response_kind(spec, &tool_name, method, op);

    for param in &mut params {
        param.description = finish_description(&param.name, &param.description);
    }
    check_path_params(&tool_name, path, &params);
    let input_schema = input_schema(&tool_name, &params);

    Operation {
        upstream_operation_id: str_field(op, "x-upstream-operation-id"),
        tool_name,
        toolset,
        summary: str_field(op, "summary").unwrap_or_default(),
        description: str_field(op, "description").unwrap_or_default(),
        docs_url,
        method,
        path: path.to_owned(),
        access,
        params,
        body,
        response,
        input_schema,
    }
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(|s| s.trim().to_owned())
}

/// Параметр пути или запроса; `None` для заголовков авторизации и организации.
fn parse_param(spec: &Value, operation_id: &str, raw: &Value) -> Option<Param> {
    let raw = deref(spec, raw);
    let object = raw.as_object().expect("параметр не является объектом");
    let name = str_field(raw, "name").expect("у параметра нет имени");
    let location = match str_field(raw, "in").as_deref() {
        Some("path") => Location::Path,
        Some("query") => Location::Query,
        Some("header") => {
            // Авторизацию и организацию задаёт только исполнитель: агент не должен
            // их видеть и переопределять. Другие заголовки генератор не поддерживает.
            assert!(
                AUTH_HEADERS.iter().any(|h| h.eq_ignore_ascii_case(&name)),
                "{operation_id}: неподдерживаемый заголовок {name}"
            );
            return None;
        }
        other => panic!("{operation_id}: неподдерживаемое расположение параметра {other:?}"),
    };
    let required =
        location == Location::Path || raw.get("required").and_then(Value::as_bool) == Some(true);
    let schema = raw
        .get("schema")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: у параметра {name} нет schema"));
    let flat = flatten_simple(spec, operation_id, &name, schema);
    let (schema, value_type) = simple_schema(spec, operation_id, &name, &flat);
    assert_ne!(
        value_type, "File",
        "{operation_id}: файл в параметре {name}; файл передаётся только телом запроса"
    );
    Some(Param {
        name,
        location,
        required,
        description: description_of(object)
            .or_else(|| description_of(&flat))
            .unwrap_or_default(),
        value_type,
        schema,
    })
}

/// Разбирает `requestBody`: JSON-тело становится параметром `body`, сырое тело
/// (`application/octet-stream`) — параметром `file` с путём к локальному файлу.
fn parse_body(
    spec: &Value,
    resolver: &mut Resolver<'_>,
    operation_id: &str,
    raw: &Value,
    params: &mut Vec<Param>,
) -> BodyKind {
    let raw = deref(spec, raw);
    let body_required = raw.get("required").and_then(Value::as_bool) == Some(true);
    let content = raw
        .get("content")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: requestBody без content"));
    let [(media_type, media)] = content.iter().collect::<Vec<_>>()[..] else {
        panic!("{operation_id}: у requestBody должен быть ровно один тип содержимого");
    };
    let raw_schema = media
        .get("schema")
        .unwrap_or_else(|| panic!("{operation_id}: requestBody без schema"));
    let body_description = raw
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    match media_type.as_str() {
        JSON_MEDIA => {
            let schema = resolver.resolve(raw_schema);
            let own = body_description
                .or_else(|| schema.get("description").and_then(Value::as_str))
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            params.push(Param {
                name: "body".to_owned(),
                location: Location::Body,
                required: body_required,
                description: format!("JSON request body.{own}"),
                value_type: "Object",
                schema,
            });
            BodyKind::Json
        }
        BINARY_MEDIA => {
            let schema = deref(spec, raw_schema);
            let is_binary = schema.get("type").and_then(Value::as_str) == Some("string")
                && schema.get("format").and_then(Value::as_str) == Some("binary");
            assert!(
                is_binary,
                "{operation_id}: схема тела {BINARY_MEDIA} должна быть type: string, \
                 format: binary"
            );
            let own = body_description
                .map(str::to_owned)
                .or_else(|| schema.as_object().and_then(description_of))
                .unwrap_or_default();
            params.push(Param {
                name: FILE_BODY_ARG.to_owned(),
                location: Location::Body,
                required: body_required,
                description: file_description(&own),
                value_type: "File",
                schema: json!({ "type": "string" }),
            });
            BodyKind::Binary
        }
        other => panic!("{operation_id}: неподдерживаемый тип requestBody {other}"),
    }
}

/// Вид успешного ответа.
///
/// Двоичный, если хоть один ответ `2xx` отдаёт не JSON (`application/octet-stream`,
/// `image/*` и другие). Официальная спецификация Wiki API не описывает тело скачивания
/// файла вовсе (ответ `200` без `content`), поэтому `GET`, у которого ни один ответ `2xx`
/// не описывает содержимое, тоже считается скачиванием файла.
fn response_kind(spec: &Value, operation_id: &str, method: &str, op: &Value) -> ResponseKind {
    let responses = op
        .get("responses")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{operation_id}: нет responses"));
    let mut kind = ResponseKind::Json;
    let mut described = false;
    for (code, response) in responses {
        if !code.starts_with('2') {
            continue;
        }
        let response = deref(spec, response);
        for media_type in response
            .get("content")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(Map::keys)
        {
            described = true;
            let essence = media_type.split(';').next().unwrap_or("").trim();
            if essence != JSON_MEDIA && !essence.ends_with("+json") {
                kind = ResponseKind::Binary;
            }
        }
    }
    if !described && method == "get" {
        kind = ResponseKind::Binary;
    }
    kind
}

/// Описание аргумента с файлом: агент передаёт путь к файлу, а не его содержимое.
fn file_description(description: &str) -> String {
    let note = "Абсолютный путь к локальному файлу: сервер читает его и отправляет \
                содержимое сырым телом запроса (application/octet-stream). Файл не пустой \
                и не больше 16 МиБ.";
    let text = description.trim();
    if text.is_empty() {
        note.to_owned()
    } else if text.ends_with(['.', '!', '?', ':']) {
        format!("{text} {note}")
    } else {
        format!("{text}. {note}")
    }
}

/// Схема простого параметра с раскрытыми `$ref`, вариантами `allOf` и без `null` из
/// `anyOf` и списка типов: модели Pydantic описывают необязательный параметр как
/// `anyOf: [{type: X}, {type: null}]`, а перечисление — как `allOf: [{$ref: ...}]`.
/// Ключи самой схемы важнее ключей вариантов (описание и `default` поля точнее).
fn flatten_simple(
    spec: &Value,
    operation_id: &str,
    name: &str,
    object: &Map<String, Value>,
) -> Map<String, Value> {
    let mut out = object.clone();
    for key in ["$ref", "allOf", "anyOf", "oneOf"] {
        out.remove(key);
    }
    let mut variants: Vec<&Value> = Vec::new();
    if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
        variants.push(lookup(spec, reference));
    }
    if let Some(all) = object.get("allOf") {
        let all = all
            .as_array()
            .unwrap_or_else(|| panic!("{operation_id}: allOf параметра {name} не список"));
        variants.extend(all.iter().map(|v| deref(spec, v)));
    }
    for key in ["anyOf", "oneOf"] {
        let Some(any) = object.get(key) else { continue };
        let any = any
            .as_array()
            .unwrap_or_else(|| panic!("{operation_id}: {key} параметра {name} не список"));
        let non_null: Vec<&Value> = any
            .iter()
            .map(|v| deref(spec, v))
            .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
            .collect();
        let [variant] = non_null[..] else {
            panic!(
                "{operation_id}: параметр {name} допускает несколько типов в {key}; \
                 генератор понимает только один тип и null"
            );
        };
        variants.push(variant);
    }
    for variant in variants {
        let variant = variant
            .as_object()
            .unwrap_or_else(|| panic!("{operation_id}: вариант схемы {name} не объект"));
        for (key, value) in flatten_simple(spec, operation_id, name, variant) {
            out.entry(key).or_insert(value);
        }
    }
    if let Some(Value::Array(kinds)) = out.get("type") {
        let non_null: Vec<&Value> = kinds.iter().filter(|k| *k != "null").collect();
        let [kind] = non_null[..] else {
            panic!("{operation_id}: неподдерживаемый тип {kinds:?} параметра {name}");
        };
        let kind = kind.clone();
        out.insert("type".into(), kind);
    }
    out
}

/// Схема и тип простого параметра (path, query) после [`flatten_simple`].
fn simple_schema(
    spec: &Value,
    operation_id: &str,
    name: &str,
    object: &Map<String, Value>,
) -> (Value, &'static str) {
    // Без типа простой параметр передаётся строкой.
    let kind = match object.get("type") {
        None => "string",
        Some(Value::String(kind)) => kind.as_str(),
        Some(other) => panic!("{operation_id}: неподдерживаемый тип {other} параметра {name}"),
    };
    if object.get("format").and_then(Value::as_str) == Some("binary") {
        return (json!({ "type": "string" }), "File");
    }
    let value_type = match kind {
        "string" => "String",
        "integer" => "Integer",
        "number" => "Number",
        "boolean" => "Boolean",
        "array" => "Array",
        other => panic!("{operation_id}: неподдерживаемый тип {other} параметра {name}"),
    };
    let mut schema = Map::new();
    schema.insert("type".into(), json!(kind));
    if let Some(format) = object.get("format") {
        schema.insert("format".into(), normalize_format(format));
    }
    for key in SIMPLE_KEYS {
        if let Some(value) = object.get(*key).filter(|v| !v.is_null()) {
            schema.insert((*key).into(), value.clone());
        }
    }
    if let Some(items) = object.get("items").and_then(Value::as_object) {
        let items = flatten_simple(spec, operation_id, name, items);
        let (items, item_type) = simple_schema(spec, operation_id, name, &items);
        assert!(
            !matches!(item_type, "Array" | "File"),
            "{operation_id}: неподдерживаемый элемент массива {name}"
        );
        schema.insert("items".into(), items);
    } else if value_type == "Array" {
        // Элементы массива в пути и строке запроса всё равно уходят текстом.
        schema.insert("items".into(), json!({ "type": "string" }));
    }
    (Value::Object(schema), value_type)
}

/// Итоговое описание параметра: текст спецификации или имя параметра.
fn finish_description(name: &str, description: &str) -> String {
    let text = description.trim();
    if text.is_empty() {
        name.to_owned()
    } else {
        text.to_owned()
    }
}

/// Проверяет, что плейсхолдеры `{name}` пути совпадают с path-параметрами.
fn check_path_params(operation_id: &str, path: &str, params: &[Param]) {
    let placeholders: BTreeSet<&str> = path
        .split('{')
        .skip(1)
        .filter_map(|part| part.split_once('}').map(|(name, _)| name))
        .collect();
    let declared: BTreeSet<&str> = params
        .iter()
        .filter(|p| p.location == Location::Path)
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(
        placeholders, declared,
        "{operation_id}: плейсхолдеры пути {path} не совпадают с path-параметрами"
    );
}

/// JSON Schema аргументов инструмента.
fn input_schema(operation_id: &str, params: &[Param]) -> String {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for param in params {
        let mut schema = param.schema.clone();
        let object = schema
            .as_object_mut()
            .expect("схема параметра не является объектом");
        object.insert(
            "description".into(),
            Value::String(param.description.clone()),
        );
        assert!(
            properties.insert(param.name.clone(), schema).is_none(),
            "{operation_id}: параметр {} повторяется",
            param.name
        );
        if param.required {
            required.push(Value::String(param.name.clone()));
        }
    }
    let mut schema = Map::new();
    schema.insert("type".into(), json!("object"));
    schema.insert("properties".into(), Value::Object(properties));
    if !required.is_empty() {
        schema.insert("required".into(), Value::Array(required));
    }
    schema.insert("additionalProperties".into(), json!(false));
    serde_json::to_string(&Value::Object(schema)).expect("сериализация JSON Schema")
}

fn check_invariants(operations: &[Operation], toolsets: &[Toolset]) {
    let mut names = BTreeSet::new();
    for op in operations {
        assert!(
            names.insert(op.tool_name.as_str()),
            "имя инструмента {} повторяется ({} {})",
            op.tool_name,
            op.method.to_uppercase(),
            op.path
        );
    }
    for toolset in toolsets {
        assert!(
            operations.iter().any(|op| op.toolset == toolset.slug),
            "в наборе {} нет операций",
            toolset.slug
        );
    }
}

fn rust_method(method: &str) -> &'static str {
    match method {
        "get" => "HttpMethod::Get",
        "post" => "HttpMethod::Post",
        "put" => "HttpMethod::Put",
        "patch" => "HttpMethod::Patch",
        "delete" => "HttpMethod::Delete",
        other => panic!("неподдерживаемый метод {other}"),
    }
}

fn rust_option(value: Option<&String>) -> String {
    value.map_or_else(|| "None".to_owned(), |s| format!("Some({s:?})"))
}

/// Строковые литералы выводятся через `{:?}`: это корректное экранирование для Rust.
fn emit(toolsets: &[Toolset], operations: &[Operation]) -> String {
    let mut out = String::new();
    out.push_str("// Сгенерировано build.rs из spec/wiki.openapi.json. Не редактировать.\n");
    out.push_str("pub static TOOLSETS: &[ToolsetDef] = &[\n");
    for toolset in toolsets {
        let _ = writeln!(
            out,
            "    ToolsetDef {{ slug: {:?}, description: {:?}, default_enabled: {} }},",
            toolset.slug, toolset.description, toolset.default_enabled
        );
    }
    out.push_str("];\n\n");
    out.push_str("pub static OPERATIONS: &[OperationDef] = &[\n");
    for op in operations {
        let _ = writeln!(out, "    OperationDef {{");
        let _ = writeln!(out, "        tool_name: {:?},", op.tool_name);
        let _ = writeln!(
            out,
            "        upstream_operation_id: {},",
            rust_option(op.upstream_operation_id.as_ref())
        );
        let _ = writeln!(out, "        toolset: {:?},", op.toolset);
        let _ = writeln!(out, "        summary: {:?},", op.summary);
        let _ = writeln!(out, "        description: {:?},", op.description);
        let _ = writeln!(
            out,
            "        docs_url: {},",
            rust_option(op.docs_url.as_ref())
        );
        let _ = writeln!(out, "        method: {},", rust_method(op.method));
        let _ = writeln!(out, "        path: {:?},", op.path);
        let _ = writeln!(out, "        access: {},", op.access);
        let _ = writeln!(out, "        params: &[");
        for param in &op.params {
            let _ = writeln!(
                out,
                "            ParamDef {{ name: {:?}, location: {}, required: {}, description: {:?}, value_type: ParamType::{} }},",
                param.name,
                param.location.rust_name(),
                param.required,
                param.description,
                param.value_type,
            );
        }
        let _ = writeln!(out, "        ],");
        let body = op.body.map_or_else(
            || "None".to_owned(),
            |kind| format!("Some({})", kind.rust_name()),
        );
        let _ = writeln!(out, "        body: {body},");
        let _ = writeln!(out, "        response: {},", op.response.rust_name());
        let _ = writeln!(out, "        input_schema: {:?},", op.input_schema);
        let _ = writeln!(out, "    }},");
    }
    out.push_str("];\n");
    out
}
