//! Реестр MCP-инструментов сервера.
//!
//! Строится один раз при запуске из `wiki_api::spec::operations()` с учётом
//! [`ServerOptions`]: инструменты выше `max_access` и вне включённых наборов не регистрируются,
//! поэтому не видны в `tools/list` и не вызываются. Для незарегистрированных, но известных
//! инструментов [`ToolRegistry::unavailable_reason`] объясняет, какой флаг их включает.
//!
//! Без [`ServerOptions::local_machine`] (HTTP-режим) не регистрируются операции с локальными
//! файлами: сырое тело из файла (загрузка части файла) и двоичный ответ (скачивание вложений).
//!
//! Схема аргументов инструмента — `input_schema` операции; у операций с двоичным ответом
//! добавляется обязательный аргумент `save_to` (куда сохранить файл). Описание —
//! [`OperationDef::tool_description`], пояснения про файлы и уровень доступа.
//!
//! Составные инструменты ([`crate::composite`]) регистрируются после операций по тем же
//! правилам: уровень доступа, набор и локальные файлы.
//!
//! Встроенные инструменты [`crate::API_REQUEST_TOOL`] и [`crate::API_SEARCH_TOOL`] регистрируются всегда:
//! первый сам проверяет уровень доступа каждого запроса, второй только читает таблицу операций.

use std::collections::HashMap;
use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Value, json};
use wiki_api::spec::{AccessLevel, HttpMethod, OperationDef, ParamType, operations};

use crate::ServerOptions;
use crate::api_request::api_request_tool;
use crate::api_search::api_search_tool;
use crate::composite::Composite;
use crate::request::{SAVE_TO_ARG, takes_save_to};
use crate::toolsets::effective_toolsets;

/// Что выполняет инструмент.
#[derive(Debug, Clone, Copy)]
pub enum ToolKind {
    /// Сгенерированная операция Wiki API.
    Operation(&'static OperationDef),
    /// Составной инструмент: несколько вызовов API ([`crate::composite`]).
    Composite(Composite),
    /// Запрос к произвольному пути API ([`crate::API_REQUEST_TOOL`]).
    ApiRequest,
    /// Поиск по операциям API ([`crate::API_SEARCH_TOOL`]).
    ApiSearch,
}

/// Зарегистрированный инструмент: описание для `tools/list` и обработчик.
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub tool: Tool,
    pub kind: ToolKind,
    /// Уровень доступа инструмента.
    pub access: AccessLevel,
}

/// Набор инструментов, доступных при заданных [`ServerOptions`].
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    index: HashMap<String, usize>,
    /// Включённые наборы (slug).
    toolsets: Vec<String>,
    max_access: AccessLevel,
    /// Инструменты могут работать с локальными файлами ([`ServerOptions::local_machine`]).
    local_files: bool,
}

/// Работает ли инструмент операции с локальными файлами: сырое тело из файла или
/// сохранение двоичного ответа.
pub fn uses_local_files(op: &OperationDef) -> bool {
    takes_save_to(op)
        || op
            .params
            .iter()
            .any(|param| param.value_type == ParamType::File)
}

