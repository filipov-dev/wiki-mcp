//! Составные инструменты: несколько вызовов Wiki API за один вызов инструмента.
//!
//! - [`UPLOAD_ATTACHMENT_TOOL`] — загрузка локального файла вложением страницы
//!   ([`crate::upload`]): сессия загрузки, части, завершение, прикрепление; при ошибке после
//!   создания сессии сессия отменяется.
//! - [`WAIT_FOR_OPERATION_TOOL`] — ожидание фоновой операции (копирование страницы или
//!   таблицы, перенос страниц) с тайм-аутом ([`crate::wait`]).
//! - [`CREATE_UPLOAD_URL_TOOL`] — одноразовый адрес HTTP-сервера, на который агент сам
//!   отправляет файл; сервер загружает его вложением страницы ([`crate::upload_url`]).
//!
//! Составной инструмент регистрируется по тем же правилам, что операция: уровень доступа,
//! набор инструментов и машина сервера: загрузка локального файла — только на машине агента
//! (stdio), одноразовый адрес — только в HTTP-режиме.
//! Шаги строятся из операций спецификации (`step_operation` и
//! [`crate::request::build_request`]) и выполняются через
//! `server::execute_redacted`, поэтому пути, проверка аргументов и вырезание
//! учётных данных те же, что у одиночных инструментов. Наборы и уровни операций шагов на
//! составной инструмент не влияют: он, как `api_request`, вызывает их сам.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::Value;
use wiki_api::executor::{ApiClient, ResponseBody};
use wiki_api::spec::{AccessLevel, OperationDef};

use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::redact::Redactions;
use crate::server::execute_redacted;
use crate::{upload, upload_url, wait};

/// Имя инструмента загрузки файла вложением страницы.
pub const UPLOAD_ATTACHMENT_TOOL: &str = "upload_attachment";

/// Имя инструмента ожидания фоновой операции.
pub const WAIT_FOR_OPERATION_TOOL: &str = "wait_for_operation";

/// Имя инструмента одноразового адреса загрузки файла (HTTP-режим).
pub const CREATE_UPLOAD_URL_TOOL: &str = "create_upload_url";

/// Составной инструмент.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Composite {
    /// [`UPLOAD_ATTACHMENT_TOOL`].
    UploadAttachment,
    /// [`WAIT_FOR_OPERATION_TOOL`].
    WaitForOperation,
    /// [`CREATE_UPLOAD_URL_TOOL`].
    CreateUploadUrl,
}

impl Composite {
    /// Все составные инструменты в порядке регистрации.
    pub const ALL: [Composite; 3] = [
        Self::UploadAttachment,
        Self::WaitForOperation,
        Self::CreateUploadUrl,
    ];

