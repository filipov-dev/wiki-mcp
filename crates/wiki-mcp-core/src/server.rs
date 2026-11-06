//! Обработчик MCP-сервера `rmcp` без привязки к транспорту.
//!
//! Набор инструментов динамический (строится из таблицы операций), поэтому `ServerHandler`
//! реализован вручную, без макросов `#[tool]`. Транспорт подключает бинарник:
//! `WikiServer::new(...)?.serve(stdio())`.
//!
//! Токен Wiki для вызова берётся из [`TokenSource`]: у stdio-сервера это токен клиента
//! API из системного хранилища, у HTTP-сервера (появится позже) — токен текущего
//! HTTP-запроса агента, который транспорт кладёт в расширения запроса MCP. Ядро не знает,
//! как устроен HTTP: функцию извлечения токена передаёт бинарник. Организация — из клиента
//! API (профиль), а в HTTP-режиме — из того же запроса, если транспорт её передал.

use std::borrow::Cow;
use std::fmt;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, Extensions, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use wiki_api::executor::{ApiClient, ApiRequest, ApiResponse, Credentials, OrgId};
use wiki_api::spec::{AccessLevel, OperationDef};

use crate::ServerOptions;
use crate::api_request::prepare;
use crate::api_search::search;
use crate::composite::{UPLOAD_ATTACHMENT_TOOL, WAIT_FOR_OPERATION_TOOL};
use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::redact::{Redactions, redact_api_error, redact_message, redact_response};
use crate::registry::{ToolKind, ToolRegistry, access_name};
use crate::request::build_request;
use crate::response::{failure, response_text, success};

/// Имя сервера в `initialize`.
pub const SERVER_NAME: &str = "wiki-mcp";

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

/// MCP-сервер Wiki. Дёшево клонируется.
#[derive(Debug, Clone)]
pub struct WikiServer {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    client: ApiClient,
    tokens: TokenSource,
    options: ServerOptions,
    files: FilePolicy,
    registry: ToolRegistry,
    instructions: String,
}

