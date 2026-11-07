//! Одноразовые адреса приёма файла для HTTP-режима ([`crate::CREATE_UPLOAD_URL_TOOL`]).
//!
//! HTTP-сервер работает не на машине агента и его файлы не читает, а файл в аргументе
//! инструмента (base64) прошёл бы через контекст агента. Поэтому агент получает адрес на этом
//! же HTTP-сервере и отправляет туда файл сам (`curl -T`). Транспорт принимает запрос
//! ([`UploadUrls::redeem`]) и передаёт тело потоком в ту же загрузку, что у
//! `upload_attachment` ([`crate::WikiServer::upload_stream`]): сессия, части по 16 МиБ,
//! завершение, прикрепление, отмена сессии при ошибке.
//!
//! Безопасность:
//! - токен Wiki не хранится и в адрес не входит. Запрос с файлом несёт те же заголовки
//!   `Authorization` и организации, что MCP-запрос, создавший адрес, и проходит те же
//!   проверки HTTP-сервера (`Host`, `Origin`, токен, организация). Адрес привязан к этим
//!   учётным данным отпечатком — HMAC-SHA256 со случайным ключом процесса: с другим
//!   токеном или другой организацией адрес не принимается (403), а по отпечатку токен не
//!   восстановить;
//! - адрес — случайный идентификатор (128 бит) и секрет (256 бит) из системного генератора
//!   (`getrandom`). Секрет сравнивается за постоянное время; адрес и секрет в логи не
//!   пишутся;
//! - адрес одноразовый: после первой принятой отправки он помечен использованным, повтор —
//!   409, пока загрузка идёт, и 410 после неё. Начать отправку нужно до истечения срока
//!   (410 после); срок по умолчанию [`DEFAULT_TTL`], не больше [`MAX_TTL`];
//! - размер файла задаётся при создании адреса (не больше [`MAX_FILE_BYTES`]): тело другого
//!   размера отклоняется (413 — больше, 400 — меньше);
//! - активных адресов (не использованных и загружаемых) не больше [`MAX_ACTIVE`]: каждая
//!   загрузка держит в памяти одну часть файла, до 16 МиБ. На диск сервер ничего не пишет.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use rmcp::model::{Extensions, JsonObject};
use serde_json::{Value, json};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use wiki_api::executor::{ApiClient, AuthScheme};

use crate::composite::{argument, call_json, object, reject_unknown, required, step_operation};
use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::redact::Redactions;
use crate::request::build_request;
use crate::server::RequestAuth;
use crate::upload::{PART_BYTES, UploadTarget};

/// Префикс пути адресов на HTTP-сервере: `/uploads/<id>/<секрет>`.
pub const UPLOAD_PATH: &str = "/uploads";
/// Срок жизни адреса по умолчанию.
pub const DEFAULT_TTL: Duration = Duration::from_secs(5 * 60);
/// Наибольший срок жизни адреса.
pub const MAX_TTL: Duration = Duration::from_secs(15 * 60);
/// Наибольшее число активных адресов.
pub const MAX_ACTIVE: usize = 8;
/// Наибольший размер файла, байт: 1 ГиБ.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

const CHECK_PAGE: &str = "get_page_by_id";

/// Операции шагов: проверка страницы при создании адреса и шаги загрузки при приёме файла.
pub(crate) const STEPS: &[&str] = &[
    CHECK_PAGE,
    "create_upload_session",
    "upload_file_part",
    "finish_upload_session",
    "attach_uploaded_files",
    "abort_upload_session",
];

const ARGUMENTS: &[&str] = &["idx", "file_name", "file_size", "expires_in_seconds"];
const MAX_FILE_NAME_CHARS: usize = 255;
const ID_BYTES: usize = 16;
const SECRET_BYTES: usize = 32;
/// Сколько помнить использованный или просроченный адрес, чтобы отвечать 410, а не 404.
const RETAIN: Duration = Duration::from_secs(15 * 60);
/// Наибольшее число записей вместе с использованными и просроченными.
const MAX_ENTRIES: usize = 1024;
const MIB: u64 = 1024 * 1024;

