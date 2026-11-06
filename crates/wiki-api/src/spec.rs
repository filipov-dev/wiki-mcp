//! Описание операций Wiki API.
//!
//! Таблицы [`operations`] и [`toolsets`] генерируются `build.rs` из
//! `spec/wiki.openapi.json`. Типы этого модуля — контракт между генератором,
//! исполнителем запросов и MCP-сервером.
//!
//! Как MCP-сервер строит запрос из аргументов инструмента:
//!
//! 1. Проверить, что переданы все параметры с `required: true`.
//! 2. Подставить [`ParamLocation::Path`] в шаблон [`OperationDef::path`] (значения
//!    кодируются как сегмент пути), [`ParamLocation::Query`] передать в строке запроса
//!    (массивы — повторением ключа). Разбивка выдачи Wiki — обычные query-параметры
//!    `cursor` и `page_size`, курсоры следующей страницы приходят в теле ответа.
//! 3. Тело выбрать по [`OperationDef::body`]: для [`BodyKind::Json`] аргумент `body`
//!    отправляется JSON-телом ([`crate::executor::RequestBody::Json`]), для
//!    [`BodyKind::Binary`] аргумент `file` ([`ParamType::File`]) — путь к локальному
//!    файлу, содержимое которого уходит сырым телом
//!    ([`crate::executor::RequestBody::File`]).
//! 4. Ответ разобрать по [`OperationDef::response`]: JSON или содержимое файла, которое
//!    сохраняется на диск (`save_to`).
//!
//! Заголовки `Authorization`, `X-Org-Id` и `X-Cloud-Org-Id` в параметры не входят:
//! их добавляет исполнитель.

pub use crate::executor::HttpMethod;

/// Уровень доступа операции (`x-access-level` спецификации).
/// Уровни упорядочены: `Read < Write < Destructive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccessLevel {
    Read,
    Write,
    Destructive,
}

impl AccessLevel {
    /// Разрешает ли текущий уровень операцию с уровнем `required`.
    pub fn allows(self, required: AccessLevel) -> bool {
        required <= self
    }

    /// Значение `x-access-level`: `read`, `write` или `destructive`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Destructive => "destructive",
        }
    }
}

/// Где передаётся параметр операции.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamLocation {
    /// Подставляется в шаблон пути (`{name}`).
    Path,
    /// Передаётся в строке запроса.
    Query,
    /// Тело запроса; у операции не больше одного такого параметра: `body`
    /// ([`ParamType::Object`], [`BodyKind::Json`]) или `file` ([`ParamType::File`],
    /// [`BodyKind::Binary`]).
    Body,
}

/// Тип значения параметра в аргументах инструмента.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    String,
    Integer,
    Number,
    Boolean,
    /// Массив; в строке запроса передаётся повторением ключа (`ids=a&ids=b`).
    Array,
    /// JSON-объект (тело запроса).
    Object,
    /// Путь к локальному файлу, содержимое которого отправляется сырым телом запроса.
    File,
}

/// Вид тела запроса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyKind {
    /// `application/json`: аргумент `body` ([`ParamLocation::Body`]).
    Json,
    /// `application/octet-stream`: содержимое локального файла из аргумента `file`
    /// ([`ParamLocation::Body`], [`ParamType::File`]) отправляется сырым телом, например
    /// часть файла в сессии загрузки.
    Binary,
}

/// Вид успешного ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseKind {
    /// JSON (или пустой ответ, например `204`).
    Json,
    /// Содержимое файла (скачивание вложения).
    Binary,
}

/// Параметр операции.
#[derive(Debug, Clone, Copy)]
pub struct ParamDef {
    /// Имя параметра: ключ в аргументах инструмента, имя в пути или строке запроса.
    pub name: &'static str,
    pub location: ParamLocation,
    /// Обязателен ли параметр. Параметры пути обязательны всегда.
    pub required: bool,
    pub description: &'static str,
    pub value_type: ParamType,
}

/// Набор инструментов — тег спецификации из верхнеуровневого `tags`.
#[derive(Debug, Clone, Copy)]
pub struct ToolsetDef {
    /// Короткое английское имя для `--toolsets`, например `pages`; оно же тег операций.
    pub slug: &'static str,
    /// Описание набора из спецификации.
    pub description: &'static str,
    /// Включён ли набор, если `--toolsets` не задан (`x-default-enabled`).
    pub default_enabled: bool,
}