impl ToolRegistry {
    /// Строит реестр. Ошибка — неизвестный набор инструментов или некорректная схема
    /// в сгенерированной таблице (последнее — ошибка генератора).
    pub fn new(options: &ServerOptions) -> Result<Self, String> {
        let selected = match &options.toolsets {
            Some(names) => Some(crate::toolsets::normalize_toolsets(names)?),
            None => None,
        };
        let mut registry = Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets: effective_toolsets(selected.as_deref()),
            max_access: options.max_access,
            local_files: options.local_machine,
        };
        for op in operations() {
            if registry.operation_enabled(op) {
                let tool = operation_tool(op)?;
                registry.push(RegisteredTool {
                    tool,
                    kind: ToolKind::Operation(op),
                    access: op.access,
                })?;
            }
        }
        for composite in Composite::ALL {
            if registry.composite_enabled(composite) {
                registry.push(RegisteredTool {
                    tool: composite.tool(),
                    kind: ToolKind::Composite(composite),
                    access: composite.access(),
                })?;
            }
        }
        registry.push(RegisteredTool {
            tool: api_request_tool(options.max_access, options.local_machine),
            kind: ToolKind::ApiRequest,
            access: options.max_access,
        })?;
        registry.push(RegisteredTool {
            tool: api_search_tool(),
            kind: ToolKind::ApiSearch,
            access: AccessLevel::Read,
        })?;
        Ok(registry)
    }

    fn operation_enabled(&self, op: &OperationDef) -> bool {
        self.max_access.allows(op.access)
            && self.toolset_enabled(op.toolset)
            && (self.local_files || !uses_local_files(op))
    }

    fn composite_enabled(&self, composite: Composite) -> bool {
        self.max_access.allows(composite.access())
            && self.toolset_enabled(composite.toolset())
            && (self.local_files || !composite.uses_local_files())
    }

    /// Включён ли набор `slug`.
    pub fn toolset_enabled(&self, slug: &str) -> bool {
        self.toolsets.iter().any(|name| name == slug)
    }

    /// Включённые наборы (slug).
    pub fn enabled_toolsets(&self) -> &[String] {
        &self.toolsets
    }

    /// Могут ли инструменты работать с локальными файлами (см. [`ServerOptions::local_machine`]).
    pub fn local_files(&self) -> bool {
        self.local_files
    }

    /// Максимальный уровень доступа сервера.
    pub fn max_access(&self) -> AccessLevel {
        self.max_access
    }

    fn push(&mut self, tool: RegisteredTool) -> Result<(), String> {
        let name = tool.tool.name.to_string();
        if self.index.contains_key(&name) {
            return Err(format!("duplicate tool name `{name}`"));
        }
        self.index.insert(name, self.tools.len());
        self.tools.push(tool);
        Ok(())
    }

    /// Зарегистрированные инструменты в порядке регистрации: операции по имени, затем
    /// составные, затем встроенные.
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredTool> {
        self.tools.iter()
    }

    /// Описания инструментов для `tools/list`.
    pub fn tools(&self) -> Vec<Tool> {
        self.tools.iter().map(|t| t.tool.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.index.get(name).and_then(|&i| self.tools.get(i))
    }

    /// Почему известный инструмент не зарегистрирован; `None` — инструмент неизвестен или
    /// зарегистрирован.
    pub fn unavailable_reason(&self, name: &str) -> Option<String> {
        if self.get(name).is_some() {
            return None;
        }
        let (access, toolset, local_files) = match Composite::find(name) {
            Some(composite) => (
                composite.access(),
                composite.toolset(),
                composite.uses_local_files(),
            ),
            None => {
                let op = wiki_api::spec::find(name)?;
                (op.access, op.toolset, uses_local_files(op))
            }
        };
        if !self.local_files && local_files {
            return Some(format!(
                "tool `{name}` is not available on this server: it reads or writes a local file, \
                 and this server runs on another machine (HTTP mode) and cannot access the \
                 agent's files. Ask the user to upload or download the file in Wiki, or to use \
                 a local (stdio) `wiki-mcp serve`"
            ));
        }
        if !self.max_access.allows(access) {
            return Some(format!(
                "tool `{name}` is disabled: it {} data in Wiki and the server runs with {} \
                 access. Ask the user to restart the server with {} if this change is intended",
                access_verb(access),
                access_name(self.max_access),
                access_flag(access),
            ));
        }
        if !self.toolset_enabled(toolset) {
            return Some(format!(
                "tool `{name}` belongs to toolset `{toolset}`, which is not enabled; ask the user \
                 to add it to --toolsets (enabled: {})",
                self.toolsets.join(", ")
            ));
        }
        None
    }
}

/// Описание MCP-инструмента для операции.
///
/// Схема — `input_schema` операции (у операций с двоичным ответом — плюс `save_to`),
/// описание — [`OperationDef::tool_description`] (`summary`, `description`, ссылка на
/// справочник), пояснения про локальные файлы и уровень доступа.
pub fn operation_tool(op: &'static OperationDef) -> Result<Tool, String> {
    let mut schema: JsonObject = serde_json::from_str(op.input_schema)
        .map_err(|err| format!("{}: invalid input schema: {err}", op.tool_name))?;
    if takes_save_to(op) {
        add_save_to(op, &mut schema)?;
    }

    let mut description = op.tool_description();
    if op.params.iter().any(|p| p.value_type == ParamType::File) {
        description.push_str(
            "\n\nFile arguments take a path to a local file (absolute, or relative to the \
             server's working directory) inside the directories allowed by the server (see the \
             server instructions).",
        );
    }
    if takes_save_to(op) {
        description.push_str(
            "\n\nThe response is a file: it is saved to `save_to` (a new file inside the \
             allowed directories) and the result reports its path and size.",
        );
    }
    if !description.is_empty() {
        description.push_str("\n\n");
    }
    description.push_str("Access: ");
    description.push_str(match op.access {
        AccessLevel::Read => "read-only.",
        AccessLevel::Write => "write (creates or changes data).",
        AccessLevel::Destructive => {
            "destructive (deletes data, changes access rights or makes irreversible changes)."
        }
    });

    let summary = op.summary.trim();
    let mut tool = Tool::new(op.tool_name, description, Arc::new(schema));
    if !summary.is_empty() {
        tool.title = Some(summary.to_owned());
    }
    tool.annotations = Some(annotations(op));
    Ok(tool)
}