/// Ограничения адресов. Транспорт берёт [`Default`]; тесты задают короткие сроки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadUrlLimits {
    /// Срок жизни адреса, если агент его не задал.
    pub default_ttl: Duration,
    /// Наибольший срок жизни адреса.
    pub max_ttl: Duration,
    /// Наибольшее число активных адресов.
    pub max_active: usize,
    /// Наибольший размер файла, байт.
    pub max_file_bytes: u64,
}

impl Default for UploadUrlLimits {
    fn default() -> Self {
        Self {
            default_ttl: DEFAULT_TTL,
            max_ttl: MAX_TTL,
            max_active: MAX_ACTIVE,
            max_file_bytes: MAX_FILE_BYTES,
        }
    }
}

/// Функция транспорта: базовый адрес сервера (`http://хост:порт`) по расширениям MCP-запроса,
/// без пути и завершающей косой черты. `None` — адрес не построить.
pub type BaseUrlFn = dyn Fn(&Extensions) -> Option<String> + Send + Sync;

/// Одноразовые адреса приёма файлов. Создаёт транспорт и делит между ядром (создание адреса
/// инструментом) и обработчиком HTTP-запросов с файлом (приём).
pub struct UploadUrls {
    limits: UploadUrlLimits,
    base_url_of: Box<BaseUrlFn>,
    org_in_command: bool,
    /// Ключ отпечатков учётных данных; живёт только в памяти процесса.
    key: [u8; 32],
    entries: Mutex<HashMap<String, Entry>>,
}

impl fmt::Debug for UploadUrls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Ни ключа, ни адресов.
        f.debug_struct("UploadUrls")
            .field("limits", &self.limits)
            .field("org_in_command", &self.org_in_command)
            .finish_non_exhaustive()
    }
}

struct Entry {
    /// Секрет адреса (hex).
    secret: String,
    /// Отпечаток учётных данных MCP-запроса, создавшего адрес.
    binding: [u8; 32],
    target: UploadTarget,
    expires: Instant,
    state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Создан, файл ещё не отправляли.
    Pending,
    /// Файл принимается.
    Uploading,
    /// Отправка была (успешная или нет).
    Used,
}

impl Entry {
    fn active(&self, now: Instant) -> bool {
        match self.state {
            State::Pending => now < self.expires,
            State::Uploading => true,
            State::Used => false,
        }
    }
}

/// Созданный адрес.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedUrl {
    /// Полный адрес с секретом.
    pub url: String,
    pub expires_in: Duration,
}

impl fmt::Debug for IssuedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedUrl")
            .field("url", &"***")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

/// Почему отправка файла на адрес не принята. Значения адреса в ошибку не попадают.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedeemError {
    /// Адреса нет или секрет не совпал (404).
    NotFound,
    /// Токен или организация не те, с которыми адрес создан (403).
    Forbidden,
    /// Файл по этому адресу уже принимается (409).
    InProgress,
    /// Адрес уже использован (410).
    Used,
    /// Срок адреса истёк (410).
    Expired,
    /// `Content-Length` больше размера, для которого создан адрес (413).
    TooLarge { expected: u64 },
    /// `Content-Length` меньше размера, для которого создан адрес (400).
    TooShort { expected: u64 },
}

/// Принятый адрес: цель загрузки. Пока значение живо, адрес «загружается»; при удалении
/// (в любом исходе загрузки, в том числе при панике или отмене) он помечается использованным.
pub struct Redeemed {
    urls: Arc<UploadUrls>,
    id: String,
    target: UploadTarget,
}

impl Redeemed {
    pub fn target(&self) -> &UploadTarget {
        &self.target
    }
}

impl fmt::Debug for Redeemed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Redeemed")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

impl Drop for Redeemed {
    fn drop(&mut self) {
        if let Some(entry) = self.urls.lock().get_mut(&self.id) {
            entry.state = State::Used;
        }
    }
}

impl UploadUrls {
    /// `base_url_of` — базовый адрес сервера по MCP-запросу; `org_in_command` — добавлять ли
    /// заголовок организации в команду `curl` (не нужно, если сервер закреплён за
    /// организацией). Ошибка — не удалось получить случайный ключ.
    pub fn new(
        limits: UploadUrlLimits,
        base_url_of: Box<BaseUrlFn>,
        org_in_command: bool,
    ) -> Result<Self, String> {
        let mut key = [0_u8; 32];
        getrandom::fill(&mut key)
            .map_err(|err| format!("cannot get random bytes for upload URLs: {err}"))?;
        Ok(Self {
            limits,
            base_url_of,
            org_in_command,
            key,
            entries: Mutex::new(HashMap::new()),
        })
    }