/// Операция Wiki API, доступная как MCP-инструмент.
#[derive(Debug, Clone, Copy)]
pub struct OperationDef {
    /// Имя MCP-инструмента — `operationId` объединённой спецификации, уникально.
    pub tool_name: &'static str,
    /// `operationId` официальной спецификации (`x-upstream-operation-id`), если есть.
    pub upstream_operation_id: Option<&'static str>,
    /// Slug набора инструментов ([`ToolsetDef::slug`]).
    pub toolset: &'static str,
    pub summary: &'static str,
    pub description: &'static str,
    /// Страница справочника с описанием операции (`externalDocs.url`), если есть.
    pub docs_url: Option<&'static str>,
    pub method: HttpMethod,
    /// Шаблон пути с версией API, например `/v1/pages/{idx}`.
    pub path: &'static str,
    pub access: AccessLevel,
    pub params: &'static [ParamDef],
    /// Вид тела запроса; `None` — тела нет.
    pub body: Option<BodyKind>,
    pub response: ResponseKind,
    /// JSON Schema аргументов инструмента в виде JSON-текста.
    ///
    /// Объект с `type: object` и `additionalProperties: false`: параметры пути и запроса и
    /// `body` (JSON-тело с раскрытыми `$ref`) или `file` (путь к файлу для сырого тела).
    /// Заголовков авторизации и организации в схеме нет.
    pub input_schema: &'static str,
}

impl OperationDef {
    /// Параметры в расположении `location`.
    pub fn params_in(&self, location: ParamLocation) -> impl Iterator<Item = &'static ParamDef> {
        self.params
            .iter()
            .filter(move |param| param.location == location)
    }

    /// Параметр по имени.
    pub fn param(&self, name: &str) -> Option<&'static ParamDef> {
        self.params.iter().find(|param| param.name == name)
    }

    /// Описание MCP-инструмента из спецификации: `summary`, `description` (если отличается)
    /// и ссылка на справочник (`externalDocs.url`), если она есть.
    ///
    /// Ядро MCP дополняет его своими пояснениями (файлы, `save_to`, уровень доступа).
    pub fn tool_description(&self) -> String {
        let summary = self.summary.trim();
        let mut description = summary.to_owned();
        let details = self.description.trim();
        if !details.is_empty() && details != summary {
            if !description.is_empty() {
                description.push_str("\n\n");
            }
            description.push_str(details);
        }
        if let Some(url) = self.docs_url {
            if !description.is_empty() {
                description.push_str("\n\n");
            }
            description.push_str("Reference: ");
            description.push_str(url);
        }
        description
    }
}

include!(concat!(env!("OUT_DIR"), "/operations.rs"));

/// Все операции API, отсортированные по имени инструмента.
pub fn operations() -> &'static [OperationDef] {
    OPERATIONS
}

/// Операция по имени MCP-инструмента.
pub fn find(tool_name: &str) -> Option<&'static OperationDef> {
    OPERATIONS
        .binary_search_by(|op| op.tool_name.cmp(tool_name))
        .ok()
        .and_then(|index| OPERATIONS.get(index))
}

/// Наборы инструментов в порядке `tags` спецификации.
pub fn toolsets() -> &'static [ToolsetDef] {
    TOOLSETS
}

/// Набор инструментов по slug.
pub fn find_toolset(slug: &str) -> Option<&'static ToolsetDef> {
    TOOLSETS.iter().find(|toolset| toolset.slug == slug)
}

/// Наборы, включённые по умолчанию (`x-default-enabled: true`), в порядке `tags`.
pub fn default_toolsets() -> impl Iterator<Item = &'static ToolsetDef> {
    TOOLSETS.iter().filter(|toolset| toolset.default_enabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_levels_are_ordered() {
        assert!(AccessLevel::Destructive.allows(AccessLevel::Write));
        assert!(AccessLevel::Write.allows(AccessLevel::Read));
        assert!(!AccessLevel::Read.allows(AccessLevel::Write));
        assert!(!AccessLevel::Write.allows(AccessLevel::Destructive));
    }

    #[test]
    fn find_by_name() {
        let op = find("get_page_by_id").expect("операция get_page_by_id");
        assert_eq!(op.path, "/v1/pages/{idx}");
        assert_eq!(
            op.param("idx").map(|p| p.location),
            Some(ParamLocation::Path)
        );
        assert!(find("no_such_tool").is_none());
        assert!(find_toolset("pages").is_some_and(|t| t.default_enabled));
        assert!(find_toolset("no_such_toolset").is_none());
    }

    #[test]
    fn tool_description_includes_reference() {
        let op = OperationDef {
            docs_url: Some("https://example.test/docs/op"),
            summary: "Summary",
            description: "Details",
            ..*find("get_page_by_id").expect("операция get_page_by_id")
        };
        assert_eq!(
            op.tool_description(),
            "Summary\n\nDetails\n\nReference: https://example.test/docs/op"
        );
        let op = OperationDef {
            docs_url: None,
            description: "Summary",
            ..op
        };
        assert_eq!(op.tool_description(), "Summary");
    }
}