/// Аннотации по уровню доступа. `idempotentHint` имеет смысл только для изменяющих
/// инструментов: `PUT` и `DELETE` идемпотентны, `POST` и `PATCH` — нет (в Wiki `POST`
/// и создаёт, и меняет, и дописывает текст).
fn annotations(op: &OperationDef) -> ToolAnnotations {
    let summary = op.summary.trim();
    let base = if summary.is_empty() {
        ToolAnnotations::new()
    } else {
        ToolAnnotations::with_title(summary)
    }
    .open_world(true);
    let idempotent = matches!(op.method, HttpMethod::Put | HttpMethod::Delete);
    match op.access {
        AccessLevel::Read => base.read_only(true),
        AccessLevel::Write => base
            .read_only(false)
            .destructive(false)
            .idempotent(idempotent),
        AccessLevel::Destructive => base
            .read_only(false)
            .destructive(true)
            .idempotent(idempotent),
    }
}

/// Добавляет обязательный аргумент `save_to` в схему операции с двоичным ответом.
fn add_save_to(op: &OperationDef, schema: &mut JsonObject) -> Result<(), String> {
    let properties = schema
        .entry("properties")
        .or_insert_with(|| Value::Object(JsonObject::new()));
    let Value::Object(properties) = properties else {
        return Err(format!("{}: `properties` is not an object", op.tool_name));
    };
    if properties.contains_key(SAVE_TO_ARG) {
        return Err(format!(
            "{}: parameter `{SAVE_TO_ARG}` clashes with the file download argument",
            op.tool_name
        ));
    }
    properties.insert(
        SAVE_TO_ARG.to_owned(),
        json!({
            "type": "string",
            "description": "Path of a new local file to save the downloaded content to \
                (absolute, or relative to the server's working directory). The directory must \
                exist and be inside the directories allowed by the server; existing files are \
                never overwritten."
        }),
    );
    let required = schema
        .entry("required")
        .or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(required) = required else {
        return Err(format!("{}: `required` is not an array", op.tool_name));
    };
    required.push(Value::String(SAVE_TO_ARG.to_owned()));
    Ok(())
}

fn access_verb(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "reads",
        AccessLevel::Write => "creates or changes",
        AccessLevel::Destructive => "deletes or irreversibly changes",
    }
}

/// Название уровня доступа для сообщений и инструкций.
pub fn access_name(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read-only",
        AccessLevel::Write => "write",
        AccessLevel::Destructive => "destructive",
    }
}

/// Флаг сервера, который включает уровень доступа `access`.
pub fn access_flag(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "no extra flag",
        AccessLevel::Write => "--allow-write",
        AccessLevel::Destructive => "--allow-destructive",
    }
}

#[cfg(test)]
mod tests {
    use wiki_api::spec::toolsets;

    use super::*;
    use crate::composite::{UPLOAD_ATTACHMENT_TOOL, WAIT_FOR_OPERATION_TOOL};
    use crate::toolsets::default_toolsets;
    use crate::{API_REQUEST_TOOL, API_SEARCH_TOOL};

    fn options(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ServerOptions {
        ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            ..ServerOptions::default()
        }
    }