    pub fn limits(&self) -> UploadUrlLimits {
        self.limits
    }

    /// Число активных адресов (не использованных и загружаемых).
    pub fn active(&self) -> usize {
        let now = Instant::now();
        self.lock().values().filter(|e| e.active(now)).count()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        // Под замком нет кода, который может паниковать посередине изменения записи.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Отпечаток учётных данных: схема, токен и организация.
    fn binding(&self, auth: &RequestAuth) -> Option<[u8; 32]> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key).ok()?;
        let scheme: &[u8] = match auth.credentials.scheme {
            AuthScheme::OAuth => b"oauth",
            AuthScheme::Iam => b"iam",
        };
        let (org_kind, org_id) = match &auth.org {
            Some(org) => (org.header_name().as_bytes(), org.id().as_bytes()),
            None => (&b""[..], &b""[..]),
        };
        for field in [
            scheme,
            auth.credentials.token.expose().as_bytes(),
            org_kind,
            org_id,
        ] {
            // Длина перед каждым полем: границы полей однозначны.
            mac.update(&(field.len() as u64).to_be_bytes());
            mac.update(field);
        }
        Some(mac.finalize().into_bytes().into())
    }

    /// Убирает давно использованные и просроченные записи; при переполнении — самые старые
    /// неактивные.
    fn purge(entries: &mut HashMap<String, Entry>, now: Instant) {
        entries.retain(|_, entry| {
            entry.state == State::Uploading
                || entry
                    .expires
                    .checked_add(RETAIN)
                    .is_none_or(|until| now < until)
        });
        if entries.len() <= MAX_ENTRIES {
            return;
        }
        let mut inactive: Vec<(Instant, String)> = entries
            .iter()
            .filter(|(_, entry)| !entry.active(now))
            .map(|(id, entry)| (entry.expires, id.clone()))
            .collect();
        inactive.sort();
        let excess = entries.len() - MAX_ENTRIES;
        for (_, id) in inactive.into_iter().take(excess) {
            entries.remove(&id);
        }
    }

    /// Создаёт адрес для `target` со сроком `ttl`.
    pub(crate) fn issue(
        &self,
        auth: &RequestAuth,
        target: UploadTarget,
        ttl: Duration,
        extensions: &Extensions,
    ) -> Result<IssuedUrl, ToolError> {
        let base = (self.base_url_of)(extensions).ok_or_else(|| {
            ToolError::new(
                "cannot build an upload URL: the request has no usable Host header; this tool \
                 works only over the HTTP transport",
            )
        })?;
        let binding = self
            .binding(auth)
            .ok_or_else(|| ToolError::new("internal error: cannot bind the upload URL"))?;
        let id = random_hex(ID_BYTES)?;
        let secret = random_hex(SECRET_BYTES)?;
        let now = Instant::now();
        let expires = now
            .checked_add(ttl)
            .ok_or_else(|| ToolError::new("argument `expires_in_seconds` is too large"))?;
        let mut entries = self.lock();
        Self::purge(&mut entries, now);
        let active = entries.values().filter(|e| e.active(now)).count();
        if active >= self.limits.max_active {
            return Err(ToolError::new(format!(
                "too many active upload URLs ({active}, at most {}): use the URLs you already \
                 created or wait until they expire, then try again",
                self.limits.max_active
            )));
        }
        entries.insert(
            id.clone(),
            Entry {
                secret: secret.clone(),
                binding,
                target,
                expires,
                state: State::Pending,
            },
        );
        Ok(IssuedUrl {
            url: format!("{base}{UPLOAD_PATH}/{id}/{secret}"),
            expires_in: ttl,
        })
    }

    /// Принимает отправку на адрес `id`/`secret` с учётными данными запроса `auth`.
    ///
    /// Порядок проверок: адрес и секрет (404), учётные данные (403), состояние (409, 410),
    /// `Content-Length` (413, 400). Адрес помечается «загружается» только после всех
    /// проверок: отклонённая отправка (не тот токен, не тот размер) его не тратит.
    pub fn redeem(
        self: &Arc<Self>,
        id: &str,
        secret: &str,
        auth: &RequestAuth,
        content_length: Option<u64>,
    ) -> Result<Redeemed, RedeemError> {
        let binding = self.binding(auth).ok_or(RedeemError::Forbidden)?;
        let now = Instant::now();
        let mut entries = self.lock();
        Self::purge(&mut entries, now);
        let entry = entries.get_mut(id).ok_or(RedeemError::NotFound)?;
        if !bool::from(entry.secret.as_bytes().ct_eq(secret.as_bytes())) {
            return Err(RedeemError::NotFound);
        }
        if !bool::from(entry.binding.ct_eq(&binding)) {
            return Err(RedeemError::Forbidden);
        }
        match entry.state {
            State::Uploading => return Err(RedeemError::InProgress),
            State::Used => return Err(RedeemError::Used),
            State::Pending if now >= entry.expires => return Err(RedeemError::Expired),
            State::Pending => {}
        }
        let expected = entry.target.file_size;
        match content_length {
            Some(len) if len > expected => return Err(RedeemError::TooLarge { expected }),
            Some(len) if len < expected => return Err(RedeemError::TooShort { expected }),
            _ => {}
        }
        entry.state = State::Uploading;
        let target = entry.target.clone();
        drop(entries);
        Ok(Redeemed {
            urls: Arc::clone(self),
            id: id.to_owned(),
            target,
        })
    }
}

