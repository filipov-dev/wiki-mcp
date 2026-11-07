//! Обработчик MCP-сервера `rmcp` без привязки к транспорту.
//!
//! Набор инструментов динамический (строится из таблицы операций), поэтому `ServerHandler`
//! реализован вручную, без макросов `#[tool]`. Транспорт подключает бинарник:
//! `WikiServer::new(...)?.serve(stdio())`.
//!
//! Токен Wiki для вызова берётся из [`TokenSource`]: у stdio-сервера это токен клиента
//! API из системного хранилища, у HTTP-сервера — токен текущего HTTP-запроса агента, который
//! транспорт кладёт в расширения запроса MCP. Ядро не знает, как устроен HTTP: функцию
//! извлечения токена передаёт бинарник. Организация — из клиента API (профиль), а в
//! HTTP-режиме — из того же запроса, если транспорт её передал.
//!
//! Состояние сессии — текущий реестр инструментов: в режиме динамических наборов
//! (`ServerOptions::dynamic_toolsets`) `enable_toolset`/`disable_toolset` подменяют его снимок,
//! и клиент получает `notifications/tools/list_changed`: в сессии с `initialize` — сразу после
//! вызова, в протоколе 2026-07-28 — через подписку `subscriptions/listen`. Клоны сервера делят
//! сессию; транспорт с несколькими сессиями создаёт их через [`WikiServer::new_session`].
//! Реестр от токена не зависит, поэтому с токенами отдельных запросов наборы переключаются
//! так же. HTTP-режим сессий не хранит, и динамических наборов в нём нет: там реестр не
//! меняется, и общая сессия копий сервера ничего не разделяет.
//!
//! HTTP-транспорт ещё подключает одноразовые адреса загрузки ([`WikiServer::with_upload_urls`]):
//! инструмент `create_upload_url` создаёт адрес, а транспорт, приняв на него файл, вызывает
//! [`WikiServer::upload_stream`] с учётными данными того HTTP-запроса.

use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, RwLock};

use tokio::io::AsyncRead;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Extensions, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
    ServerConfig, SubscriptionFilter, Tool,
};
use rmcp::service::{RequestContext, SubscriptionContext};
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{Map, Value};
use wiki_api::executor::{ApiClient, ApiRequest, ApiResponse, Credentials, OrgId, ResponseBody};
use wiki_api::spec::{AccessLevel, OperationDef};

use crate::ServerOptions;
use crate::api_request::prepare;
use crate::api_search::search;
use crate::composite::{
    CREATE_UPLOAD_URL_TOOL, Composite, UPLOAD_ATTACHMENT_TOOL, WAIT_FOR_OPERATION_TOOL,
};
use crate::dynamic_toolsets;
use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::jq::{FILTER_INPUT_LIMIT, JqFilter, JqRunner, take_jq, takes_jq};
use crate::redact::{Redactions, redact_api_error, redact_message, redact_response};
use crate::registry::{ToolKind, ToolRegistry, access_name};
use crate::request::build_request;
use crate::response::{failure, response_text, success};
use crate::upload::{StreamUploadError, UploadTarget, run_stream};
use crate::upload_url::{self, UploadUrls};

/// Имя сервера в `initialize`.
pub const SERVER_NAME: &str = "wiki-mcp";

/// Поля тела ответа Wiki с курсорами постраничной выдачи. С фильтром `jq` они выносятся
/// рядом с результатом фильтра (см. [`filter_response`]).
const CURSOR_FIELDS: [&str; 2] = ["next_cursor", "prev_cursor"];

/// Учётные данные текущего запроса: токен и, если запрос её указал, организация.
///
/// `Debug` безопасен: [`Credentials`] и [`OrgId`] не раскрывают значения.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestAuth {
    pub credentials: Credentials,
    /// Организация запроса. `None` — организация клиента API (заданная при запуске).
    pub org: Option<OrgId>,
}

/// Функция, которая достаёт учётные данные текущего запроса из расширений запроса MCP.
pub type RequestCredentialsFn = dyn Fn(&Extensions) -> Option<RequestAuth> + Send + Sync;

/// Откуда брать токен Wiki для вызова инструмента.
#[derive(Clone, Default)]
pub enum TokenSource {
    /// Токен клиента API, заданный при запуске (stdio: из системного хранилища по профилю).
    #[default]
    Client,
    /// Токен текущего запроса (HTTP: заголовок `Authorization` каждого HTTP-запроса).
    ///
    /// Токен читается заново на каждый вызов и не сохраняется: сессия MCP может пережить
    /// токен, а у разных запросов могут быть разные токены. Без токена вызов к API не
    /// выполняется и возвращает ошибку инструмента.
    PerRequest(Arc<RequestCredentialsFn>),
}

impl fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client => f.write_str("Client"),
            Self::PerRequest(_) => f.write_str("PerRequest"),
        }
    }
}

/// MCP-сервер Wiki. Дёшево клонируется; клоны делят сессию.
#[derive(Debug, Clone)]
pub struct WikiServer {
    inner: Arc<Inner>,
    session: Arc<Session>,
}

#[derive(Debug, Clone)]
struct Inner {
    client: ApiClient,
    tokens: TokenSource,
    options: ServerOptions,
    files: FilePolicy,
    /// Реестр при запуске; с него начинается каждая сессия.
    initial: Arc<ToolRegistry>,
    instructions: String,
    /// Одноразовые адреса загрузки HTTP-транспорта; `None` — их нет (stdio, тесты ядра).
    uploads: Option<Arc<UploadUrls>>,
}

