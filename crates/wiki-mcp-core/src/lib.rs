//! Ядро MCP-сервера Yandex Wiki: регистрация инструментов, уровни доступа и
//! инструкции для агентов. Не зависит от транспорта (stdio или HTTP).
//!
//! - [`server`] — обработчик `rmcp` ([`WikiServer`]) и инструкции для агента;
//! - [`registry`] — какие инструменты регистрируются при заданных [`ServerOptions`];
//! - [`api_request`] и [`api_search`] — встроенные инструменты: запрос к любому пути API
//!   в пределах уровня доступа и поиск по операциям;
//! - [`composite`] — составные инструменты: загрузка файла вложением ([`upload`]) и
//!   ожидание фоновой операции ([`wait`]);
//! - [`routes`] — сопоставление метода и пути с операциями (уровень доступа `api_request`);
//! - [`request`] — сборка запроса к API из аргументов инструмента;
//! - [`response`] — представление ответа API в результате инструмента;
//! - [`redact`] — вырезание значений, похожих на учётные данные, из ответов и ошибок;
//! - [`files`] — политика доступа к локальным файлам (загрузка и скачивание);
//! - [`error`] — ошибки вызова с текстом для агента;
//! - [`toolsets`] — наборы инструментов (`--toolsets`) и наборы по умолчанию.
//!
//! Транспорт (stdio, позже HTTP) подключается в бинарнике `wiki-mcp`. Ядро о транспорте знает
//! две вещи, обе в настройках: работает ли сервер на машине агента
//! ([`ServerOptions::local_machine`]) и откуда брать токен ([`TokenSource`]).

pub mod api_request;
pub mod api_search;
pub mod composite;
pub mod error;
pub mod files;
pub mod redact;
pub mod registry;
pub mod request;
pub mod response;
pub mod routes;
pub mod server;
pub mod toolsets;
pub mod upload;
pub mod wait;

pub use api_request::API_REQUEST_TOOL;
pub use api_search::API_SEARCH_TOOL;
pub use composite::{Composite, UPLOAD_ATTACHMENT_TOOL, WAIT_FOR_OPERATION_TOOL};
pub use error::ToolError;
pub use files::{FilePolicy, FilePolicyError, LOCAL_FILES_DISABLED};
pub use registry::{RegisteredTool, ToolKind, ToolRegistry, access_name};
pub use server::{RequestAuth, RequestCredentialsFn, SERVER_NAME, TokenSource, WikiServer};
pub use toolsets::{
    ALL_TOOLSETS, Toolset, available_toolsets, default_toolsets, effective_toolsets,
    normalize_toolsets,
};
pub use wiki_api;

use wiki_api::AccessLevel;

/// Настройки MCP-сервера.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Максимальный разрешённый уровень доступа. Инструменты выше него не регистрируются.
    pub max_access: AccessLevel,
    /// Включённые наборы инструментов (slug, уже проверенные [`normalize_toolsets`]).
    /// `None` или пустой список — наборы по умолчанию ([`default_toolsets`]).
    pub toolsets: Option<Vec<String>>,
    /// Сервер работает на машине агента (stdio).
    ///
    /// `false` — сервер удалённый (HTTP-режим): файлы машины сервера не имеют отношения
    /// к агенту, поэтому файловые инструменты не регистрируются и любой путь отклоняется
    /// ([`FilePolicy::disabled`]), а токен приходит с каждым запросом
    /// ([`TokenSource::PerRequest`]).
    pub local_machine: bool,
    /// Профиль токена, с которым запущен сервер; подставляется в подсказки инструкций
    /// (`wiki-mcp auth login --profile …`).
    pub profile: Option<String>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_access: AccessLevel::Read,
            toolsets: None,
            local_machine: true,
            profile: None,
        }
    }
}

/// Уровень доступа по флагам `--allow-write` и `--allow-destructive`: удаление подразумевает
/// запись. Единое правило для `serve`, статуса и профилей.
pub fn access_from_flags(allow_write: bool, allow_destructive: bool) -> AccessLevel {
    if allow_destructive {
        AccessLevel::Destructive
    } else if allow_write {
        AccessLevel::Write
    } else {
        AccessLevel::Read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_implies_write() {
        assert_eq!(access_from_flags(false, false), AccessLevel::Read);
        assert_eq!(access_from_flags(true, false), AccessLevel::Write);
        assert_eq!(access_from_flags(false, true), AccessLevel::Destructive);
        assert_eq!(access_from_flags(true, true), AccessLevel::Destructive);
    }
}