/// `n` случайных байт в hex.
fn random_hex(n: usize) -> Result<String, ToolError> {
    let mut bytes = vec![0_u8; n];
    getrandom::fill(&mut bytes).map_err(|err| {
        tracing::warn!(%err, "не удалось получить случайные байты для адреса загрузки");
        ToolError::new("internal error: cannot generate an upload URL")
    })?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn description() -> String {
    format!(
        "Create a one-time URL on this HTTP server for uploading a file from your machine and \
         attaching it to a page. This server runs on another machine and cannot read your \
         files: send the file yourself with the `curl` command from the result (HTTP PUT of \
         the raw file; POST with `--data-binary @<file>` works too). Do not pass file content \
         in tool arguments.\n\n\
         The tool checks that the page exists (`{CHECK_PAGE}`) and returns `upload_url`, \
         `curl`, `headers` and `expires_in_seconds`. When the file arrives, the server streams \
         it to Wiki like `upload_attachment`: upload session, parts of {part} MiB, finish, \
         attach; if a step fails, the session is aborted and nothing is attached. The response \
         to `curl` is JSON: the attachment (`results` with `download_url`) plus `upload`, or \
         `error` and `message`.\n\n\
         The URL works once and only for `file_size` bytes (at most {max_mib} MiB): another \
         size is rejected (413 or 400), a second upload too (409 while the first runs, 410 \
         after it). Start the upload within `expires_in_seconds` (default {default_ttl}, at \
         most {max_ttl}); then the URL is gone (410). The upload request must carry the same \
         `Authorization` header and organization header as your MCP requests: the token is \
         not part of the URL, and the server does not store it. In the command \
         `$WIKI_AUTHORIZATION` stands for that `Authorization` value (`OAuth <token>`): use \
         the environment variable if it is set, never print the token or write it to files. \
         At most {active} URLs can be active at a time. The file does not appear in the page \
         text by itself: add `{{% file src=\"<download_url>\" name=\"<name>\" %}}` with \
         `append_page_content`.",
        part = PART_BYTES / MIB,
        max_mib = MAX_FILE_BYTES / MIB,
        default_ttl = DEFAULT_TTL.as_secs(),
        max_ttl = MAX_TTL.as_secs(),
        active = MAX_ACTIVE,
    )
}

pub(crate) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "idx": {
                "type": "integer",
                "description": "Numeric ID of the page to attach the file to: the `id` field of \
                    `get_page`, `get_page_by_id` or `create_page`. Not a slug or a URL."
            },
            "file_name": {
                "type": "string",
                "description": "Name of the attachment in Wiki, for example `report.pdf`: only \
                    the name, without directories."
            },
            "file_size": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_FILE_BYTES,
                "description": "Exact size of the file in bytes (for example from `wc -c < \
                    file` or `stat`). The URL accepts exactly this many bytes."
            },
            "expires_in_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_TTL.as_secs(),
                "description": format!(
                    "How long the URL waits for the upload to start, in seconds. Default {}, \
                     at most {}.",
                    DEFAULT_TTL.as_secs(),
                    MAX_TTL.as_secs()
                )
            }
        },
        "required": ["idx", "file_name", "file_size"],
        "additionalProperties": false
    })
}