/// Изменяемое состояние одной MCP-сессии.
#[derive(Debug)]
struct Session {
    registry: RwLock<Arc<ToolRegistry>>,
    /// Счётчик изменений списка инструментов: по нему подписки `subscriptions/listen` узнают,
    /// что пора отправить `notifications/tools/list_changed`.
    changes: tokio::sync::watch::Sender<u64>,
}

impl Session {
    fn new(registry: Arc<ToolRegistry>) -> Self {
        Self {
            registry: RwLock::new(registry),
            changes: tokio::sync::watch::Sender::new(0),
        }
    }

    fn registry(&self) -> Arc<ToolRegistry> {
        // Отравленная блокировка не мешает: внутри только `Arc`, его замена атомарна.
        self.registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn generation(&self) -> u64 {
        *self.changes.borrow()
    }
}

impl WikiServer {
    /// Создаёт сервер с токеном клиента API ([`TokenSource::Client`]).
    /// Ошибка — неизвестный набор инструментов в `options.toolsets` или конфликт имён
    /// пользовательских инструментов.
    pub fn new(
        client: ApiClient,
        options: ServerOptions,
        files: FilePolicy,
    ) -> Result<Self, String> {
        Self::with_token_source(client, options, files, TokenSource::Client)
    }

    /// Создаёт сервер с заданным источником токена.
    ///
    /// Без [`ServerOptions::local_machine`] политика файлов принудительно
    /// [`FilePolicy::disabled`], какой бы ни передали: удалённый сервер не работает с файлами
    /// своей машины от имени агента.
    pub fn with_token_source(
        client: ApiClient,
        options: ServerOptions,
        files: FilePolicy,
        tokens: TokenSource,
    ) -> Result<Self, String> {
        let files = if options.local_machine {
            files
        } else {
            FilePolicy::disabled()
        };
        let registry = Arc::new(ToolRegistry::new(&options)?);
        let instructions = server_instructions(&options, &registry, &files);
        Ok(Self {
            session: Arc::new(Session::new(registry.clone())),
            inner: Arc::new(Inner {
                client,
                tokens,
                options,
                files,
                initial: registry,
                instructions,
                uploads: None,
            }),
        })
    }

    /// Подключает одноразовые адреса загрузки ([`crate::CREATE_UPLOAD_URL_TOOL`]). Их создаёт
    /// HTTP-транспорт и сам принимает на них файлы ([`Self::upload_stream`]).
    pub fn with_upload_urls(mut self, urls: Arc<UploadUrls>) -> Self {
        Arc::make_mut(&mut self.inner).uploads = Some(urls);
        self
    }

    /// Одноразовые адреса загрузки, если транспорт их подключил.
    pub fn upload_urls(&self) -> Option<&Arc<UploadUrls>> {
        self.inner.uploads.as_ref()
    }

    /// Тот же сервер с новой сессией: наборы инструментов — как при запуске. Клон
    /// (`clone()`) делит сессию с исходным сервером.
    pub fn new_session(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            session: Arc::new(Session::new(self.inner.initial.clone())),
        }
    }

    /// Текущий реестр инструментов сессии (снимок).
    pub fn registry(&self) -> Arc<ToolRegistry> {
        self.session.registry()
    }

    pub fn instructions(&self) -> &str {
        &self.inner.instructions
    }

    pub fn options(&self) -> &ServerOptions {
        &self.inner.options
    }

    /// Политика локальных файлов сервера.
    pub fn files(&self) -> &FilePolicy {
        &self.inner.files
    }

    /// Вызов инструмента без транспорта. Ошибки возвращаются как `isError: true`;
    /// `Err` — только для неизвестного инструмента (ошибка протокола).
    pub async fn call(&self, name: &str, args: JsonObject) -> Result<CallToolResult, ErrorData> {
        self.call_with_extensions(name, args, &Extensions::new())
            .await
    }