    /// Составной инструмент по имени.
    pub fn find(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::UploadAttachment => UPLOAD_ATTACHMENT_TOOL,
            Self::WaitForOperation => WAIT_FOR_OPERATION_TOOL,
            Self::CreateUploadUrl => CREATE_UPLOAD_URL_TOOL,
        }
    }

    /// Уровень доступа: самый высокий из уровней шагов.
    pub fn access(self) -> AccessLevel {
        match self {
            Self::UploadAttachment | Self::CreateUploadUrl => AccessLevel::Write,
            Self::WaitForOperation => AccessLevel::Read,
        }
    }

    /// Набор инструментов: тот, где живут операции шагов (вложения; копирование страниц).
    pub fn toolset(self) -> &'static str {
        match self {
            Self::UploadAttachment | Self::CreateUploadUrl => "attachments",
            Self::WaitForOperation => "pages",
        }
    }

    /// Читает ли инструмент локальные файлы (в HTTP-режиме такие не регистрируются).
    pub fn uses_local_files(self) -> bool {
        matches!(self, Self::UploadAttachment)
    }

    /// Нужен ли инструменту HTTP-транспорт (на машине агента, в stdio, он не регистрируется:
    /// там файл загружает `upload_attachment`).
    pub fn remote_only(self) -> bool {
        matches!(self, Self::CreateUploadUrl)
    }

    /// Краткое название.
    pub fn summary(self) -> &'static str {
        match self {
            Self::UploadAttachment => "Upload a local file as a page attachment",
            Self::WaitForOperation => "Wait for a background operation",
            Self::CreateUploadUrl => "Create a one-time URL to upload a file as an attachment",
        }
    }

    /// Операции, которые вызывает инструмент.
    pub fn steps(self) -> &'static [&'static str] {
        match self {
            Self::UploadAttachment => upload::STEPS,
            Self::WaitForOperation => wait::STEPS,
            Self::CreateUploadUrl => upload_url::STEPS,
        }
    }

    /// Русские слова для `api_search`: описания инструментов на английском, а запросы
    /// агентов бывают и на русском.
    pub fn search_keywords(self) -> &'static str {
        match self {
            Self::UploadAttachment => {
                "загрузить загрузка файл файла вложение прикрепить приложить на страницу \
                 сессия загрузки части"
            }
            Self::WaitForOperation => {
                "дождаться ждать ожидание статус фоновой асинхронной операции копирование \
                 копии страницы таблицы перенос переноса страниц"
            }
            Self::CreateUploadUrl => {
                "загрузить загрузка отправить файл файла вложение прикрепить приложить на \
                 страницу адрес ссылка одноразовый curl большой"
            }
        }
    }

    /// Описание инструмента без строки об уровне доступа.
    pub fn description(self) -> String {
        match self {
            Self::UploadAttachment => upload::description(),
            Self::WaitForOperation => wait::description(),
            Self::CreateUploadUrl => upload_url::description(),
        }
    }

    /// Схема аргументов.
    pub fn input_schema(self) -> JsonObject {
        let schema = match self {
            Self::UploadAttachment => upload::input_schema(),
            Self::WaitForOperation => wait::input_schema(),
            Self::CreateUploadUrl => upload_url::input_schema(),
        };
        match schema {
            Value::Object(map) => map,
            _ => JsonObject::new(),
        }
    }

    /// Описание MCP-инструмента с аннотациями по уровню доступа.
    pub fn tool(self) -> Tool {
        let mut description = self.description();
        description.push_str("\n\nAccess: ");
        description.push_str(match self.access() {
            AccessLevel::Read => "read-only.",
            AccessLevel::Write => "write (creates or changes data).",
            AccessLevel::Destructive => "destructive (deletes data or makes irreversible changes).",
        });
        let mut tool = Tool::new(self.name(), description, Arc::new(self.input_schema()));
        tool.title = Some(self.summary().to_owned());
        let base = ToolAnnotations::with_title(self.summary()).open_world(true);
        tool.annotations = Some(match self.access() {
            AccessLevel::Read => base.read_only(true),
            // Повторная загрузка прикрепляет ещё одну копию файла: не идемпотентно.
            AccessLevel::Write => base.read_only(false).destructive(false).idempotent(false),
            AccessLevel::Destructive => base.read_only(false).destructive(true).idempotent(false),
        });
        tool
    }

    /// Выполняет инструмент. Текст ответа уже без учётных данных, с пометкой о замене.
    ///
    /// [`Self::CreateUploadUrl`] нужны ещё адреса транспорта и учётные данные запроса: его
    /// вызывает сервер напрямую ([`upload_url::run`]).
    pub(crate) async fn call(
        self,
        client: &ApiClient,
        files: &FilePolicy,
        args: &JsonObject,
    ) -> Result<String, ToolError> {
        match self {
            Self::UploadAttachment => upload::run(client, files, args).await,
            Self::WaitForOperation => wait::run(client, files, args).await,
            Self::CreateUploadUrl => Err(ToolError::new(
                "internal error: `create_upload_url` is handled by the server",
            )),
        }
    }
}

/// Операция шага по имени инструмента. Её отсутствие — расхождение кода и спецификации
/// (проверяется тестом), агенту возвращается ошибка, а не паника.
pub(crate) fn step_operation(name: &str) -> Result<&'static OperationDef, ToolError> {
    wiki_api::spec::find(name).ok_or_else(|| {
        ToolError::new(format!(
            "internal error: operation `{name}` is missing from the API description"
        ))
    })
}