/// Выполняет инструмент: проверяет аргументы и страницу, создаёт адрес.
pub(crate) async fn run(
    urls: Option<&UploadUrls>,
    client: &ApiClient,
    files: &FilePolicy,
    auth: &RequestAuth,
    args: &JsonObject,
    extensions: &Extensions,
) -> Result<String, ToolError> {
    let urls = urls.ok_or_else(|| {
        ToolError::new(
            "upload URLs are not available on this server: they need the HTTP transport \
             (`wiki-mcp serve --http`)",
        )
    })?;
    reject_unknown(args, ARGUMENTS)?;
    let limits = urls.limits();
    let idx = integer(required(args, "idx")?, "idx")?;
    let file_name = file_name(required(args, "file_name")?)?;
    let file_size = integer(required(args, "file_size")?, "file_size")?;
    if file_size == 0 || file_size > limits.max_file_bytes {
        return Err(ToolError::new(format!(
            "argument `file_size` must be from 1 to {} bytes ({} MiB); Wiki does not accept \
             empty files",
            limits.max_file_bytes,
            limits.max_file_bytes / MIB
        )));
    }
    let max_ttl = limits.max_ttl;
    let ttl = match argument(args, "expires_in_seconds") {
        None => limits.default_ttl.min(max_ttl),
        Some(value) => {
            let secs = integer(value, "expires_in_seconds")?;
            let ttl = Duration::from_secs(secs);
            if secs == 0 || ttl > max_ttl {
                return Err(ToolError::new(format!(
                    "argument `expires_in_seconds` must be from 1 to {}",
                    max_ttl.as_secs()
                )));
            }
            ttl
        }
    };

    // Страница и доступ проверяются сразу: иначе ошибка выяснилась бы только после передачи
    // всего файла. Ответ не нужен.
    let check = build_request(
        step_operation(CHECK_PAGE)?,
        &object(json!({"idx": idx})),
        files,
    )?;
    let mut ignored = Redactions::default();
    call_json(client, check, &mut ignored)
        .await
        .map_err(|err| {
            ToolError::new(format!(
                "page {idx} cannot be used for the upload (`{CHECK_PAGE}` failed), no URL was \
                 created: {}",
                err.message()
            ))
        })?;

    let target = UploadTarget {
        idx,
        file_name: file_name.clone(),
        file_size,
    };
    let issued = urls.issue(auth, target, ttl, extensions)?;
    let org_header = auth
        .org
        .as_ref()
        .filter(|_| urls.org_in_command)
        .map(|org| (org.header_name(), org.id().to_owned()));
    let mut headers = serde_json::Map::new();
    headers.insert(
        "Authorization".to_owned(),
        json!("the same value as the Authorization header your MCP client sends"),
    );
    let mut command = "curl -sS -T <file> -H \"Authorization: $WIKI_AUTHORIZATION\"".to_owned();
    if let Some((name, id)) = &org_header {
        headers.insert((*name).to_owned(), json!(id));
        // ID организации — из `A-Z a-z 0-9 - _` (проверено транспортом): кавычек не нужно
        // экранировать.
        command.push_str(&format!(" -H '{name}: {id}'"));
    }
    command.push_str(&format!(" '{}'", issued.url));
    let result = json!({
        "upload_url": issued.url,
        "method": "PUT",
        "expires_in_seconds": issued.expires_in.as_secs_f64().ceil() as u64,
        "idx": idx,
        "file_name": file_name,
        "file_size": file_size,
        "headers": Value::Object(headers),
        "curl": command,
        "note": "Run `curl` before the URL expires: replace `<file>` with the path of the file \
                 (exactly `file_size` bytes) and let the shell expand $WIKI_AUTHORIZATION, the \
                 value of the Authorization header of your MCP client (`OAuth <token>`). Do not \
                 print the token or write it to files; if it is not available to your shell, \
                 ask the user to run the command. The URL works once: for another upload \
                 create a new one. The command prints JSON with the attachment or an error.",
    });
    Ok(result.to_string())
}