    /// Вызов инструмента с расширениями запроса MCP, из которых [`TokenSource::PerRequest`]
    /// берёт токен.
    pub async fn call_with_extensions(
        &self,
        name: &str,
        args: JsonObject,
        extensions: &Extensions,
    ) -> Result<CallToolResult, ErrorData> {
        let registry = self.registry();
        let Some(tool) = registry.get(name) else {
            // Известный, но выключенный инструмент: агенту объясняем, какой флаг нужен.
            return match registry.unavailable_reason(name) {
                Some(reason) => Ok(failure(&ToolError::new(reason))),
                None => Err(ErrorData::invalid_params(
                    format!("unknown tool `{name}`"),
                    None,
                )),
            };
        };
        // Поиск и управление наборами не обращаются к API: токен им не нужен.
        let local = match tool.kind {
            ToolKind::ApiSearch => Some(search(&args, &registry)),
            ToolKind::ListToolsets => Some(dynamic_toolsets::list(&args, &registry)),
            ToolKind::EnableToolset => Some(self.toggle_toolsets(&args, true)),
            ToolKind::DisableToolset => Some(self.toggle_toolsets(&args, false)),
            ToolKind::Operation(_)
            | ToolKind::Composite(_)
            | ToolKind::ApiRequest
            | ToolKind::Custom(_) => None,
        };
        if let Some(result) = local {
            return Ok(match result {
                Ok(text) => success(text),
                Err(err) => failure(&err),
            });
        }
        let client = match self.client_for(extensions) {
            Ok(client) => client,
            Err(err) => return Ok(failure(&err)),
        };
        let result = match tool.kind {
            ToolKind::Operation(op) => self.call_operation(&client, op, args).await,
            ToolKind::Composite(Composite::CreateUploadUrl) => {
                tracing::debug!(tool = name, "одноразовый адрес загрузки");
                match self.request_auth(extensions) {
                    Ok(auth) => {
                        upload_url::run(
                            self.inner.uploads.as_deref(),
                            &client,
                            &self.inner.files,
                            &auth,
                            &args,
                            extensions,
                        )
                        .await
                    }
                    Err(err) => Err(err),
                }
            }
            ToolKind::Composite(composite) => {
                tracing::debug!(tool = name, "составной инструмент");
                composite.call(&client, &self.inner.files, &args).await
            }
            ToolKind::ApiRequest => self.call_api_request(&client, args).await,
            // Пользовательские инструменты идут с клиентом этого вызова: в HTTP-режиме —
            // с токеном текущего запроса.
            ToolKind::Custom(index) => self.call_custom(&client, index, args).await,
            ToolKind::ApiSearch
            | ToolKind::ListToolsets
            | ToolKind::EnableToolset
            | ToolKind::DisableToolset => {
                // Обработаны выше, без токена.
                Err(ToolError::new(format!("tool `{name}` is not an API call")))
            }
        };
        // Защита в глубину: токен не попадает в ответ агенту, даже если API его отразил.
        // Тексты ошибок, кроме ответа Wiki (его обработал `execute`), проходят ещё и проверку
        // значений по форме: в них бывают пути и детали соединения.
        Ok(match result {
            Ok(text) => success(client.scrub_token(text)),
            Err(err) => {
                let err = ToolError::new(redact_message(
                    &client.scrub_token(err.message().to_owned()),
                ));
                tracing::debug!(tool = name, error = %err, "инструмент завершился ошибкой");
                failure(&err)
            }
        })
    }

    /// `enable_toolset`/`disable_toolset`: пересобирает реестр сессии. Замена делается под
    /// блокировкой записи от текущего реестра, чтобы параллельные вызовы не потеряли изменения.
    fn toggle_toolsets(&self, args: &JsonObject, enable: bool) -> Result<String, ToolError> {
        let mut guard = self
            .session
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let toggled = dynamic_toolsets::toggle(&guard, args, enable)?;
        if let Some(next) = toggled.registry {
            tracing::info!(
                toolsets = ?next.enabled_toolsets(),
                tools = next.len(),
                "наборы инструментов изменены"
            );
            *guard = Arc::new(next);
            drop(guard);
            self.session
                .changes
                .send_modify(|generation| *generation += 1);
        }
        Ok(toggled.text)
    }

