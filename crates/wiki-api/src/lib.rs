//! Клиент Yandex Wiki Public API.
//!
//! Операции описаны в объединённой OpenAPI-спецификации `spec/wiki.openapi.json`:
//! снимок официальной спецификации Wiki API (`spec/wiki.upstream.json`) и
//! файл-дополнение `spec/overlay.json`, объединённые скриптом
//! `spec/build_openapi.py`.
//!
//! - [`spec`] — операции API и наборы инструментов, сгенерированные из спецификации при
//!   сборке.
//! - [`executor`] — выполнение HTTP-запросов к API.
//! - [`path`] — проверка и сборка относительных путей API.
//! - [`org`] — организация Wiki и заголовок, в котором передаётся её ID.
//! - [`token`] — обёртка токена, которая не попадает в логи и `Debug`.
//! - [`auth`] — хранение токенов и ключей сервисных аккаунтов в системном хранилище.
//! - [`iam`] — IAM-токены Yandex Cloud: получение через `yc` или по ключу сервисного
//!   аккаунта ([`service_account`]), обновление по сроку, только в памяти процесса.

pub mod auth;
pub mod executor;
pub mod iam;
pub mod org;
pub mod path;
pub mod service_account;
pub mod spec;
pub mod token;

pub use executor::{ApiClient, ApiError, ApiRequest, DEFAULT_BASE_URL};
pub use org::{OrgKind, Organization};
pub use spec::{
    AccessLevel, BodyKind, HttpMethod, OperationDef, ParamDef, ParamLocation, ParamType,
    ResponseKind, ToolsetDef,
};
pub use token::ApiToken;

/// Объединённая OpenAPI-спецификация Wiki API в виде JSON.
pub const OPENAPI_JSON: &str = include_str!("../spec/wiki.openapi.json");
