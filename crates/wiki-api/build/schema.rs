//! Перевод схем OpenAPI 3.1 в самодостаточную JSON Schema для аргументов MCP-инструмента.
//!
//! `$ref` раскрываются на месте: MCP-клиенты не получают `components`, поэтому ссылки
//! в схеме инструмента были бы неразрешимы. Переносятся только ключевые слова JSON Schema;
//! `title`, `example`, `examples` и расширения `x-*` отбрасываются.
//!
//! Официальная спецификация Wiki API сгенерирована из моделей Pydantic: `title` там —
//! имя поля или модели (`Page Size`, `CreatePageSchema`) и агенту ничего не добавляет,
//! поэтому описанием служит только `description`.

use serde_json::{Map, Value, json};

/// Максимальная вложенность раскрытия `$ref`. Глубже схема заменяется разрешающим объектом.
const MAX_REF_DEPTH: usize = 8;

/// Ключи схемы, которые переносятся как есть (без рекурсии).
const PLAIN_KEYS: &[&str] = &[
    "type",
    "format",
    "enum",
    "const",
    "default",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minLength",
    "maxLength",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
];

pub struct Resolver<'a> {
    spec: &'a Value,
    /// Цепочка раскрываемых ссылок — защита от рекурсивных схем.
    stack: Vec<String>,
}

impl<'a> Resolver<'a> {
    pub fn new(spec: &'a Value) -> Self {
        Self {
            spec,
            stack: Vec::new(),
        }
    }

    /// Раскрывает схему в JSON Schema без `$ref`.
    pub fn resolve(&mut self, schema: &Value) -> Value {
        let Some(object) = schema.as_object() else {
            return schema.clone();
        };

        if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
            // Описание рядом с `$ref` относится к конкретному полю и точнее описания типа.
            let local_description = description_of(object);
            let mut resolved = self.resolve_ref(reference);
            if let (Some(text), Some(target)) = (local_description, resolved.as_object_mut()) {
                target.insert("description".into(), Value::String(text));
            }
            return resolved;
        }

        let mut out = Map::new();
        for key in PLAIN_KEYS {
            if let Some(value) = object.get(*key) {
                out.insert((*key).into(), value.clone());
            }
        }
        if let Some(format) = out.get_mut("format") {
            *format = normalize_format(format);
        }
        if let Some(text) = description_of(object) {
            out.insert("description".into(), Value::String(text));
        }
        // Пустой `required` допустим не во всех черновиках JSON Schema — не переносим.
        if let Some(required) = object.get("required").and_then(Value::as_array)
            && !required.is_empty()
        {
            out.insert("required".into(), Value::Array(required.clone()));
        }
        if let Some(items) = object.get("items") {
            out.insert("items".into(), self.resolve(items));
        } else if is_array_type(object.get("type")) {
            // Справочник не всегда описывает элементы массива. Часть клиентов (API OpenAI,
            // на котором работает Codex) отклоняет массив без `items`: разрешаем любые элементы.
            out.insert("items".into(), json!({}));
        }
        if let Some(properties) = object.get("properties").and_then(Value::as_object) {
            let resolved: Map<String, Value> = properties
                .iter()
                .map(|(name, property)| (name.clone(), self.resolve(property)))
                .collect();
            out.insert("properties".into(), Value::Object(resolved));
        }
        if let Some(additional) = object.get("additionalProperties") {
            let value = if additional.is_object() {
                self.resolve(additional)
            } else {
                additional.clone()
            };
            out.insert("additionalProperties".into(), value);
        }
        for key in ["allOf", "anyOf", "oneOf"] {
            if let Some(variants) = object.get(key).and_then(Value::as_array) {
                let resolved = variants.iter().map(|v| self.resolve(v)).collect();
                out.insert(key.into(), Value::Array(resolved));
            }
        }
        Value::Object(out)
    }

    fn resolve_ref(&mut self, reference: &str) -> Value {
        let name = reference.rsplit('/').next().unwrap_or(reference).to_owned();
        if self.stack.iter().any(|seen| seen == reference) || self.stack.len() >= MAX_REF_DEPTH {
            // Рекурсия или слишком глубокая вложенность: структура не раскрывается,
            // агент может передать объект произвольной формы.
            return json!({
                "type": "object",
                "description": format!("`{name}` object (nested structure is not expanded)"),
            });
        }
        let target = lookup(self.spec, reference);
        self.stack.push(reference.to_owned());
        let resolved = self.resolve(target);
        self.stack.pop();
        resolved
    }
}

/// `type: array` или тип-список, в котором есть `array`.
fn is_array_type(kind: Option<&Value>) -> bool {
    match kind {
        Some(Value::String(kind)) => kind == "array",
        Some(Value::Array(kinds)) => kinds.iter().any(|k| k == "array"),
        _ => false,
    }
}

/// Объект, на который указывает локальная ссылка `#/...`.
pub fn lookup<'s>(spec: &'s Value, reference: &str) -> &'s Value {
    let pointer = reference
        .strip_prefix('#')
        .unwrap_or_else(|| panic!("неподдерживаемая ссылка {reference}: нужна локальная #/..."));
    spec.pointer(pointer)
        .unwrap_or_else(|| panic!("ссылка {reference} никуда не ведёт"))
}

/// Объект, на который указывает `$ref` значения, или само значение.
pub fn deref<'s>(spec: &'s Value, value: &'s Value) -> &'s Value {
    match value.get("$ref").and_then(Value::as_str) {
        Some(reference) => lookup(spec, reference),
        None => value,
    }
}

/// Описание схемы или параметра: непустой `description` (`title` не используется, см.
/// описание модуля).
pub fn description_of(object: &Map<String, Value>) -> Option<String> {
    object
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Формат строки в терминах JSON Schema: `uuid4` из моделей Pydantic — это `uuid`.
/// Нестандартный формат строгие валидаторы MCP-клиентов отклоняют.
pub fn normalize_format(format: &Value) -> Value {
    match format.as_str() {
        Some("uuid4") => json!("uuid"),
        _ => format.clone(),
    }
}