    /// Клиент API с токеном для этого вызова. В HTTP-режиме — копия общего клиента
    /// с токеном текущего запроса; токен живёт только до конца вызова.
    fn client_for(&self, extensions: &Extensions) -> Result<Cow<'_, ApiClient>, ToolError> {
        match &self.inner.tokens {
            TokenSource::Client => Ok(Cow::Borrowed(&self.inner.client)),
            TokenSource::PerRequest(_) => {
                let auth = self.request_auth(extensions)?;
                self.client_with(auth).map(Cow::Owned)
            }
        }
    }

    /// Учётные данные вызова: из запроса ([`TokenSource::PerRequest`]) или клиента API.
    fn request_auth(&self, extensions: &Extensions) -> Result<RequestAuth, ToolError> {
        match &self.inner.tokens {
            TokenSource::Client => {
                let config = self.inner.client.config();
                let credentials = config
                    .credentials
                    .clone()
                    .ok_or_else(|| ToolError::new("this server has no Wiki token configured"))?;
                Ok(RequestAuth {
                    credentials,
                    org: config.org.clone(),
                })
            }
            TokenSource::PerRequest(credentials_of) => {
                credentials_of(extensions).ok_or_else(|| {
                    ToolError::new(
                        "this request carries no Wiki token: the MCP client must send it in \
                         the `Authorization` HTTP header",
                    )
                })
            }
        }
    }

    /// Копия общего клиента API с учётными данными `auth`.
    fn client_with(&self, auth: RequestAuth) -> Result<ApiClient, ToolError> {
        let copy = self
            .inner
            .client
            .with_credentials(auth.credentials)
            .map_err(|_| ToolError::new("the Wiki token in the request is malformed"))?;
        match auth.org {
            Some(org) => copy
                .with_org(org)
                .map_err(|_| ToolError::new("the Wiki organization in the request is malformed")),
            None => Ok(copy),
        }
    }

    /// Загружает файл из потока вложением страницы: то же, что `upload_attachment`, но части
    /// читаются из `reader` (тело HTTP-запроса на одноразовый адрес). `auth` — учётные данные
    /// этого HTTP-запроса; транспорт уже сверил их с адресом ([`UploadUrls::redeem`]).
    ///
    /// Результат — JSON-ответ прикрепления с `upload`; ответ и текст ошибки — без учётных
    /// данных, как у инструментов.
    pub async fn upload_stream(
        &self,
        auth: RequestAuth,
        target: &UploadTarget,
        reader: &mut (dyn AsyncRead + Unpin + Send),
    ) -> Result<String, StreamUploadError> {
        let client = self.client_with(auth).map_err(StreamUploadError::from)?;
        match run_stream(&client, &self.inner.files, target, reader).await {
            Ok(value) => Ok(client.scrub_token(value.to_string())),
            Err(err) => Err(StreamUploadError {
                failure: err.failure,
                message: redact_message(&client.scrub_token(err.message)),
            }),
        }
    }

    async fn call_operation(
        &self,
        client: &ApiClient,
        op: &'static OperationDef,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        // Фильтр проверяется до запроса: ошибка в нём не должна появиться после записи.
        // У операций с двоичным ответом аргумента `jq` нет: его отклонит `build_request`.
        let jq = if takes_jq(op) {
            take_jq(&mut args)?
        } else {
            None
        };
        self.run_operation(client, op, args, jq).await
    }

    /// Запрос операции `op` с уже проверенным фильтром.
    async fn run_operation(
        &self,
        client: &ApiClient,
        op: &'static OperationDef,
        args: JsonObject,
        jq: Option<JqFilter>,
    ) -> Result<String, ToolError> {
        let request = build_request(op, &args, &self.inner.files)?;
        tracing::debug!(tool = op.tool_name, path = %request.path, "вызов операции Wiki API");
        execute(client, request, jq.as_ref(), &self.inner.options.jq).await
    }

    /// `api_request`: уровень доступа и файлы проверяет [`prepare`], дальше — тот же путь,
    /// что у сгенерированных инструментов.
    async fn call_api_request(
        &self,
        client: &ApiClient,
        mut args: JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        let jq = take_jq(&mut args)?;
        let prepared = prepare(&args, inner.options.max_access, &inner.files)?;
        tracing::debug!(
            method = prepared.request.method.as_str(),
            path = %prepared.request.path,
            operation = prepared.operation.map(|op| op.tool_name),
            access = access_name(prepared.access),
            "api_request"
        );
        if jq.is_some() && prepared.request.save_to.is_some() {
            return Err(ToolError::new(
                "`jq` cannot be combined with `save_to`: a downloaded file is not filtered",
            ));
        }
        execute(client, prepared.request, jq.as_ref(), &inner.options.jq).await
    }

    /// Пользовательский инструмент: фиксированные аргументы добавляются к аргументам агента,
    /// дальше — тот же путь, что у инструмента операции (сборка запроса, файлы, ответ).
    /// Уровень доступа операции проверен при регистрации.
    async fn call_custom(
        &self,
        client: &ApiClient,
        index: usize,
        args: JsonObject,
    ) -> Result<String, ToolError> {
        let Some(tool) = self.inner.options.custom_tools.get(index) else {
            return Err(ToolError::new("custom tool is not available"));
        };
        let (args, jq) = tool.build_call(args)?;
        tracing::debug!(tool = %tool.name, operation = tool.operation.tool_name, "пользовательский инструмент");
        self.run_operation(client, tool.operation, args, jq).await
    }
}

/// Выполняет запрос и превращает ответ в текст для агента.
///
/// Единая точка постобработки ответа одиночных инструментов. Порядок: вырезание учётных
/// данных ([`execute_redacted`]) → фильтр `jq` → лимит размера → текст с пометкой о замене.
/// Фильтр получает только очищенный ответ, поэтому не может раскрыть вырезанные значения,
/// в том числе преобразованием (`@base64`), которое не поймала бы итоговая `scrub_token`.
///
/// С фильтром ответ читается до [`FILTER_INPUT_LIMIT`]: фильтр как раз уменьшает большой
/// ответ, а лимит ответа сервера (`max_response_bytes`) применяется к результату фильтра.
async fn execute(
    client: &ApiClient,
    request: ApiRequest,
    jq: Option<&JqFilter>,
    runner: &JqRunner,
) -> Result<String, ToolError> {
    let max = client.config().max_response_bytes;
    let Some(filter) = jq else {
        let (response, redactions) = execute_redacted(client, request).await?;
        return Ok(with_note(response_text(&response), &redactions));
    };
    let (response, redactions) =
        execute_redacted_with_limit(client, request, FILTER_INPUT_LIMIT.max(max)).await?;
    let text = filter_response(client, runner, filter, response, max).await?;
    Ok(with_note(text, &redactions))
}

/// Выполняет запрос и возвращает ответ с уже вырезанными учётными данными
/// ([`redact_response`]) и то, что скрыто. Ошибка — текст для агента, тоже без учётных
/// данных ([`redact_api_error`]) и с пометкой о замене.
///
/// Через неё идут все запросы к API: и одиночные инструменты, и шаги составных.
pub(crate) async fn execute_redacted(
    client: &ApiClient,
    request: ApiRequest,
) -> Result<(ApiResponse, Redactions), ToolError> {
    let limit = client.config().max_response_bytes;
    execute_redacted_with_limit(client, request, limit).await
}

/// Как [`execute_redacted`], но тело ответа читается до `limit` байт (для фильтра `jq`).
async fn execute_redacted_with_limit(
    client: &ApiClient,
    request: ApiRequest,
    limit: usize,
) -> Result<(ApiResponse, Redactions), ToolError> {
    let mut response = client
        .execute_with_limit(request, limit)
        .await
        .map_err(|err| {
            let (err, redactions) = redact_api_error(err);
            let message = ToolError::from(err).message().to_owned();
            ToolError::new(with_note(message, &redactions))
        })?;
    let redactions = redact_response(&mut response);
    Ok((response, redactions))
}