/// Целое неотрицательное число: JSON-число или строка из цифр.
fn integer(value: &Value, name: &str) -> Result<u64, ToolError> {
    let parsed = match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => {
            let text = text.trim();
            if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
                text.parse().ok()
            } else {
                None
            }
        }
        _ => None,
    };
    parsed.ok_or_else(|| {
        ToolError::new(format!(
            "argument `{name}` must be a non-negative integer (a number or a string of digits)"
        ))
    })
}

/// Имя вложения: непустое, без каталогов и управляющих символов.
fn file_name(value: &Value) -> Result<String, ToolError> {
    let Value::String(name) = value else {
        return Err(ToolError::new("argument `file_name` must be a string"));
    };
    let name = name.trim();
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().count() > MAX_FILE_NAME_CHARS
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control())
    {
        return Err(ToolError::new(format!(
            "argument `file_name` must be a file name without directories, 1 to \
             {MAX_FILE_NAME_CHARS} characters, for example `report.pdf`"
        )));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use wiki_api::ApiToken;
    use wiki_api::executor::{Credentials, OrgId};

    use super::*;

    fn urls(limits: UploadUrlLimits) -> Arc<UploadUrls> {
        Arc::new(
            UploadUrls::new(
                limits,
                Box::new(|_| Some("http://127.0.0.1:8767".to_owned())),
                true,
            )
            .unwrap(),
        )
    }

    fn auth(token: &str, org: &str) -> RequestAuth {
        RequestAuth {
            credentials: Credentials::oauth(ApiToken::new(token)),
            org: Some(OrgId::Yandex360(org.to_owned())),
        }
    }

    fn target(size: u64) -> UploadTarget {
        UploadTarget {
            idx: 42,
            file_name: "report.pdf".into(),
            file_size: size,
        }
    }

    /// Идентификатор и секрет из адреса.
    fn split(url: &str) -> (String, String) {
        let rest = url.strip_prefix("http://127.0.0.1:8767/uploads/").unwrap();
        let (id, secret) = rest.split_once('/').unwrap();
        (id.to_owned(), secret.to_owned())
    }

    #[test]
    fn url_has_random_id_and_secret() {
        let urls = urls(UploadUrlLimits::default());
        let a = urls
            .issue(&auth("t", "1"), target(5), DEFAULT_TTL, &Extensions::new())
            .unwrap();
        let b = urls
            .issue(&auth("t", "1"), target(5), DEFAULT_TTL, &Extensions::new())
            .unwrap();
        let (id_a, secret_a) = split(&a.url);
        let (id_b, secret_b) = split(&b.url);
        assert_eq!(id_a.len(), 2 * ID_BYTES);
        assert_eq!(secret_a.len(), 2 * SECRET_BYTES);
        assert!(secret_a.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(id_a, id_b);
        assert_ne!(secret_a, secret_b);
        assert_eq!(format!("{a:?}").matches(&secret_a).count(), 0);
        assert_eq!(urls.active(), 2);
    }

    #[test]
    fn url_is_used_once_and_bound_to_credentials() {
        let urls = urls(UploadUrlLimits::default());
        let owner = auth("token-a", "1");
        let issued = urls
            .issue(&owner, target(5), DEFAULT_TTL, &Extensions::new())
            .unwrap();
        let (id, secret) = split(&issued.url);

        let wrong_secret = format!("{}0", &secret[..secret.len() - 1]);
        let wrong_secret = if wrong_secret == secret {
            format!("{}1", &secret[..secret.len() - 1])
        } else {
            wrong_secret
        };
        let err = |r: Result<Redeemed, RedeemError>| r.map(|_| ()).unwrap_err();
        assert_eq!(
            err(urls.redeem(&id, &wrong_secret, &owner, None)),
            RedeemError::NotFound
        );
        assert_eq!(
            err(urls.redeem("00", &secret, &owner, None)),
            RedeemError::NotFound
        );
        assert_eq!(
            err(urls.redeem(&id, &secret, &auth("token-b", "1"), None)),
            RedeemError::Forbidden
        );
        assert_eq!(
            err(urls.redeem(&id, &secret, &auth("token-a", "2"), None)),
            RedeemError::Forbidden
        );
        let cloud = RequestAuth {
            org: Some(OrgId::Cloud("1".into())),
            ..owner.clone()
        };
        assert_eq!(
            err(urls.redeem(&id, &secret, &cloud, None)),
            RedeemError::Forbidden
        );
        let iam = RequestAuth {
            credentials: Credentials::iam(ApiToken::new("token-a")),
            ..owner.clone()
        };
        assert_eq!(
            err(urls.redeem(&id, &secret, &iam, None)),
            RedeemError::Forbidden
        );
        assert_eq!(
            err(urls.redeem(&id, &secret, &owner, Some(6))),
            RedeemError::TooLarge { expected: 5 }
        );
        assert_eq!(
            err(urls.redeem(&id, &secret, &owner, Some(4))),
            RedeemError::TooShort { expected: 5 }
        );

        // Отклонённые попытки адрес не тратят.
        let redeemed = urls.redeem(&id, &secret, &owner, Some(5)).unwrap();
        assert_eq!(redeemed.target(), &target(5));
        assert_eq!(
            err(urls.redeem(&id, &secret, &owner, None)),
            RedeemError::InProgress
        );
        assert_eq!(urls.active(), 1);
        drop(redeemed);
        assert_eq!(
            err(urls.redeem(&id, &secret, &owner, None)),
            RedeemError::Used
        );
        assert_eq!(urls.active(), 0);
    }

    #[test]
    fn url_expires() {
        let urls = urls(UploadUrlLimits::default());
        let owner = auth("t", "1");
        let issued = urls
            .issue(&owner, target(5), Duration::ZERO, &Extensions::new())
            .unwrap();
        let (id, secret) = split(&issued.url);
        assert_eq!(urls.active(), 0);
        assert_eq!(
            urls.redeem(&id, &secret, &owner, None).map(|_| ()),
            Err(RedeemError::Expired)
        );
    }

    #[test]
    fn active_urls_are_limited() {
        let urls = urls(UploadUrlLimits {
            max_active: 2,
            ..UploadUrlLimits::default()
        });
        let owner = auth("t", "1");
        let issue = || urls.issue(&owner, target(5), DEFAULT_TTL, &Extensions::new());
        let first = issue().unwrap();
        issue().unwrap();
        let err = issue().unwrap_err();
        assert!(
            err.message().contains("too many active upload URLs"),
            "{err}"
        );
        // Использованный адрес место освобождает.
        let (id, secret) = split(&first.url);
        drop(urls.redeem(&id, &secret, &owner, None).unwrap());
        issue().unwrap();
    }

    #[test]
    fn missing_base_url_is_an_error() {
        let urls = UploadUrls::new(UploadUrlLimits::default(), Box::new(|_| None), true).unwrap();
        let err = urls
            .issue(&auth("t", "1"), target(5), DEFAULT_TTL, &Extensions::new())
            .unwrap_err();
        assert!(err.message().contains("Host"), "{err}");
    }

    #[test]
    fn arguments_are_checked() {
        assert_eq!(integer(&json!(5), "x").unwrap(), 5);
        assert_eq!(integer(&json!(" 7 "), "x").unwrap(), 7);
        for bad in [json!(-1), json!(1.5), json!("1e3"), json!(""), json!(null)] {
            assert!(integer(&bad, "x").is_err(), "{bad}");
        }
        assert_eq!(file_name(&json!(" a b.pdf ")).unwrap(), "a b.pdf");
        for bad in [
            json!(""),
            json!(".."),
            json!("dir/a.pdf"),
            json!("dir\\a.pdf"),
            json!("a\nb"),
            json!(1),
            json!("x".repeat(256)),
        ] {
            assert!(file_name(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn description_names_limits() {
        let text = description();
        assert!(text.contains("curl"), "{text}");
        assert!(text.contains("$WIKI_AUTHORIZATION"), "{text}");
        assert!(text.contains("1024 MiB"), "{text}");
        assert!(text.contains("default 300, at most 900"), "{text}");
    }
}