    fn all() -> Option<&'static [&'static str]> {
        Some(&["all"])
    }

    /// Встроенные инструменты: `api_request` и `api_search`.
    const BUILTIN: usize = 2;

    /// Операции и составные инструменты уровня и наборов, доступные этому реестру
    /// (локальному), и встроенные инструменты.
    fn expected(access: AccessLevel, toolsets: &[&str]) -> usize {
        operations()
            .iter()
            .filter(|op| access.allows(op.access) && toolsets.contains(&op.toolset))
            .count()
            + Composite::ALL
                .iter()
                .filter(|c| access.allows(c.access()) && toolsets.contains(&c.toolset()))
                .count()
            + BUILTIN
    }

    fn all_slugs() -> Vec<&'static str> {
        toolsets().iter().map(|t| t.slug).collect()
    }

    #[test]
    fn schemas_match_operations() {
        assert!(!operations().is_empty());
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            assert_eq!(tool.name, op.tool_name);
            let expected: Value = serde_json::from_str(op.input_schema).unwrap();
            let mut actual = Value::Object((*tool.input_schema).clone());
            if takes_save_to(op) {
                // Единственное отличие от спецификации — аргумент `save_to`.
                let props = actual["properties"].as_object_mut().unwrap();
                assert!(props.remove(SAVE_TO_ARG).is_some(), "{}", op.tool_name);
                let required = actual["required"].as_array_mut().unwrap();
                assert_eq!(required.pop(), Some(json!(SAVE_TO_ARG)), "{}", op.tool_name);
            }
            assert_eq!(actual, expected, "{}", op.tool_name);
            assert_eq!(actual["type"], "object", "{}", op.tool_name);
            assert_eq!(actual["additionalProperties"], false, "{}", op.tool_name);

            let description = tool.description.as_deref().unwrap();
            assert!(
                description.starts_with(&op.tool_description()),
                "{}",
                op.tool_name
            );
            assert!(description.contains("Access: "), "{}", op.tool_name);
            if let Some(url) = op.docs_url {
                assert!(description.contains(url), "{}", op.tool_name);
            }
            assert_eq!(
                tool.title.as_deref(),
                Some(op.summary.trim()),
                "{}",
                op.tool_name
            );
        }
    }

    #[test]
    fn annotations_follow_access_level() {
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            let a = tool.annotations.as_ref().unwrap();
            assert_eq!(
                a.title.as_deref(),
                Some(op.summary.trim()),
                "{}",
                op.tool_name
            );
            assert_eq!(a.open_world_hint, Some(true));
            match op.access {
                AccessLevel::Read => {
                    assert_eq!(a.read_only_hint, Some(true), "{}", op.tool_name);
                    assert_eq!(a.destructive_hint, None, "{}", op.tool_name);
                }
                AccessLevel::Write | AccessLevel::Destructive => {
                    assert_eq!(a.read_only_hint, Some(false), "{}", op.tool_name);
                    assert_eq!(
                        a.destructive_hint,
                        Some(op.access == AccessLevel::Destructive),
                        "{}",
                        op.tool_name
                    );
                    assert_eq!(
                        a.idempotent_hint,
                        Some(matches!(op.method, HttpMethod::Put | HttpMethod::Delete)),
                        "{}",
                        op.tool_name
                    );
                }
            }
        }
    }

    #[test]
    fn access_filtering() {
        let slugs = all_slugs();
        let read = ToolRegistry::new(&options(AccessLevel::Read, all())).unwrap();
        assert_eq!(read.len(), expected(AccessLevel::Read, &slugs));
        assert!(read.iter().all(|t| t.access == AccessLevel::Read));
        assert!(read.get(API_REQUEST_TOOL).is_some());
        assert!(read.get(API_SEARCH_TOOL).is_some());
        assert!(read.get("get_page").is_some());
        assert!(read.get("search").is_some(), "чтение через POST");
        assert!(
            read.get("suggest_grid_column").is_some(),
            "чтение через POST"
        );
        assert!(read.get("create_page").is_none());
        assert!(read.get("delete_page").is_none());

        let write = ToolRegistry::new(&options(AccessLevel::Write, all())).unwrap();
        assert_eq!(write.len(), expected(AccessLevel::Write, &slugs));
        assert!(write.get("create_page").is_some());
        assert!(write.get("update_grid_cells").is_some());
        assert!(write.get("delete_page").is_none());
        // Изменение прав доступа — уровень удаления.
        assert!(write.get("create_page_access").is_none());

        let destructive = ToolRegistry::new(&options(AccessLevel::Destructive, all())).unwrap();
        assert_eq!(
            destructive.len(),
            expected(AccessLevel::Destructive, &slugs)
        );
        let delete = destructive.get("delete_page").unwrap();
        assert_eq!(delete.access, AccessLevel::Destructive);
        assert!(destructive.get("create_page_access").is_some());
        assert!(read.len() < write.len() && write.len() < destructive.len());
    }

    #[test]
    fn toolset_filtering() {
        let default = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        assert_eq!(default.enabled_toolsets(), default_toolsets());
        assert_eq!(
            default.len(),
            expected(AccessLevel::Destructive, &default_toolsets())
        );
        // Наборы `access` и `extra` по умолчанию выключены.
        assert!(default.get("create_page_access").is_none());
        assert!(default.get("list_spaces").is_none());
        // Пустой список — тоже наборы по умолчанию.
        let empty = ToolRegistry::new(&options(AccessLevel::Destructive, Some(&[]))).unwrap();
        assert_eq!(empty.len(), default.len());

        let chosen =
            ToolRegistry::new(&options(AccessLevel::Read, Some(&["grids", " extra "]))).unwrap();
        assert!(!chosen.is_empty());
        assert!(chosen.iter().all(|t| match t.kind {
            ToolKind::Operation(op) => op.toolset == "grids" || op.toolset == "extra",
            ToolKind::Composite(c) => c.toolset() == "grids" || c.toolset() == "extra",
            ToolKind::ApiRequest | ToolKind::ApiSearch => true,
        }));
        // Встроенные инструменты не зависят от наборов.
        assert!(chosen.get(API_REQUEST_TOOL).is_some());
        assert!(chosen.get(API_SEARCH_TOOL).is_some());
        assert!(chosen.get("get_page").is_none());
        assert!(chosen.get("get_grid").is_some());
        assert!(chosen.get("list_spaces").is_some());

        let err = ToolRegistry::new(&options(AccessLevel::Read, Some(&["Таблицы"]))).unwrap_err();
        assert!(err.contains("unknown toolset `Таблицы`"), "{err}");
    }

    #[test]
    fn unavailable_reasons_name_the_flag() {
        let read = ToolRegistry::new(&options(AccessLevel::Read, None)).unwrap();
        let reason = read.unavailable_reason("create_page").unwrap();
        assert!(reason.contains("--allow-write"), "{reason}");
        let reason = read.unavailable_reason("delete_page").unwrap();
        assert!(reason.contains("--allow-destructive"), "{reason}");
        assert!(read.unavailable_reason("get_page").is_none());
        assert!(read.unavailable_reason("no_such_tool").is_none());

        let write = ToolRegistry::new(&options(AccessLevel::Write, None)).unwrap();
        let reason = write.unavailable_reason("delete_page").unwrap();
        assert!(reason.contains("--allow-destructive") && reason.contains("write access"));

        let destructive = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        let reason = destructive
            .unavailable_reason("create_page_access")
            .unwrap();
        assert!(reason.contains("toolset `access`"), "{reason}");
        assert!(reason.contains("--toolsets"), "{reason}");
    }

    #[test]
    fn composite_tools_follow_access_and_toolsets() {
        let read = ToolRegistry::new(&options(AccessLevel::Read, None)).unwrap();
        assert!(read.get(UPLOAD_ATTACHMENT_TOOL).is_none());
        let reason = read.unavailable_reason(UPLOAD_ATTACHMENT_TOOL).unwrap();
        assert!(reason.contains("--allow-write"), "{reason}");
        let wait = read.get(WAIT_FOR_OPERATION_TOOL).unwrap();
        assert_eq!(wait.access, AccessLevel::Read);
        assert!(matches!(wait.kind, ToolKind::Composite(_)));
        let a = wait.tool.annotations.as_ref().unwrap();
        assert_eq!(a.read_only_hint, Some(true));

        let write = ToolRegistry::new(&options(AccessLevel::Write, None)).unwrap();
        let upload = write.get(UPLOAD_ATTACHMENT_TOOL).unwrap();
        assert_eq!(upload.access, AccessLevel::Write);
        let a = upload.tool.annotations.as_ref().unwrap();
        assert_eq!(a.read_only_hint, Some(false));
        assert_eq!(a.destructive_hint, Some(false));
        assert_eq!(a.idempotent_hint, Some(false));

        // Наборы: загрузка — во вложениях, ожидание — в страницах.
        let grids = ToolRegistry::new(&options(AccessLevel::Write, Some(&["grids"]))).unwrap();
        assert!(grids.get(UPLOAD_ATTACHMENT_TOOL).is_none());
        assert!(grids.get(WAIT_FOR_OPERATION_TOOL).is_none());
        let reason = grids.unavailable_reason(WAIT_FOR_OPERATION_TOOL).unwrap();
        assert!(reason.contains("toolset `pages`"), "{reason}");
        let reason = grids.unavailable_reason(UPLOAD_ATTACHMENT_TOOL).unwrap();
        assert!(reason.contains("toolset `attachments`"), "{reason}");
        let attachments =
            ToolRegistry::new(&options(AccessLevel::Write, Some(&["attachments"]))).unwrap();
        assert!(attachments.get(UPLOAD_ATTACHMENT_TOOL).is_some());
        assert!(attachments.get(WAIT_FOR_OPERATION_TOOL).is_none());
        let pages = ToolRegistry::new(&options(AccessLevel::Read, Some(&["pages"]))).unwrap();
        assert!(pages.get(WAIT_FOR_OPERATION_TOOL).is_some());
    }

    #[test]
    fn builtin_tools_follow_access_level() {
        for access in [
            AccessLevel::Read,
            AccessLevel::Write,
            AccessLevel::Destructive,
        ] {
            let registry = ToolRegistry::new(&options(access, None)).unwrap();
            let request = registry.get(API_REQUEST_TOOL).unwrap();
            assert_eq!(request.access, access);
            let a = request.tool.annotations.as_ref().unwrap();
            assert_eq!(a.read_only_hint, Some(access == AccessLevel::Read));
            let search = registry.get(API_SEARCH_TOOL).unwrap();
            assert_eq!(search.access, AccessLevel::Read);
        }
        // Имена встроенных инструментов не совпадают с операциями.
        for name in [API_REQUEST_TOOL, API_SEARCH_TOOL] {
            assert!(wiki_api::spec::find(name).is_none(), "{name}");
        }
    }

    #[test]
    fn file_tools_describe_local_files() {
        let upload = operation_tool(wiki_api::spec::find("upload_file_part").unwrap()).unwrap();
        let description = upload.description.as_deref().unwrap();
        assert!(
            description.contains("File arguments take a path"),
            "{description}"
        );
        assert!(upload.input_schema["properties"].get(SAVE_TO_ARG).is_none());
        let download =
            operation_tool(wiki_api::spec::find("download_attachment").unwrap()).unwrap();
        let description = download.description.as_deref().unwrap();
        assert!(description.contains("saved to `save_to`"), "{description}");
        assert_eq!(
            download.input_schema["properties"][SAVE_TO_ARG]["type"],
            "string"
        );
    }

    #[test]
    fn remote_server_has_no_local_file_tools() {
        let remote = ToolRegistry::new(&ServerOptions {
            local_machine: false,
            ..options(AccessLevel::Destructive, all())
        })
        .unwrap();
        assert!(!remote.local_files());
        let file_tools: Vec<&str> = operations()
            .iter()
            .filter(|op| uses_local_files(op))
            .map(|op| op.tool_name)
            .collect();
        for name in [
            "upload_file_part",
            "download_attachment",
            "download_attachment_by_url",
            "download_attachment_preview",
        ] {
            assert!(file_tools.contains(&name), "{name}");
        }
        assert_eq!(file_tools.len(), 4, "{file_tools:?}");
        for name in &file_tools {
            assert!(remote.get(name).is_none(), "{name}");
            let reason = remote.unavailable_reason(name).unwrap();
            assert!(reason.contains("HTTP mode"), "{reason}");
        }
        // Загрузка файла — тоже работа с локальными файлами; ожидание операций остаётся.
        assert!(remote.get(UPLOAD_ATTACHMENT_TOOL).is_none());
        let reason = remote.unavailable_reason(UPLOAD_ATTACHMENT_TOOL).unwrap();
        assert!(reason.contains("HTTP mode"), "{reason}");
        assert!(remote.get(WAIT_FOR_OPERATION_TOOL).is_some());
        assert_eq!(
            remote.len(),
            operations().len() - file_tools.len() + 1 + BUILTIN
        );
        for tool in remote.iter() {
            let description = tool.tool.description.as_deref().unwrap_or_default();
            assert!(!description.contains("local file"), "{}", tool.tool.name);
            assert!(
                tool.tool.input_schema["properties"]
                    .get(SAVE_TO_ARG)
                    .is_none(),
                "{}",
                tool.tool.name
            );
        }

        let local = ToolRegistry::new(&options(AccessLevel::Destructive, all())).unwrap();
        assert!(local.local_files());
        assert!(local.get(UPLOAD_ATTACHMENT_TOOL).is_some());
        assert!(file_tools.iter().all(|name| local.get(name).is_some()));
    }
}