/// Текст с пометкой о вырезанных значениях, если что-то скрыто.
pub(crate) fn with_note(mut text: String, redactions: &Redactions) -> String {
    if let Some(note) = redactions.note() {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    text
}

/// Применяет фильтр к телу очищенного ответа. Курсоры постраничной выдачи из тела
/// ([`CURSOR_FIELDS`]) остаются рядом с результатом: `{"result", "pagination"}`, чтобы агент
/// не терял следующую страницу, даже если фильтр их отбросил. Ответ не в JSON возвращается
/// без фильтра, с пояснением.
async fn filter_response(
    client: &ApiClient,
    runner: &JqRunner,
    filter: &JqFilter,
    response: ApiResponse,
    max: usize,
) -> Result<String, ToolError> {
    let status = response.status;
    // Ответ уже получен: изменение (если это запись) выполнено, и агент должен это знать,
    // чтобы не повторять запрос из-за ошибки фильтра.
    let after_request = |problem: String| {
        ToolError::new(format!(
            "the API request succeeded (HTTP {status}), but {problem}. Repeating a request that \
             changes data would apply the change again"
        ))
    };
    let ApiResponse {
        body: ResponseBody::Json(mut value),
        ..
    } = response
    else {
        let mut text = response_text(&response);
        if text.len() > max {
            return Err(after_request(format!(
                "the response is not JSON and is larger than {max} bytes, so it was not returned"
            )));
        }
        text.push_str("\n\n[The `jq` filter was not applied: the response is not JSON.]");
        return Ok(text);
    };
    scrub_token_json(client, &mut value);
    let pagination = cursors(&value);
    let output = runner
        .run(filter, value, max)
        .await
        .map_err(|err| after_request(format!("the `jq` filter failed: {err}. Fix the filter")))?;
    if output.truncated {
        return Err(after_request(format!(
            "the filtered result is larger than {max} bytes; select fewer fields or items with \
             the filter, or narrow the request"
        )));
    }
    let Some(pagination) = pagination else {
        return Ok(output.text);
    };
    // Результат фильтра — компактный JSON (не обрезан), поэтому склейка даёт валидный JSON.
    Ok(format!(
        "{{\"result\":{},\"pagination\":{}}}",
        output.text,
        Value::Object(pagination)
    ))
}

/// Курсоры постраничной выдачи из тела ответа: есть, если в объекте верхнего уровня есть
/// хотя бы одно из полей [`CURSOR_FIELDS`] (значение `null` — тоже: последняя страница
/// выглядит так же, как остальные).
fn cursors(value: &Value) -> Option<Map<String, Value>> {
    let Value::Object(map) = value else {
        return None;
    };
    let found: Map<String, Value> = CURSOR_FIELDS
        .iter()
        .filter_map(|field| {
            map.get(*field)
                .map(|cursor| ((*field).to_owned(), cursor.clone()))
        })
        .collect();
    (!found.is_empty()).then_some(found)
}

/// Заменяет токен клиента во всех строках и ключах JSON до фильтра `jq`: итоговая
/// `scrub_token` ищет токен только в исходном виде, а фильтр мог бы его преобразовать.
fn scrub_token_json(client: &ApiClient, value: &mut Value) {
    match value {
        Value::String(text) => {
            let scrubbed = client.scrub_token(std::mem::take(text));
            *text = scrubbed;
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| scrub_token_json(client, item)),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut item) in entries {
                scrub_token_json(client, &mut item);
                map.insert(client.scrub_token(key), item);
            }
        }
        _ => {}
    }
}

impl ServerHandler for WikiServer {
    fn get_info(&self) -> ServerConfig {
        let capabilities = if self.inner.options.dynamic_toolsets {
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build()
        } else {
            ServerCapabilities::builder().enable_tools().build()
        };
        ServerConfig::new(capabilities)
            .with_server_info(
                Implementation::new(SERVER_NAME, env!("CARGO_PKG_VERSION"))
                    .with_title("Yandex Wiki"),
            )
            .with_instructions(self.inner.instructions.clone())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(self.registry().tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        let before = self.session.generation();
        let result = self
            .call_with_extensions(&request.name, args, &context.extensions)
            .await;
        // В сессии с `initialize` уведомление уходит сразу; в протоколе 2026-07-28 клиент
        // получает его через подписку (`listen`), а незапрошенные уведомления отклоняет.
        if self.session.generation() != before
            && uses_legacy_session(&context)
            && let Err(err) = context.peer.notify_tool_list_changed().await
        {
            tracing::warn!(error = %err, "не удалось отправить tools/list_changed");
        }
        result.map(Into::into)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        // Подписка нужна только для смены списка инструментов в динамическом режиме.
        self.inner
            .options
            .dynamic_toolsets
            .then(|| requested.supported_by(&self.get_info().capabilities))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), ErrorData> {
        if context.accepted().tools_list_changed != Some(true) {
            context.cancelled().await;
            return Ok(());
        }
        let mut changes = self.session.changes.subscribe();
        changes.mark_unchanged();
        loop {
            tokio::select! {
                () = context.cancelled() => return Ok(()),
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Ok(());
                    }
                    changes.mark_unchanged();
                    if let Err(err) = context.sink().notify_tool_list_changed().await {
                        tracing::debug!(error = %err, "подписка на tools/list_changed закрыта");
                        return Ok(());
                    }
                }
            }
        }
    }

    fn get_tool(&self, _name: &str) -> Option<Tool> {
        // Аргументы проверяет `build_request` с понятными агенту сообщениями;
        // проверку схемы на стороне rmcp не включаем.
        None
    }
}