/// Выполняет шаг и возвращает JSON ответа (пустой ответ — `null`, текст — строкой),
/// добавляя скрытые значения в `redactions`.
pub(crate) async fn call_json(
    client: &ApiClient,
    request: wiki_api::executor::ApiRequest,
    redactions: &mut Redactions,
) -> Result<Value, ToolError> {
    let (response, found) = execute_redacted(client, request).await?;
    redactions.merge(found);
    Ok(match response.body {
        ResponseBody::Json(value) => value,
        ResponseBody::Text(text) => Value::String(text),
        ResponseBody::Empty | ResponseBody::SavedToFile { .. } => Value::Null,
    })
}

/// JSON-объект аргументов для [`crate::request::build_request`].
pub(crate) fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

/// Отклоняет аргументы не из `allowed`.
pub(crate) fn reject_unknown(args: &JsonObject, allowed: &[&str]) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = allowed.iter().map(|name| format!("`{name}`")).collect();
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: {}",
        unknown.join(", "),
        names.join(", ")
    )))
}

/// Аргумент, если передан и не `null`.
pub(crate) fn argument<'a>(args: &'a JsonObject, name: &str) -> Option<&'a Value> {
    args.get(name).filter(|value| !value.is_null())
}

/// Обязательный аргумент.
pub(crate) fn required<'a>(args: &'a JsonObject, name: &str) -> Result<&'a Value, ToolError> {
    argument(args, name)
        .ok_or_else(|| ToolError::new(format!("missing required argument `{name}`")))
}

/// Строковый аргумент без пробелов по краям; пустая строка — как непереданный.
pub(crate) fn optional_string<'a>(
    args: &'a JsonObject,
    name: &str,
) -> Result<Option<&'a str>, ToolError> {
    match argument(args, name) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.trim()).filter(|text| !text.is_empty())),
        Some(_) => Err(ToolError::new(format!(
            "argument `{name}` must be a string"
        ))),
    }
}

/// Обязательный непустой строковый аргумент.
pub(crate) fn required_string<'a>(args: &'a JsonObject, name: &str) -> Result<&'a str, ToolError> {
    optional_string(args, name)?
        .ok_or_else(|| ToolError::new(format!("missing required argument `{name}`")))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn composites_are_consistent() {
        for composite in Composite::ALL {
            assert_eq!(Composite::find(composite.name()), Some(composite));
            // Имя не совпадает с операциями спецификации.
            assert!(wiki_api::spec::find(composite.name()).is_none());
            assert!(wiki_api::spec::find_toolset(composite.toolset()).is_some());
            for step in composite.steps() {
                assert!(step_operation(step).is_ok(), "{step}");
            }
            let tool = composite.tool();
            assert_eq!(tool.input_schema["type"], "object");
            assert_eq!(tool.input_schema["additionalProperties"], false);
            let description = tool.description.as_deref().unwrap();
            assert!(description.contains("Access: "), "{description}");
            let annotations = tool.annotations.as_ref().unwrap();
            assert_eq!(
                annotations.read_only_hint,
                Some(composite.access() == AccessLevel::Read)
            );
            if composite.access() != AccessLevel::Read {
                assert_eq!(annotations.destructive_hint, Some(false));
            }
        }
        assert_eq!(Composite::find("get_page"), None);
        assert_eq!(Composite::UploadAttachment.access(), AccessLevel::Write);
        assert_eq!(Composite::WaitForOperation.access(), AccessLevel::Read);
        assert_eq!(Composite::CreateUploadUrl.access(), AccessLevel::Write);
        assert!(Composite::CreateUploadUrl.remote_only());
        assert!(!Composite::CreateUploadUrl.uses_local_files());
    }

    #[test]
    fn arguments_are_checked() {
        let args = object(json!({"a": " x ", "b": 1, "c": null, "d": ""}));
        assert_eq!(optional_string(&args, "a").unwrap(), Some("x"));
        assert!(optional_string(&args, "b").is_err());
        assert_eq!(optional_string(&args, "c").unwrap(), None);
        assert_eq!(optional_string(&args, "d").unwrap(), None);
        assert!(required_string(&args, "d").is_err());
        assert!(required(&args, "c").is_err());
        let err = reject_unknown(&args, &["a", "b", "c"]).unwrap_err();
        assert!(err.message().contains("`d`"), "{err}");
        assert!(reject_unknown(&args, &["a", "b", "c", "d"]).is_ok());
    }
}