impl WikiServer {
    /// Создаёт сервер с токеном клиента API ([`TokenSource::Client`]).
    /// Ошибка — неизвестный набор инструментов в `options.toolsets`.
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
        let registry = ToolRegistry::new(&options)?;
        let instructions = server_instructions(&options, &registry, &files);
        Ok(Self {
            inner: Arc::new(Inner {
                client,
                tokens,
                options,
                files,
                registry,
                instructions,
            }),
        })
    }

    /// Реестр инструментов сервера.
    pub fn registry(&self) -> &ToolRegistry {
        &self.inner.registry
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
        let registry = &self.inner.registry;
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
        if let ToolKind::ApiSearch = tool.kind {
            // Поиск читает только таблицу операций: токен не нужен.
            return Ok(match search(&args, registry) {
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
            ToolKind::Composite(composite) => {
                tracing::debug!(tool = name, "составной инструмент");
                composite.call(&client, &self.inner.files, &args).await
            }
            ToolKind::ApiRequest => self.call_api_request(&client, args).await,
            ToolKind::ApiSearch => Err(ToolError::new("`api_search` is handled above")),
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

    /// Клиент API с токеном для этого вызова. В HTTP-режиме — копия общего клиента
    /// с токеном текущего запроса; токен живёт только до конца вызова.
    fn client_for(&self, extensions: &Extensions) -> Result<Cow<'_, ApiClient>, ToolError> {
        let client = &self.inner.client;
        match &self.inner.tokens {
            TokenSource::Client => Ok(Cow::Borrowed(client)),
            TokenSource::PerRequest(credentials_of) => {
                let auth = credentials_of(extensions).ok_or_else(|| {
                    ToolError::new(
                        "this request carries no Wiki token: the MCP client must send it in \
                         the `Authorization` HTTP header",
                    )
                })?;
                let copy = client
                    .with_credentials(auth.credentials)
                    .map_err(|_| ToolError::new("the Wiki token in the request is malformed"))?;
                let copy = match auth.org {
                    Some(org) => copy.with_org(org).map_err(|_| {
                        ToolError::new("the Wiki organization in the request is malformed")
                    })?,
                    None => copy,
                };
                Ok(Cow::Owned(copy))
            }
        }
    }

    async fn call_operation(
        &self,
        client: &ApiClient,
        op: &'static OperationDef,
        args: JsonObject,
    ) -> Result<String, ToolError> {
        let request = build_request(op, &args, &self.inner.files)?;
        tracing::debug!(tool = op.tool_name, path = %request.path, "вызов операции Wiki API");
        execute(client, request).await
    }

    async fn call_api_request(
        &self,
        client: &ApiClient,
        args: JsonObject,
    ) -> Result<String, ToolError> {
        let inner = &self.inner;
        let prepared = prepare(&args, inner.options.max_access, &inner.files)?;
        tracing::debug!(
            method = prepared.request.method.as_str(),
            path = %prepared.request.path,
            operation = prepared.operation.map(|op| op.tool_name),
            access = access_name(prepared.access),
            "api_request"
        );
        execute(client, prepared.request).await
    }
}

/// Выполняет запрос и превращает ответ в текст для агента.
///
/// Единая точка постобработки ответа для всех инструментов: вырезание учётных данных
/// ([`execute_redacted`]), затем текст с пометкой о замене.
async fn execute(client: &ApiClient, request: ApiRequest) -> Result<String, ToolError> {
    let (response, redactions) = execute_redacted(client, request).await?;
    Ok(with_note(response_text(&response), &redactions))
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
    let mut response = client.execute(request).await.map_err(|err| {
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

impl ServerHandler for WikiServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
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
        Ok(ListToolsResult::with_all_items(self.inner.registry.tools()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let args = request.arguments.unwrap_or_default();
        self.call_with_extensions(&request.name, args, &context.extensions)
            .await
            .map(Into::into)
    }

    fn get_tool(&self, _name: &str) -> Option<Tool> {
        // Аргументы проверяет `build_request` с понятными агенту сообщениями;
        // проверку схемы на стороне rmcp не включаем.
        None
    }
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
        "The MCP client sends the user's Wiki token with every request (the `Authorization` \
         header from its configuration: `OAuth <token>` for an OAuth token, `Bearer <token>` \
         for a Yandex Cloud IAM token) together with the organization (`X-Org-Id` or \
         `X-Cloud-Org-Id`, unless the server sets it); the server does not store them. Never \
         read the token from the client configuration, print it, or pass it to tools, \
         commands or files. If the API rejects the token (HTTP 401), ask the user to replace \
         it in the MCP client configuration; if it rejects the organization (HTTP 403), ask \
         the user to check the organization header there."
            .to_owned()
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
    } else {
        "- Remote server: this server runs on another machine (HTTP mode) and cannot read or \
         write files on your machine, so there are no upload or download tools. Ask the user \
         to attach or download files in Wiki.\n"
            .to_owned()
    };
    // Составные инструменты называем, только если они зарегистрированы.
    let composites: Vec<String> = registry
        .iter()
        .filter(|tool| matches!(tool.kind, ToolKind::Composite(_)))
        .map(|tool| format!("`{}`", tool.tool.name))
        .collect();
    let composites = match composites.as_slice() {
        [] => String::new(),
        [one] => format!(", except the composite tool {one}, which runs several calls"),
        many => format!(
            ", except the composite tools {}, which run several calls",
            many.join(" and ")
        ),
    };
    format!(
        "Yandex Wiki MCP server ({level} access). Every tool calls one Wiki Public API \
         operation and returns its JSON response{composites}. Enabled toolsets: {toolsets}; tools of other \
         toolsets (and above the access level) are not registered, and calling them explains \
         which flag enables them.\n\
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
         - Large responses are rejected with an error; narrow the request (fewer `fields`, \
         a smaller `page_size`, stricter filters).\n\
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
        toolsets = registry.enabled_toolsets().join(", "),
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
}