/// Сессия с `initialize` (протокол до 2026-07-28): уведомления отправляются без подписки.
fn uses_legacy_session(context: &RequestContext<RoleServer>) -> bool {
    // Версии `YYYY-MM-DD` сравниваются лексикографически так же, как по времени.
    context
        .protocol_version()
        .is_none_or(|version| version.as_str() < ProtocolVersion::V_2026_07_28.as_str())
}

/// Инструкции сервера для агента (поле `instructions` в `initialize`).
pub fn server_instructions(
    options: &ServerOptions,
    registry: &ToolRegistry,
    files: &FilePolicy,
) -> String {
    let access = match options.max_access {
        AccessLevel::Read => {
            "This server is read-only: only tools that read data are available. Tools that \
             create or change data (pages, comments, grids, attachments) are enabled when the \
             user starts the server with --allow-write, and tools that delete data or change \
             access rights with --allow-destructive. If a task needs them, ask the user \
             instead of looking for workarounds."
        }
        AccessLevel::Write => {
            "This server can read, create and change data, but not delete it or change access \
             rights: those tools need the user to start the server with --allow-destructive. \
             Confirm changes with the user when they were not explicitly requested."
        }
        AccessLevel::Destructive => {
            "This server can read, change and delete data and change access rights to pages. \
             Deleting is irreversible: confirm with the user before calling destructive tools \
             unless they explicitly asked for it."
        }
    };
    let token = if options.local_machine {
        format!(
            "The Wiki token and the organization come from the server's profile; they are \
             never passed in tool arguments. If the API rejects the token (HTTP 401), ask the \
             user to store a new one with `wiki-mcp auth login --profile {profile}` (a Yandex \
             Cloud IAM token lives at most 12 hours); if it rejects the organization (HTTP \
             403 for every page), ask the user to check the organization ID of the profile.",
            profile = options.profile.as_deref().unwrap_or("<profile>"),
        )
    } else {
        // Исключение для команды загрузки — только если инструмент зарегистрирован.
        let exception = if registry.get(CREATE_UPLOAD_URL_TOOL).is_some() {
            format!(
                "; the only exception is the `Authorization` header of the `curl` command from \
                 `{CREATE_UPLOAD_URL_TOOL}`, through the environment variable named there"
            )
        } else {
            String::new()
        };
        format!(
            "The MCP client sends the user's Wiki token with every request (the `Authorization` \
             header from its configuration: `OAuth <token>` for an OAuth token, `Bearer \
             <token>` for a Yandex Cloud IAM token) together with the organization (`X-Org-Id` \
             or `X-Cloud-Org-Id`, unless the server sets it); the server does not store them. \
             Never read the token from the client configuration, print it, or pass it to \
             tools, commands or files{exception}. If the API rejects the token (HTTP 401), ask \
             the user to replace it in the MCP client configuration; if it rejects the \
             organization (HTTP 403), ask the user to check the organization header there."
        )
    };
    let files_line = if options.local_machine {
        format!(
            "- Local files: file arguments (`file` of `{upload}` and `upload_file_part`, \
             `save_to` of the `download_attachment*` tools and of `api_request`) take a path \
             (absolute, or relative to the server's working directory) inside the allowed \
             directories: {roots}. Other paths and symbolic links leading outside are rejected; \
             the user can allow more with --allow-path. Downloads never overwrite existing \
             files. To attach a local file to a page, call `{upload}` with the page `idx` and \
             the file path: it creates the upload session, uploads the parts, finishes the \
             session and attaches the file in one call, and aborts the session if a step \
             fails. Use the manual steps only if `{upload}` cannot be used: \
             `create_upload_session` (file name and size), `upload_file_part` for each part \
             (the whole file as part 1 if it is at most 16 MiB, otherwise parts of 5-16 MiB), \
             `finish_upload_session`, then `attach_uploaded_files`; abort a session you do not \
             finish with `abort_upload_session`.\n",
            upload = UPLOAD_ATTACHMENT_TOOL,
            roots = files.describe_roots(),
        )
    } else if registry.get(CREATE_UPLOAD_URL_TOOL).is_some() {
        format!(
            "- Remote server: this server runs on another machine (HTTP mode) and cannot read \
             or write files on your machine, so there are no download tools and no file \
             arguments. To attach a file from your machine to a page, call \
             `{CREATE_UPLOAD_URL_TOOL}` with the page `idx`, the file name and its exact size \
             in bytes, then run the returned `curl` command: it sends the file to a one-time \
             URL of this server, which uploads it to Wiki and attaches it. Never pass file \
             content (for example base64) in tool arguments. To download a file, ask the user \
             to do it in Wiki.\n"
        )
    } else {
        "- Remote server: this server runs on another machine (HTTP mode) and cannot read or \
         write files on your machine, so there are no upload or download tools. Ask the user \
         to attach or download files in Wiki.\n"
            .to_owned()
    };
    // Составные инструменты называем, только если они зарегистрированы; в динамическом
    // режиме — если их можно включить набором (уровень доступа и локальные файлы).
    let composites: Vec<String> = if registry.is_dynamic() {
        Composite::ALL
            .iter()
            .filter(|composite| {
                registry.max_access().allows(composite.access())
                    && (registry.local_files() || !composite.uses_local_files())
            })
            .map(|composite| format!("`{}`", composite.name()))
            .collect()
    } else {
        registry
            .iter()
            .filter(|tool| matches!(tool.kind, ToolKind::Composite(_)))
            .map(|tool| format!("`{}`", tool.tool.name))
            .collect()
    };
    let composites = match composites.as_slice() {
        [] => String::new(),
        [one] => format!(", except the composite tool {one}, which runs several calls"),
        many => format!(
            ", except the composite tools {}, which run several calls",
            many.join(" and ")
        ),
    };
    let enabled = registry.enabled_toolsets();
    let toolsets = if registry.is_dynamic() {
        format!(
            "Toolsets are loaded on demand; enabled at start: {}. Tools of other toolsets are \
             not registered until you enable them, and tools above the access level are never \
             registered; calling them explains what to do.",
            if enabled.is_empty() {
                "none".to_owned()
            } else {
                enabled.join(", ")
            }
        )
    } else {
        format!(
            "Enabled toolsets: {}; tools of other toolsets (and above the access level) are not \
             registered, and calling them explains which flag enables them.",
            enabled.join(", ")
        )
    };
    let dynamic = if registry.is_dynamic() {
        "- Toolsets: tools are loaded on demand, by toolset. Call `list_toolsets` to see the \
         toolsets, then `enable_toolset` to load the tools you need (`disable_toolset` unloads \
         them); `api_search` shows the toolset of an operation. Composite tools come with their \
         toolset (`upload_attachment` with `attachments`, `wait_for_operation` with `pages`). \
         The client is notified that the tool list changed; if the enabled tools do not appear \
         in your tool list, call the same operations with `api_request`.\n"
    } else {
        ""
    };
    let custom = if registry
        .iter()
        .any(|tool| matches!(tool.kind, ToolKind::Custom(_)))
    {
        "- Custom tools: some tools are defined by the user (their description says so): each \
         calls one API operation with arguments fixed by the user and an optional built-in \
         filter. Prefer them when they fit the task; fixed arguments cannot be passed.\n"
    } else {
        ""
    };
    format!(
        "Yandex Wiki MCP server ({level} access). Every tool calls one Wiki Public API \
         operation and returns its JSON response{composites}. {toolsets}\n\
         \n\
         - Access: {access}\n\
         - Pages: a page is addressed either by its slug — the path of the page inside Wiki \
         without the host, like `users/name/page` or `team/project` (take it from the page \
         URL; never pass the full URL) — or by `idx`, its numeric ID (the `id` field of \
         `get_page`). Tools named `*_by_id` and most page tools take `idx`; `get_page` and \
         `list_page_descendants` take `slug`. For grids `idx` is the grid ID (a UUID) from \
         `create_grid` or `list_page_grids`.\n\
         - Arguments: path and query parameters are plain arguments named as in the API \
         (`idx`, `slug`, `fields`, `page_size`, ...); a JSON request body goes in the `body` \
         argument as a JSON object. Unknown arguments are rejected. `fields` adds optional \
         blocks to the response, for example `content` for the page text.\n\
         - Pagination: list responses carry `next_cursor` (and `prev_cursor`) in the body; \
         to get the next page call the same tool again with `cursor` set to it. The list \
         ends when `next_cursor` is missing or empty. Use `page_size` to change the page \
         size.\n\
         - Filtering: API tools and `api_request` accept an optional `jq` argument, a jq filter \
         (jaq dialect) applied to the JSON response body, for example \
         `.results[] | {{id, slug, title}}` for a list of pages or \
         `[.results[] | select(.title | test(\"plan\"; \"i\")) | .slug]`. Use it to keep large \
         responses small. Cursors stay outside the filter: when the response has \
         `next_cursor` or `prev_cursor`, the result is wrapped as \
         `{{\"result\": <filter output>, \"pagination\": {{\"next_cursor\": ..., \"prev_cursor\": ...}}}}`. \
         Credentials are redacted before the filter runs; the size limit applies to the \
         filtered result. Several outputs are returned as a JSON array, no output as `[]`. \
         Filters cannot read the environment, files or other inputs. A broken filter is \
         reported before the request is sent; a filter that fails at runtime (after the \
         request succeeded) returns an error that says so: do not repeat a request that \
         changed data just to fix the filter. Composite tools and file downloads take no \
         `jq`.\n\
         - Background operations: copying a page (`clone_page`) or a grid (`clone_grid`) and \
         moving pages (`move_page`) run in the background and return `operation.type` and \
         `operation.id`. Call `{wait}` with them to wait for the final status instead of \
         repeating the operation; the `get_*_operation_status` tools return the current \
         status once.\n\
         - Finding operations: `api_search` searches all Wiki API operations and the \
         composite tools by words in English or Russian (names, summaries, paths), including \
         tools not enabled here, and tells which flag enables them. Use it when no listed \
         tool fits. `api_request` calls any API path (`/v1/...`) within the server's access \
         level; toolsets do not limit it. Prefer the dedicated tool when it is registered: it \
         validates arguments and documents the schema.\n\
         {dynamic}\
         {custom}\
         - Large responses are rejected with an error; narrow the request (fewer `fields`, \
         a smaller `page_size`, stricter filters) or select less with `jq`.\n\
         - Reference: tool descriptions end with a link to the Wiki API reference when there \
         is one; read it when the arguments or the request body are unclear.\n\
         {files_line}\
         - Secrets: {token} Never ask the user to paste tokens, passwords or other secrets \
         into the conversation. Values that look like credentials are replaced with \
         \"[redacted]\" in every response, including errors, and a note says when that \
         happened: fields named like passwords, tokens or keys, and token-, key- or \
         password-shaped values inside any text, including page content and grid cells \
         (OAuth and IAM tokens, `Authorization` values, private keys, passwords in URLs). Do \
         not try to recover hidden values.\n\
         - Personal data: responses contain personal data of Wiki users (names, logins, \
         comments, page texts). Use it only for the current task: do not copy it into logs, \
         commits, code, test fixtures, other pages or other systems unless the user asks for \
         exactly that.",
        level = access_name(options.max_access),
        wait = WAIT_FOR_OPERATION_TOOL,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instructions(options: &ServerOptions) -> String {
        let registry = ToolRegistry::new(options).unwrap();
        let files = if options.local_machine {
            FilePolicy::new(&[]).unwrap()
        } else {
            FilePolicy::disabled()
        };
        server_instructions(options, &registry, &files)
    }

    #[test]
    fn instructions_depend_on_access() {
        let read = instructions(&ServerOptions::default());
        assert!(read.contains("(read-only access)"), "{read}");
        assert!(read.contains("--allow-write") && read.contains("--allow-destructive"));
        assert!(
            read.contains("Enabled toolsets: pages, attachments, comments, grids;"),
            "{read}"
        );
        assert!(read.contains("auth login --profile <profile>"), "{read}");
        assert!(read.contains("`users/name/page`"), "{read}");
        assert!(read.contains("numeric ID"), "{read}");
        assert!(read.contains("next_cursor"), "{read}");
        assert!(read.contains("`api_search`"), "{read}");
        assert!(read.contains("`api_request`"), "{read}");
        assert!(read.contains("never passed in tool arguments"), "{read}");
        assert!(
            read.contains("Never ask the user to paste tokens"),
            "{read}"
        );
        assert!(read.contains("allowed directories: none"), "{read}");
        assert!(read.contains("`create_upload_session`"), "{read}");
        assert!(read.contains("call `upload_attachment`"), "{read}");
        assert!(read.contains("Call `wait_for_operation`"), "{read}");
        assert!(
            read.contains("except the composite tool `wait_for_operation`, which"),
            "{read}"
        );
        assert!(read.contains("[redacted]"), "{read}");
        assert!(
            read.contains("Personal data") && read.contains("commits"),
            "{read}"
        );

        let write = instructions(&ServerOptions {
            max_access: AccessLevel::Write,
            toolsets: Some(vec!["grids".into()]),
            profile: Some("codex".into()),
            ..ServerOptions::default()
        });
        assert!(write.contains("(write access)"), "{write}");
        assert!(write.contains("Enabled toolsets: grids;"), "{write}");
        assert!(write.contains("auth login --profile codex"), "{write}");
        assert!(!write.contains("except the composite"), "{write}");
        let write = instructions(&ServerOptions {
            max_access: AccessLevel::Write,
            ..ServerOptions::default()
        });
        assert!(
            write.contains("composite tools `upload_attachment` and `wait_for_operation`"),
            "{write}"
        );

        let all = instructions(&ServerOptions {
            max_access: AccessLevel::Destructive,
            ..ServerOptions::default()
        });
        assert!(all.contains("(destructive access)"), "{all}");
        assert!(all.contains("irreversible"), "{all}");
    }

    #[test]
    fn remote_instructions_have_no_local_features() {
        let text = instructions(&ServerOptions {
            local_machine: false,
            profile: Some("codex".into()),
            ..ServerOptions::default()
        });
        for absent in ["auth login", "--allow-path", "allowed directories"] {
            assert!(!text.contains(absent), "{absent}: {text}");
        }
        assert!(text.contains("HTTP mode"), "{text}");
        assert!(text.contains("`Authorization`"), "{text}");
        assert!(!text.contains("`upload_attachment`"), "{text}");
    }

    #[test]
    fn instructions_describe_filter_and_dynamic_toolsets() {
        let text = instructions(&ServerOptions::default());
        assert!(text.contains("- Filtering:"), "{text}");
        assert!(text.contains("\"pagination\""), "{text}");
        assert!(!text.contains("`list_toolsets`"), "{text}");
        assert!(!text.contains("Custom tools"), "{text}");

        let dynamic = instructions(&ServerOptions {
            max_access: AccessLevel::Write,
            dynamic_toolsets: true,
            ..ServerOptions::default()
        });
        assert!(dynamic.contains("enabled at start: none"), "{dynamic}");
        assert!(dynamic.contains("`list_toolsets`"), "{dynamic}");
        // Составные инструменты названы, хотя их наборы ещё не включены.
        assert!(
            dynamic.contains("composite tools `upload_attachment` and `wait_for_operation`"),
            "{dynamic}"
        );
        assert!(!dynamic.contains("Enabled toolsets:"), "{dynamic}");
    }

    #[test]
    fn cursors_are_taken_from_the_top_level() {
        let value = serde_json::json!({"results": [], "next_cursor": "c2", "prev_cursor": null});
        let found = cursors(&value).unwrap();
        assert_eq!(
            Value::Object(found),
            serde_json::json!({"next_cursor": "c2", "prev_cursor": null})
        );
        assert!(cursors(&serde_json::json!({"id": 1})).is_none());
        assert!(cursors(&serde_json::json!([{"next_cursor": "x"}])).is_none());
    }
}
