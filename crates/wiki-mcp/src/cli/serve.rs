//! `wiki-mcp serve` — запуск MCP-сервера через stdio или HTTP (`--http`).
//!
//! stdio: stdout занят протоколом MCP, всё остальное (логи, ошибки запуска) — только в stderr.
//! Токен и организация берутся только из профиля: токен — из системного хранилища, тип
//! токена и организация — из `profiles.toml`. Профиль с IAM-токенами из `yc` или по ключу
//! сервисного аккаунта получает токены сам при запросах и обновляет их до истечения срока
//! (`wiki_api::iam::IamTokens`), токены держатся только в памяти. Переменные окружения токен
//! не задают.
//!
//! Права (уровень доступа и каталоги `allow_path`) — из настроек профиля; флаги командной
//! строки их только сужают, на macOS права сверх чтения ещё и должны быть одобрены
//! владельцем ([`mcp_kit::approval`]). Наборы: флаги, иначе профиль, иначе по умолчанию.
//!
//! HTTP ([`http_server`]): профилей и хранилища нет, токен (и организация, если её не задаёт
//! флаг сервера) приходит в заголовках каждого запроса агента и нигде не сохраняется. Уровень
//! доступа и наборы задаёт оператор флагами `serve --http`: права профиля и одобрение на macOS
//! относятся только к stdio (`serve`, `call`, `connect`). Токен в HTTP-режиме виден агенту,
//! поэтому уровень сервера — страховка от ошибок, а не граница. Локальные файлы выключены
//! (`ServerOptions::local_machine = false`).
//!
//! `serve --status` печатает действующие настройки (профиль, тип токена, организация, уровень
//! доступа и его одобрение, наборы, разрешённые каталоги) и выходит, не запуская сервер и не
//! обращаясь к API. С `--http` он недоступен: остальные настройки HTTP-сервера — флаги запуска.
//!
//! Локальные файлы (загрузка части файла, скачивание вложений в `save_to`) разрешены только
//! в рабочем каталоге, временном каталоге и каталогах `allow_path` профиля (`FilePolicy`).
//!
//! Пользовательские инструменты ([`crate::cli::tools`]) читаются при запуске: в stdio — файл
//! пользователя и `--tools-file`, в HTTP — только `--tools-file`. Фильтры `jq` выполняются в
//! дочернем процессе этого бинарника ([`crate::cli::jq_worker`]). Динамические наборы
//! (`--dynamic-toolsets`) есть только в stdio: HTTP-режим не хранит сессий.

mod http_server;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::Args;
use mcp_kit::approval::{self, ApprovalState, ApprovalStore, Resolved};
use mcp_kit::{AccessRequest, ProfileAccess};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;
use wiki_api::auth::{KEYRING_SERVICE, KeyringStore, TokenStore, service_account_key_account};
use wiki_api::executor::{ApiClient, ClientConfig, Credentials, OrgId};
use wiki_api::iam::IamTokens;
use wiki_api::{AccessLevel, OrgKind, Organization};
use wiki_mcp_core::{
    ALL_TOOLSETS, FilePolicy, ServerOptions, ToolRegistry, WikiServer, access_from_flags,
    access_name, default_toolsets, normalize_toolsets,
};

use crate::cli::{jq_worker, tools};
use crate::clients::EXE_STEM;
use crate::profiles::{
    DEFAULT_PROFILE, IamSourceKind, ProfileAuth, ProfileRepo, ProfileSettings, TokenType,
    load_profile_with, validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};

/// Таймаут запроса к API по умолчанию, секунды.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Лимит тела ответа, возвращаемого агенту, по умолчанию: 256 KiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Уровень логов в stderr, если не задан ни `--log-level`, ни `RUST_LOG`.
const DEFAULT_LOG_LEVEL: &str = "warn";

#[derive(Debug, Clone, Args)]
#[command(after_help = "\
Access: the access level and the extra directories (allow_path) are settings of the profile; \
change them with `wiki-mcp ui`. --allow-write, --allow-destructive and --allow-path can only \
narrow them: asking for more than the profile allows is an error. On macOS, access above \
read-only takes effect only after the owner approved it on this Mac (`wiki-mcp ui` asks for \
Touch ID or the macOS password); without the approval the server runs read-only and says so in \
stderr. --allow-destructive implies --allow-write; changing access rights to pages needs the \
destructive level.

Toolsets: --toolsets wins; without it the toolsets saved for the profile are used, otherwise \
the default ones: pages, attachments, comments, grids. The `access` (personal access rights to \
pages) and `extra` (operations missing from the Wiki API reference) toolsets are enabled only \
explicitly. Use --toolsets all for every toolset; the error for an unknown name lists the \
available ones.

Dynamic toolsets: with --dynamic-toolsets the server starts with only `api_search`, \
`api_request`, `list_toolsets`, `enable_toolset` and `disable_toolset`, plus the toolsets given \
with --toolsets and the custom tools; the agent enables toolsets on demand (composite tools come \
with their toolset: upload_attachment with attachments, wait_for_operation with pages) and the \
client is notified that the tool list changed. The access level still applies. Use it with \
clients that load all tool descriptions up front and reload the list on \
`notifications/tools/list_changed`. Not available in HTTP mode, which keeps no sessions.

Response filter: every API tool and `api_request` accept an optional `jq` argument (jaq \
dialect) applied to the JSON response after credentials are redacted; --max-response-bytes then \
limits the filtered result. Cursors of list responses (`next_cursor`, `prev_cursor`) are kept \
next to the filter output. Filters run in a child process without environment, files or \
network, with a timeout.

Custom tools: tools defined in tools.toml next to profiles.toml (~/.config/wiki-mcp/tools.toml) \
are registered when the file exists; files from other places (for example, a project \
repository) only with --tools-file. A custom tool calls one API operation with fixed arguments \
and an optional `jq` filter, and has the access level of that operation; toolsets do not limit \
it. Check the files with `wiki-mcp tools check`.

Token and organization: read only from the profile (the token from the system keychain, its \
type and the organization from profiles.toml); store them with `wiki-mcp auth login`. An OAuth \
token is sent as `Authorization: OAuth <token>`, a Yandex Cloud IAM token as \
`Authorization: Bearer <token>`. A profile set up with `--iam-source yc` or \
`--iam-source service-account-key` gets IAM tokens itself (from `yc iam create-token` or for \
the service account key in the system keychain), renews them before they expire and keeps \
them in memory only, so the server runs for any time without a manual token update. The token \
is never read from environment variables or command-line arguments.

Local files: file arguments of tools (`file` of upload_attachment and upload_file_part, `save_to` for attachment \
downloads) must be inside the server's working directory (unless it is the filesystem root or \
the home directory), the system temp directory, or an extra directory of the profile \
(allow_path; --allow-path keeps only the given ones). The temp directory is allowed for scratch files and downloads; other programs share it, so keep \
nothing private there. Paths are checked after resolving `..` and symbolic links: a link that \
leads outside these directories is rejected, and existing files are never overwritten.

Logs go to stderr only (stdout carries the MCP protocol). The level comes from --log-level, \
otherwise from RUST_LOG, otherwise `warn`.

HTTP mode (--http): the server listens on --listen (default 127.0.0.1:8767) and serves MCP at \
/mcp (Streamable HTTP, stateless, JSON responses) and a health check at /health. There are no \
profiles and no stored tokens: every request must carry the agent's Wiki token in \
`Authorization: OAuth <token>` (OAuth token) or `Authorization: Bearer <token>` (Yandex Cloud \
IAM token), and the organization in `X-Org-Id` or `X-Cloud-Org-Id` unless the server is bound \
to one with --org-id or --cloud-org-id (then a different organization in a request is \
rejected). Access level and toolsets come from the command line only: --allow-write and \
--allow-destructive set the access of the HTTP server (profiles and their access are for stdio \
only). Local files are disabled: there are no upload and download tools. The Host header must \
be a loopback name, the \
--listen address or an --allowed-host; browser requests are rejected unless their Origin is \
allowed with --allowed-origin. Outside loopback, put the server behind a TLS reverse proxy.")]
pub struct ServeArgs {
    /// Profile with the token, organization and access [default: default]. Not available
    /// with --http.
    #[arg(long, conflicts_with = "http")]
    pub profile: Option<String>,
    /// Limit the access to write (the profile must allow it). With --http: allow tools that
    /// create or modify data.
    #[arg(long)]
    pub allow_write: bool,
    /// Limit the access to destructive: also deleting data and changing access rights (the
    /// profile must allow it; implies --allow-write). With --http: allow those tools.
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable, or `all`. With --dynamic-toolsets: the toolsets
    /// enabled at start.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Load toolsets on demand: start with only the search, request and toolset tools, and
    /// let the agent enable toolsets with `enable_toolset`. Not available with --http.
    #[arg(long, conflicts_with = "http")]
    pub dynamic_toolsets: bool,
    /// Additional custom tools file (same format as ~/.config/wiki-mcp/tools.toml).
    /// Repeatable. In HTTP mode only these files are read.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
    /// Limit the extra directories of the profile to this one (it must be inside a directory
    /// of the profile). Repeatable. Not available with --http.
    #[arg(long, value_name = "DIR", conflicts_with = "http")]
    pub allow_path: Vec<PathBuf>,
    /// Timeout of one API request, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Maximum size of an API response returned to the agent; larger responses are an error.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    pub max_response_bytes: usize,
    /// Log level or filter for stderr, e.g. `info` or `wiki_mcp_core=debug`.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,
    /// Print the effective profile, token type, organization, access level, toolsets and
    /// allowed directories, then exit without starting the server. Not available with --http.
    #[arg(long, conflicts_with = "http")]
    pub status: bool,
    /// Serve MCP over HTTP instead of stdio; each request carries the agent's token.
    #[arg(long)]
    pub http: bool,
    /// Address and port to listen on in HTTP mode [default: 127.0.0.1:8767].
    #[arg(long, value_name = "IP:PORT", requires = "http")]
    pub listen: Option<SocketAddr>,
    /// Extra allowed Host header value in HTTP mode: a name (any port) or name:port.
    /// Repeatable.
    #[arg(long, value_name = "HOST", requires = "http")]
    pub allowed_host: Vec<String>,
    /// Browser Origin allowed in HTTP mode, e.g. http://localhost:6274 (`:*` for any port).
    /// Repeatable. Requests without an Origin header are always allowed.
    #[arg(long, value_name = "ORIGIN", requires = "http")]
    pub allowed_origin: Vec<String>,
    /// HTTP mode: Yandex 360 for Business organization ID for every request (sent as
    /// X-Org-Id). Requests may omit the organization header; another organization is rejected.
    #[arg(
        long,
        value_name = "ID",
        requires = "http",
        conflicts_with = "cloud_org_id"
    )]
    pub org_id: Option<String>,
    /// HTTP mode: Yandex Cloud Organization ID for every request (sent as X-Cloud-Org-Id).
    #[arg(long, value_name = "ID", requires = "http")]
    pub cloud_org_id: Option<String>,
    /// Maximum size of an MCP request body in HTTP mode; larger requests get HTTP 413
    /// [default: 1048576].
    #[arg(long, value_name = "BYTES", requires = "http")]
    pub max_request_bytes: Option<usize>,
}

impl ServeArgs {
    /// `serve` без флагов для профиля `profile` (stdio): так сервер собирают `call` и `ui`.
    pub fn for_profile(profile: Option<String>) -> Self {
        Self {
            profile,
            allow_write: false,
            allow_destructive: false,
            toolsets: Vec::new(),
            dynamic_toolsets: false,
            tools_file: Vec::new(),
            allow_path: Vec::new(),
            timeout: DEFAULT_TIMEOUT_SECS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            log_level: None,
            status: false,
            http: false,
            listen: None,
            allowed_host: Vec::new(),
            allowed_origin: Vec::new(),
            org_id: None,
            cloud_org_id: None,
            max_request_bytes: None,
        }
    }

    pub fn profile(&self) -> &str {
        self.profile.as_deref().unwrap_or(DEFAULT_PROFILE)
    }

    /// Что просят флаги доступа.
    fn access_request(&self) -> AccessRequest {
        AccessRequest::from_flags(
            self.allow_write,
            self.allow_destructive,
            self.allow_path.clone(),
        )
    }

    /// Проверка аргументов до обращения к хранилищу.
    fn validate(&self) -> anyhow::Result<()> {
        if self.timeout == 0 {
            bail!("--timeout must be greater than 0");
        }
        if self.max_response_bytes == 0 {
            bail!("--max-response-bytes must be greater than 0");
        }
        if self.max_request_bytes == Some(0) {
            bail!("--max-request-bytes must be greater than 0");
        }
        validate_profile_name(self.profile())
    }
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    init_logging(args.log_level.as_deref())?;
    args.validate()?;
    if args.http {
        let setup = HttpSetup::load(&args)?;
        tracing::info!(
            access = ?setup.options.max_access,
            toolsets = ?setup.options.toolsets,
            custom_tools = setup.options.custom_tools.len(),
            "настройки MCP-сервера Wiki (HTTP)"
        );
        let listen = setup.listen;
        let (server, http) = setup.server()?;
        return http_server::run(server, listen, http).await;
    }
    let repo = ProfileRepo::open_default()?;
    let approvals = approval::platform_store(EXE_STEM);
    if args.status {
        return status(
            &KeyringStore,
            &repo,
            approvals.as_deref(),
            &args,
            &mut TerminalUi,
        );
    }
    let setup = Setup::load(&args, &KeyringStore, &repo, approvals.as_deref())?;
    if let Some(warning) = &setup.warning {
        eprintln!("wiki-mcp: {warning}");
    }
    tracing::info!(
        profile = %setup.profile,
        access = ?setup.options.max_access,
        toolsets = ?setup.options.toolsets,
        dynamic_toolsets = setup.options.dynamic_toolsets,
        custom_tools = setup.options.custom_tools.len(),
        roots = %setup.files.describe_roots(),
        "запуск MCP-сервера Wiki"
    );
    let server = setup.server()?;
    let running = server
        .serve(stdio())
        .await
        .context("MCP initialization over stdio failed")?;
    running.waiting().await.context("MCP server task failed")?;
    Ok(())
}

/// Логи — только в stderr: stdout занят протоколом MCP.
pub(crate) fn init_logging(level: Option<&str>) -> anyhow::Result<()> {
    let filter = match level {
        Some(level) => EnvFilter::try_new(level)
            .map_err(|err| anyhow!("invalid --log-level `{level}`: {err}"))?,
        None => {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL))
        }
    };
    // Повторная инициализация (тесты) не ошибка: остаётся первый подписчик.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
    Ok(())
}

/// Всё, что нужно для запуска сервера: настройки ядра, политика файлов и клиент API.
pub struct Setup {
    pub profile: String,
    pub options: ServerOptions,
    pub files: FilePolicy,
    pub config: ClientConfig,
    /// Предупреждение для stderr: права профиля урезаны до одобренных (macOS).
    pub warning: Option<String>,
}

impl Setup {
    /// Читает профиль (токен — из `store`, одобренные права — из `approvals`) и собирает
    /// настройки сервера.
    pub fn load(
        args: &ServeArgs,
        store: &dyn TokenStore,
        repo: &ProfileRepo,
        approvals: Option<&dyn ApprovalStore>,
    ) -> anyhow::Result<Self> {
        args.validate()?;
        let profile_name = args.profile();
        let profile = load_profile_with(repo, store, profile_name)
            .with_context(|| format!("cannot load profile `{profile_name}`"))?;
        let resolved = resolve_access(args, &profile.settings, approvals)?;
        let mut options = effective_options(args, &profile.settings, &resolved.access)?;
        options.custom_tools = tools::load(Some(&tools::user_tools_file(repo)), &args.tools_file)?;
        options.jq = jq_worker::current_runner();
        let files =
            FilePolicy::with_defaults(&resolved.access.allow_path).map_err(|err| anyhow!(err))?;
        // Схема `Authorization` — по типу токена профиля; IAM-токены из источника
        // получаются при запросах и обновляются по сроку.
        let (credentials, iam) = match profile.auth {
            ProfileAuth::Stored(token) => match profile.settings.token_type {
                TokenType::OAuth => (Some(Credentials::oauth(token)), None),
                TokenType::Iam => (Some(Credentials::iam(token)), None),
            },
            ProfileAuth::Iam(source) => (None, Some(Arc::new(IamTokens::new(source)))),
        };
        let mut config = ClientConfig::new(credentials, Some(OrgId::from(&profile.org)))
            .map_err(|err| anyhow!("cannot configure the Wiki API client: {err}"))?;
        config.iam = iam;
        config.timeout = Duration::from_secs(args.timeout);
        config.max_response_bytes = args.max_response_bytes;
        Ok(Self {
            profile: profile_name.to_owned(),
            options,
            files,
            config,
            warning: resolved.verified.warning(EXE_STEM, profile_name),
        })
    }

    pub fn server(self) -> anyhow::Result<WikiServer> {
        let client = ApiClient::new(self.config)
            .map_err(|err| anyhow!("cannot create the Wiki API client: {err}"))?;
        WikiServer::new(client, self.options, self.files).map_err(|err| anyhow!(err))
    }
}

/// Всё, что нужно для HTTP-режима: без хранилища; из профиля — только уровень доступа.
pub struct HttpSetup {
    pub options: ServerOptions,
    /// Настройки общего клиента API — без токена и организации: на каждый вызов ядро делает
    /// копию клиента с данными запроса агента.
    pub config: ClientConfig,
    pub listen: SocketAddr,
    pub http: http_server::HttpOptions,
}

impl HttpSetup {
    /// Настройки HTTP-сервера — только из флагов запуска: уровень доступа задаёт оператор
    /// (`--allow-write`, `--allow-destructive`), профили не читаются.
    pub fn load(args: &ServeArgs) -> anyhow::Result<Self> {
        args.validate()?;
        let toolsets = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
        let options = ServerOptions {
            max_access: access_from_flags(args.allow_write, args.allow_destructive),
            toolsets: (!toolsets.is_empty()).then_some(toolsets),
            // Сервер не на машине агента: файловых инструментов нет, инструкции — для HTTP.
            local_machine: false,
            profile: None,
            // Только явные файлы: HTTP-режим собирается из флагов, без каталога конфигурации.
            // Это файлы оператора сервера, а не агента.
            custom_tools: tools::load(None, &args.tools_file)?,
            // Фильтры `jq` и здесь выполняются в отдельном процессе: это защита сервера от
            // зависших фильтров, а не доступ к файлам.
            jq: jq_worker::current_runner(),
            // HTTP без сессий: включённые наборы негде хранить между запросами (флаг
            // `--dynamic-toolsets` с `--http` отклоняет clap).
            ..ServerOptions::default()
        };
        let org = match (&args.org_id, &args.cloud_org_id) {
            (Some(id), _) => Some(server_org(OrgKind::Yandex360, id, "--org-id")?),
            (None, Some(id)) => Some(server_org(OrgKind::Cloud, id, "--cloud-org-id")?),
            (None, None) => None,
        };
        let listen = match args.listen {
            Some(listen) => listen,
            None => http_server::DEFAULT_LISTEN
                .parse()
                .context("invalid default listen address")?,
        };
        let http = http_server::HttpOptions {
            allowed_hosts: http_server::allowed_hosts(listen, &args.allowed_host),
            allowed_origins: args.allowed_origin.clone(),
            org,
            max_request_bytes: args
                .max_request_bytes
                .unwrap_or(http_server::DEFAULT_MAX_REQUEST_BYTES),
        };
        // Организацию в каждый вызов подставляет слой HTTP (из заголовка запроса или флага).
        let mut config = ClientConfig::new(None, None)
            .map_err(|err| anyhow!("cannot configure the Wiki API client: {err}"))?;
        config.timeout = Duration::from_secs(args.timeout);
        config.max_response_bytes = args.max_response_bytes;
        Ok(Self {
            options,
            config,
            listen,
            http,
        })
    }

    pub fn server(self) -> anyhow::Result<(WikiServer, http_server::HttpOptions)> {
        let client = ApiClient::new(self.config)
            .map_err(|err| anyhow!("cannot create the Wiki API client: {err}"))?;
        let server = WikiServer::with_token_source(
            client,
            self.options,
            FilePolicy::disabled(),
            http_server::token_source(),
        )
        .map_err(|err| anyhow!(err))?
        .with_upload_urls(http_server::upload_urls(&self.http)?);
        Ok((server, self.http))
    }
}

/// Организация из флага сервера; значение в текст ошибки не попадает.
fn server_org(kind: OrgKind, id: &str, flag: &str) -> anyhow::Result<OrgId> {
    let org = Organization::new(kind, id).map_err(|err| anyhow!("{flag}: {err}"))?;
    Ok(OrgId::from(&org))
}

/// Откуда взят уровень доступа или наборы.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Flags,
    Profile,
    Default,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Flags => "from command-line flags",
            Self::Profile => "from the profile",
            Self::Default => "default",
        }
    }
}

/// Права сервера: права профиля, урезанные до одобренных (macOS), затем суженные флагами.
/// Флаг сверх прав профиля — ошибка с подсказкой, где меняются права.
fn resolve_access(
    args: &ServeArgs,
    settings: &ProfileSettings,
    approvals: Option<&dyn ApprovalStore>,
) -> anyhow::Result<Resolved> {
    resolve_with_hint(
        args.profile(),
        approvals,
        &settings.access(),
        &args.access_request(),
    )
}

fn resolve_with_hint(
    profile: &str,
    approvals: Option<&dyn ApprovalStore>,
    configured: &ProfileAccess,
    request: &AccessRequest,
) -> anyhow::Result<Resolved> {
    approval::resolve_with(approvals, profile, configured, request).map_err(|err| {
        anyhow!(
            "{err}. The access of profile `{profile}` is changed with `wiki-mcp ui`; \
             registrations made by older versions with access flags are updated by \
             `wiki-mcp connect`"
        )
    })
}

/// Уровень доступа ядра по уровню из mcp-kit.
pub fn api_level(level: mcp_kit::AccessLevel) -> AccessLevel {
    match level {
        mcp_kit::AccessLevel::Read => AccessLevel::Read,
        mcp_kit::AccessLevel::Write => AccessLevel::Write,
        mcp_kit::AccessLevel::Destructive => AccessLevel::Destructive,
    }
}

/// Откуда взяты права: из флагов (сужены), из профиля или по умолчанию (только чтение).
fn access_source(args: &ServeArgs, settings: &ProfileSettings) -> Source {
    let request = args.access_request();
    if request.level.is_some() || !request.allow_path.is_empty() {
        Source::Flags
    } else if settings.access().is_read_only() {
        Source::Default
    } else {
        Source::Profile
    }
}

/// Наборы: `--toolsets`, если задан, иначе из профиля; пусто — наборы по умолчанию (`None`).
fn effective_toolsets(
    args: &ServeArgs,
    settings: &ProfileSettings,
) -> anyhow::Result<(Option<Vec<String>>, Source)> {
    let cli = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
    if !cli.is_empty() {
        return Ok((Some(cli), Source::Flags));
    }
    let saved =
        normalize_toolsets(&settings.toolsets).map_err(|err| anyhow!("profile settings: {err}"))?;
    Ok(if saved.is_empty() {
        (None, Source::Default)
    } else {
        (Some(saved), Source::Profile)
    })
}

fn effective_options(
    args: &ServeArgs,
    settings: &ProfileSettings,
    access: &ProfileAccess,
) -> anyhow::Result<ServerOptions> {
    let (toolsets, _) = effective_toolsets(args, settings)?;
    Ok(ServerOptions {
        max_access: api_level(access.level),
        toolsets,
        dynamic_toolsets: args.dynamic_toolsets,
        local_machine: true,
        profile: Some(args.profile().to_owned()),
        // Пользовательские инструменты и исполнитель `jq` заполняет `Setup::load`.
        ..ServerOptions::default()
    })
}

/// `serve --status`: действующие настройки сервера без запуска и без запросов к API.
/// Значение токена не выводится: печатается только, сохранён ли он.
pub fn status(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    approvals: Option<&dyn ApprovalStore>,
    args: &ServeArgs,
    ui: &mut dyn Ui,
) -> anyhow::Result<()> {
    args.validate()?;
    let profile = args.profile();
    let settings = repo.get(profile)?.unwrap_or_default();
    let source = settings.refreshing_source();
    let token_line = match source {
        None if store.contains(profile)? => Ok(format!(
            "stored in the system keychain (service `{KEYRING_SERVICE}`)"
        )),
        None => Err(format!(
            "not found; store it with `wiki-mcp auth login --profile {profile}`"
        )),
        Some(IamSourceKind::Yc) => match settings.yc_command() {
            Ok(command) => Ok(format!(
                "not stored; IAM tokens from `{}` when needed, kept in memory only",
                command.display()
            )),
            Err(err) => Err(format!("invalid yc settings: {err:#}")),
        },
        Some(_) => match store.get_service_account_key(profile) {
            Ok(key) => Ok(format!(
                "not stored; IAM tokens for service account key `{}` of `{}` (key in the \
                 system keychain, account `{}`) when needed, kept in memory only",
                key.id(),
                key.service_account_id(),
                service_account_key_account(profile)
            )),
            Err(err) => Err(err.to_string()),
        },
    };
    let has_token = token_line.is_ok();
    let resolved = resolve_access(args, &settings, approvals)?;
    let access = api_level(resolved.access.level);
    let access_source = access_source(args, &settings);
    let (toolsets, toolsets_source) = effective_toolsets(args, &settings)?;
    let tools_file = tools::user_tools_file(repo);
    let options = ServerOptions {
        max_access: access,
        toolsets,
        dynamic_toolsets: args.dynamic_toolsets,
        custom_tools: tools::load(Some(&tools_file), &args.tools_file)?,
        local_machine: true,
        profile: Some(profile.to_owned()),
        ..ServerOptions::default()
    };
    let registry = ToolRegistry::new(&options).map_err(|err| anyhow!(err))?;
    let files =
        FilePolicy::with_defaults(&resolved.access.allow_path).map_err(|err| anyhow!(err))?;

    ui.say(&format!("Profile:   {profile}"));
    ui.say(&format!(
        "Token:     {}",
        token_line.unwrap_or_else(|err| err)
    ));
    ui.say(&format!(
        "Type:      {} (Authorization: {})",
        settings.token_type.label(),
        settings.token_type.auth_scheme()
    ));
    ui.say(&format!(
        "Org:       {}",
        settings.org.as_ref().map_or_else(
            || format!("not set; set it with `wiki-mcp auth login --profile {profile}`"),
            ToString::to_string
        )
    ));
    ui.say(&format!(
        "Access:    {} ({}; {})",
        access_name(access),
        access_description(access),
        access_source.label()
    ));
    ui.say(&format!(
        "Approval:  {}",
        approval_text(&resolved.verified.state)
    ));
    if let Some(warning) = resolved.verified.warning(EXE_STEM, profile) {
        ui.say(&format!("Warning:   {warning}"));
    }
    let enabled = registry.enabled_toolsets();
    let toolsets_text = if enabled.len() == wiki_api::spec::toolsets().len() {
        ALL_TOOLSETS.to_owned()
    } else if enabled.is_empty() {
        "none".to_owned()
    } else {
        enabled.join(",")
    };
    ui.say(&format!(
        "Toolsets:  {toolsets_text} ({})",
        toolsets_source.label()
    ));
    let disabled: Vec<&str> = wiki_api::spec::toolsets()
        .iter()
        .map(|toolset| toolset.slug)
        .filter(|slug| !enabled.iter().any(|name| name == slug))
        .collect();
    if !disabled.is_empty() {
        // В динамическом режиме остальные наборы включает сам агент.
        let hint = if options.dynamic_toolsets {
            "the agent enables them with enable_toolset".to_owned()
        } else {
            format!(
                "enable with --toolsets; default: {}",
                default_toolsets().join(",")
            )
        };
        ui.say(&format!("Disabled:  {} ({hint})", disabled.join(",")));
    }
    ui.say(&format!(
        "Dynamic:   {}",
        if options.dynamic_toolsets {
            format!("on ({})", Source::Flags.label())
        } else {
            "off".to_owned()
        }
    ));
    ui.say(&format!("Tools:     {}", registry.len()));
    let custom_registered = options
        .custom_tools
        .iter()
        .filter(|tool| registry.get(&tool.name).is_some())
        .count();
    ui.say(&format!(
        "Custom:    {} defined, {custom_registered} registered (user file: {}{})",
        options.custom_tools.len(),
        tools_file.display(),
        if tools_file.exists() {
            ""
        } else {
            ", not found"
        }
    ));
    ui.say(&format!("Files:     {}", files.describe_roots()));
    ui.say(&format!("Profiles:  {}", repo.path().display()));
    if !has_token || settings.org.is_none() {
        ui.say("The server cannot start until the profile has a token and an organization.");
    }
    Ok(())
}

/// Состояние одобрения для `serve --status`.
pub fn approval_text(state: &ApprovalState) -> String {
    match state {
        ApprovalState::NotRequired => {
            "not used on this platform (the token's rights in Wiki limit the access)".to_owned()
        }
        ApprovalState::ReadOnly => "not needed (read-only)".to_owned(),
        ApprovalState::Approved => "approved on this Mac".to_owned(),
        ApprovalState::Missing => "not approved on this Mac".to_owned(),
        ApprovalState::Exceeds { approved } => {
            format!("only `{}` is approved on this Mac", approved.summary())
        }
        ApprovalState::Unavailable(reason) => format!("cannot read the keychain: {reason}"),
    }
}

fn access_description(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read tools only",
        AccessLevel::Write => "read and write tools, no delete",
        AccessLevel::Destructive => "read, write and delete tools",
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use rmcp::model::CallToolRequestParams;
    use serde_json::json;
    use wiki_api::ApiToken;
    use wiki_api::auth::MemoryStore;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::ui::testing::ScriptedUi;

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        serve: ServeArgs,
    }

    fn try_parse(args: &[&str]) -> Result<ServeArgs, clap::Error> {
        let mut argv = vec!["serve"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).map(|w| w.serve)
    }

    fn parse(args: &[&str]) -> ServeArgs {
        try_parse(args).unwrap()
    }

    #[test]
    fn http_flags() {
        let args = parse(&[
            "--http",
            "--listen",
            "0.0.0.0:9000",
            "--allowed-host",
            "mcp.example.com",
            "--allowed-origin",
            "http://localhost:6274",
            "--allow-destructive",
            "--toolsets",
            "pages,access",
            "--cloud-org-id",
            "bpf-1",
            "--max-request-bytes",
            "4096",
        ]);
        let setup = HttpSetup::load(&args).unwrap();
        assert_eq!(setup.options.max_access, AccessLevel::Destructive);
        assert_eq!(
            setup.options.toolsets,
            Some(vec!["pages".to_owned(), "access".to_owned()])
        );
        assert!(!setup.options.local_machine);
        assert_eq!(setup.options.profile, None);
        assert_eq!(setup.listen, "0.0.0.0:9000".parse().unwrap());
        assert_eq!(
            setup.http.allowed_hosts,
            ["localhost", "127.0.0.1", "::1", "mcp.example.com"]
        );
        assert_eq!(setup.http.allowed_origins, ["http://localhost:6274"]);
        assert_eq!(setup.http.org, Some(OrgId::Cloud("bpf-1".into())));
        assert_eq!(setup.http.max_request_bytes, 4096);
        // У общего клиента нет ни токена, ни организации: они приходят с каждым запросом.
        assert!(setup.config.credentials.is_none());
        assert!(setup.config.org.is_none());
        let (server, _) = setup.server().unwrap();
        assert!(server.instructions().contains("HTTP mode"));
        assert!(!server.registry().local_files());

        let defaults = HttpSetup::load(&parse(&["--http"])).unwrap();
        assert_eq!(defaults.listen, "127.0.0.1:8767".parse().unwrap());
        assert_eq!(defaults.options.max_access, AccessLevel::Read);
        assert_eq!(defaults.options.toolsets, None);
        assert_eq!(defaults.http.org, None);
        assert_eq!(defaults.http.allowed_origins, Vec::<String>::new());
        assert_eq!(defaults.http.max_request_bytes, 1024 * 1024);

        let org = HttpSetup::load(&parse(&["--http", "--org-id", " 12345 "])).unwrap();
        assert_eq!(org.http.org, Some(OrgId::Yandex360("12345".into())));
        let err = HttpSetup::load(&parse(&["--http", "--org-id", "a/b"]))
            .err()
            .unwrap();
        assert!(err.to_string().starts_with("--org-id: "), "{err}");
        assert!(!err.to_string().contains("a/b"), "{err}");
        let err = HttpSetup::load(&parse(&["--http", "--max-request-bytes", "0"]))
            .err()
            .unwrap();
        assert!(err.to_string().contains("--max-request-bytes"), "{err}");
        let err = HttpSetup::load(&parse(&["--http", "--toolsets", "repos"]))
            .err()
            .unwrap();
        assert!(err.to_string().contains("unknown toolset"), "{err}");
    }

    /// HTTP-режим профилей не читает: уровень задают только флаги оператора
    /// (`HttpSetup::load` не получает ни профилей, ни хранилища одобрений).
    #[test]
    fn http_access_comes_from_the_flags_only() {
        let setup = HttpSetup::load(&parse(&["--http"])).unwrap();
        assert_eq!(setup.options.max_access, AccessLevel::Read);
        let setup = HttpSetup::load(&parse(&["--http", "--allow-write"])).unwrap();
        assert_eq!(setup.options.max_access, AccessLevel::Write);
        // Подсказки агенту в HTTP-режиме называют флаги, а не профиль.
        let (server, _) = setup.server().unwrap();
        let instructions = server.instructions();
        assert!(
            instructions.contains("--allow-destructive"),
            "{instructions}"
        );
        assert!(!instructions.contains("wiki-mcp ui"), "{instructions}");
    }

    #[test]
    fn http_rejects_local_only_flags() {
        for extra in [
            &["--profile", "codex"][..],
            &["--allow-path", "/tmp"][..],
            &["--status"][..],
            &["--org-id", "1", "--cloud-org-id", "2"][..],
        ] {
            let mut argv = vec!["--http"];
            argv.extend_from_slice(extra);
            let err = try_parse(&argv).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::ArgumentConflict,
                "{err}"
            );
        }
        // Флаги HTTP без --http — ошибка, а не молчаливое игнорирование.
        for flag in [
            &["--listen", "127.0.0.1:1"][..],
            &["--allowed-origin", "http://x"][..],
            &["--allowed-host", "x"][..],
            &["--org-id", "1"][..],
            &["--cloud-org-id", "1"][..],
            &["--max-request-bytes", "1"][..],
        ] {
            let err = try_parse(flag).unwrap_err();
            assert_eq!(
                err.kind(),
                clap::error::ErrorKind::MissingRequiredArgument,
                "{err}"
            );
        }
        assert!(try_parse(&["--http", "--listen", "localhost:80"]).is_err());
    }

    fn settings(write: bool, destructive: bool, toolsets: &[&str]) -> ProfileSettings {
        ProfileSettings {
            allow_write: write,
            allow_destructive: destructive,
            toolsets: toolsets.iter().map(|s| s.to_string()).collect(),
            org: Some(Organization::new(OrgKind::Yandex360, "12345").unwrap()),
            ..ProfileSettings::default()
        }
    }

    fn options(args: &[&str], saved: &ProfileSettings) -> ServerOptions {
        try_options(args, saved).unwrap()
    }

    fn try_options(args: &[&str], saved: &ProfileSettings) -> anyhow::Result<ServerOptions> {
        let args = parse(args);
        let resolved = resolve_access(&args, saved, None)?;
        effective_options(&args, saved, &resolved.access)
    }

    /// Профиль во временном каталоге и токен в памяти: системный keychain не трогаем.
    fn fixture(
        saved: Option<ProfileSettings>,
        token: bool,
    ) -> (tempfile::TempDir, ProfileRepo, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        if let Some(saved) = saved {
            repo.upsert("ci", saved).unwrap();
        }
        if token {
            store.set("ci", &ApiToken::new("serve-test-token")).unwrap();
        }
        (dir, repo, store)
    }

    fn status_text(args: &[&str], repo: &ProfileRepo, store: &MemoryStore) -> String {
        let mut ui = ScriptedUi::default();
        status(store, repo, None, &parse(args), &mut ui).unwrap();
        ui.text()
    }

    #[test]
    fn defaults() {
        let args = parse(&[]);
        assert_eq!(args.profile(), "default");
        assert_eq!(args.timeout, 60);
        assert_eq!(args.max_response_bytes, 256 * 1024);
        assert!(!args.status);
        assert!(args.allow_path.is_empty());
        let args = parse(&["--allow-path", "/a", "--allow-path", "/b"]);
        assert_eq!(args.allow_path, [PathBuf::from("/a"), PathBuf::from("/b")]);
        let opts = options(&[], &ProfileSettings::default());
        assert_eq!(opts.max_access, AccessLevel::Read);
        assert_eq!(opts.toolsets, None);
        assert_eq!(opts.profile.as_deref(), Some("default"));
        assert!(opts.local_machine);
    }

    #[test]
    fn flags_narrow_access_and_win_for_toolsets() {
        let saved = settings(true, true, &["pages"]);
        let opts = options(&[], &saved);
        assert_eq!(opts.max_access, AccessLevel::Destructive);
        assert_eq!(opts.toolsets, Some(vec!["pages".to_owned()]));

        let opts = options(&["--allow-write", "--toolsets", "grids,access"], &saved);
        assert_eq!(opts.max_access, AccessLevel::Write);
        assert_eq!(
            opts.toolsets,
            Some(vec!["grids".to_owned(), "access".to_owned()])
        );

        // --allow-destructive без --allow-write — удаление и запись.
        let opts = options(&["--allow-destructive"], &saved);
        assert_eq!(opts.max_access, AccessLevel::Destructive);

        // Флаги не расширяют права профиля.
        let read_only = settings(false, false, &[]);
        let err = try_options(&["--allow-write"], &read_only).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("profile allows only `read`"), "{text}");
        assert!(
            text.contains("access of profile `default` is changed with `wiki-mcp ui`"),
            "{text}"
        );
        let write = settings(true, false, &[]);
        assert!(try_options(&["--allow-destructive"], &write).is_err());

        let all = options(&["--toolsets", "all"], &saved);
        assert_eq!(
            all.toolsets.unwrap().len(),
            wiki_api::spec::toolsets().len()
        );
    }

    #[test]
    fn allow_path_comes_from_profile_and_flags_narrow_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let inner = root.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let saved = ProfileSettings {
            allow_path: vec![root.clone()],
            ..settings(false, false, &[])
        };
        let args = parse(&[]);
        let resolved = resolve_access(&args, &saved, None).unwrap();
        assert_eq!(resolved.access.allow_path, vec![root.clone()]);
        let args = parse(&["--allow-path", inner.to_str().unwrap()]);
        let resolved = resolve_access(&args, &saved, None).unwrap();
        assert_eq!(resolved.access.allow_path, vec![inner]);
        let other = tempfile::tempdir().unwrap();
        let args = parse(&["--allow-path", other.path().to_str().unwrap()]);
        let err = resolve_access(&args, &saved, None).unwrap_err();
        assert!(
            err.to_string().contains("not inside a directory allowed"),
            "{err}"
        );
    }

    #[test]
    fn unapproved_access_runs_read_only_with_a_warning() {
        use mcp_kit::approval::testing::MemoryStore as MemoryApprovals;

        let (_dir, repo, store) = fixture(Some(settings(true, true, &[])), true);
        let approvals = MemoryApprovals::default();
        let setup = Setup::load(
            &parse(&["--profile", "ci"]),
            &store,
            &repo,
            Some(&approvals),
        )
        .unwrap();
        assert_eq!(setup.options.max_access, AccessLevel::Read);
        let warning = setup.warning.unwrap();
        assert!(warning.contains("not approved on this Mac"), "{warning}");
        assert!(warning.contains("wiki-mcp ui"), "{warning}");
        // Старая регистрация с флагами в пределах профиля запускается, но на чтение.
        let setup = Setup::load(
            &parse(&["--profile", "ci", "--allow-destructive"]),
            &store,
            &repo,
            Some(&approvals),
        )
        .unwrap();
        assert_eq!(setup.options.max_access, AccessLevel::Read);

        approvals
            .save(
                "ci",
                &ProfileAccess::new(mcp_kit::AccessLevel::Write, vec![]),
            )
            .unwrap();
        let setup = Setup::load(
            &parse(&["--profile", "ci"]),
            &store,
            &repo,
            Some(&approvals),
        )
        .unwrap();
        assert_eq!(setup.options.max_access, AccessLevel::Write);
        assert!(setup.warning.unwrap().contains("only `write` is approved"));

        let mut ui = ScriptedUi::default();
        status(
            &store,
            &repo,
            Some(&approvals),
            &parse(&["--profile", "ci"]),
            &mut ui,
        )
        .unwrap();
        let text = ui.text();
        assert!(text.contains("Access:    write"), "{text}");
        assert!(
            text.contains("Approval:  only `write` is approved on this Mac"),
            "{text}"
        );
        assert!(text.contains("Warning:   profile `ci`"), "{text}");
    }

    #[test]
    fn profile_access_combinations() {
        for (write, destructive, level) in [
            (false, false, AccessLevel::Read),
            (true, false, AccessLevel::Write),
            // Профиль, написанный вручную, без allow_write: удаление подразумевает запись.
            (false, true, AccessLevel::Destructive),
            (true, true, AccessLevel::Destructive),
        ] {
            let opts = options(&[], &settings(write, destructive, &[]));
            assert_eq!(
                opts.max_access, level,
                "write={write} destructive={destructive}"
            );
        }
    }

    #[test]
    fn invalid_toolsets_are_reported() {
        let err = try_options(&["--toolsets", "repos"], &ProfileSettings::default()).unwrap_err();
        assert!(err.to_string().contains("unknown toolset `repos`"), "{err}");
        let err = try_options(&[], &settings(false, false, &["repos"])).unwrap_err();
        assert!(err.to_string().contains("profile settings"), "{err}");
    }

    #[test]
    fn status_shows_destructive_without_write_flag() {
        let (_dir, repo, store) = fixture(Some(settings(false, true, &[])), true);
        let text = status_text(&["--profile", "ci", "--allow-destructive"], &repo, &store);
        assert!(text.contains("Profile:   ci"), "{text}");
        assert!(text.contains("Token:     stored"), "{text}");
        assert!(
            text.contains("Type:      OAuth token (Authorization: OAuth)"),
            "{text}"
        );
        assert!(
            text.contains("Org:       Yandex 360 `12345` (X-Org-Id)"),
            "{text}"
        );
        assert!(
            text.contains(
                "Access:    destructive (read, write and delete tools; from command-line flags)"
            ),
            "{text}"
        );
        assert!(
            text.contains("Toolsets:  pages,attachments,comments,grids (default)"),
            "{text}"
        );
        assert!(text.contains("Disabled:  access,extra"), "{text}");
        assert!(
            text.contains("Approval:  not used on this platform"),
            "{text}"
        );
        assert!(!text.contains("Warning:"), "{text}");
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        assert!(text.contains("Files:     "), "{text}");
        assert!(text.contains(&tmp.display().to_string()), "{text}");
        assert!(!text.contains("serve-test-token"), "{text}");
        assert!(!text.contains("cannot start"), "{text}");
        let destructive = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Destructive,
            ..ServerOptions::default()
        })
        .unwrap();
        assert!(
            text.contains(&format!("Tools:     {}", destructive.len())),
            "{text}"
        );
    }

    #[test]
    fn status_reads_profile_settings() {
        // Профиль только с allow_destructive (записан вручную) — тоже удаление и запись.
        let (dir, _, store) = fixture(None, false);
        std::fs::write(
            dir.path().join("profiles.toml"),
            "[profiles.ci]\nallow_destructive = true\ntoken_type = \"iam\"\ntoolsets = [\"grids\"]\n",
        )
        .unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains(
                "Access:    destructive (read, write and delete tools; from the profile)"
            ),
            "{text}"
        );
        assert!(
            text.contains("Type:      Yandex Cloud IAM token (Authorization: Bearer)"),
            "{text}"
        );
        assert!(
            text.contains("Toolsets:  grids (from the profile)"),
            "{text}"
        );
        assert!(text.contains("Token:     not found"), "{text}");
        assert!(text.contains("Org:       not set"), "{text}");
        assert!(text.contains("cannot start"), "{text}");

        let text = status_text(
            &["--profile", "ci", "--allow-write", "--toolsets", "all"],
            &repo,
            &store,
        );
        assert!(
            text.contains(
                "Access:    write (read and write tools, no delete; from command-line flags)"
            ),
            "{text}"
        );
        assert!(
            text.contains("Toolsets:  all (from command-line flags)"),
            "{text}"
        );
        assert!(!text.contains("Disabled:"), "{text}");
    }

    #[test]
    fn setup_requires_token_and_org() {
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), false);
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None)
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains("auth login --profile ci"),
            "{err:#}"
        );

        let (_dir, repo, store) = fixture(None, true);
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("--org-id"), "{err:#}");
        assert!(!format!("{err:#}").contains("serve-test-token"));

        assert!(Setup::load(&parse(&["--timeout", "0"]), &store, &repo, None).is_err());
        // Несуществующий каталог профиля — ошибка запуска, а не тихий пропуск.
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let missing = repo.path().with_file_name("missing-dir");
        repo.upsert(
            "ci",
            ProfileSettings {
                allow_path: vec![missing],
                ..settings(false, false, &[])
            },
        )
        .unwrap();
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("not accessible"), "{err:#}");
        assert!(Setup::load(&parse(&["--profile", "a b"]), &store, &repo, None).is_err());
    }

    #[test]
    fn token_type_selects_the_auth_scheme() {
        let iam = ProfileSettings {
            token_type: TokenType::Iam,
            ..settings(false, false, &[])
        };
        let (_dir, repo, store) = fixture(Some(iam), true);
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        let credentials = setup.config.credentials.as_ref().unwrap();
        assert_eq!(
            credentials,
            &Credentials::iam(ApiToken::new("serve-test-token"))
        );

        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        assert_eq!(
            setup.config.credentials.as_ref().unwrap(),
            &Credentials::oauth(ApiToken::new("serve-test-token"))
        );
        assert_eq!(setup.config.org, Some(OrgId::Yandex360("12345".into())));
    }

    const TOOLS_TOML: &str = r#"
[[tools]]
name = "team_pages"
description = "Pages of the team section"
operation = "list_page_descendants"
jq = ".results[] | {slug, title}"

[tools.args]
slug = "team/project"

[[tools]]
name = "comment_on_home"
description = "Comment on the home page"
operation = "create_comment"
args = { idx = 42 }
"#;

    #[test]
    fn dynamic_toolsets_from_flag() {
        assert!(!options(&[], &ProfileSettings::default()).dynamic_toolsets);
        let opts = options(
            &["--dynamic-toolsets", "--toolsets", "grids"],
            &ProfileSettings::default(),
        );
        assert!(opts.dynamic_toolsets);
        assert_eq!(opts.toolsets, Some(vec!["grids".to_owned()]));

        // HTTP без сессий: включённые наборы негде хранить.
        let err = try_parse(&["--http", "--dynamic-toolsets"]).unwrap_err();
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "{err}"
        );

        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("Dynamic:   off"), "{text}");
        let text = status_text(&["--profile", "ci", "--dynamic-toolsets"], &repo, &store);
        assert!(
            text.contains("Dynamic:   on (from command-line flags)"),
            "{text}"
        );
        assert!(text.contains("Toolsets:  none"), "{text}");
        assert!(
            text.contains("(the agent enables them with enable_toolset)"),
            "{text}"
        );
    }

    #[test]
    fn status_counts_custom_tools() {
        let (dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains("Custom:    0 defined, 0 registered (user file: "),
            "{text}"
        );
        assert!(text.contains("tools.toml, not found)"), "{text}");

        std::fs::write(dir.path().join("tools.toml"), TOOLS_TOML).unwrap();
        let text = status_text(&["--profile", "ci"], &repo, &store);
        // Инструмент записи определён, но без права записи в профиле не зарегистрирован.
        assert!(
            text.contains("Custom:    2 defined, 1 registered"),
            "{text}"
        );
        repo.upsert("ci", settings(true, false, &[])).unwrap();
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains("Custom:    2 defined, 2 registered"),
            "{text}"
        );
    }

    #[test]
    fn http_reads_only_explicit_tools_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("team.toml");
        std::fs::write(&file, TOOLS_TOML).unwrap();
        let setup = HttpSetup::load(&parse(&["--http"])).unwrap();
        assert!(setup.options.custom_tools.is_empty());
        assert!(!setup.options.dynamic_toolsets);
        let file_arg = file.to_str().unwrap();
        let setup = HttpSetup::load(&parse(&["--http", "--tools-file", file_arg])).unwrap();
        let names: Vec<&str> = setup
            .options
            .custom_tools
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, ["team_pages", "comment_on_home"]);
        let (server, _) = setup.server().unwrap();
        assert!(server.registry().get("team_pages").is_some());
        assert!(server.registry().get("comment_on_home").is_none());

        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "[[tools]]\nname = \"x\"\n").unwrap();
        let args = parse(&["--http", "--tools-file", bad.to_str().unwrap()]);
        let err = HttpSetup::load(&args).err().unwrap();
        assert!(err.to_string().contains("bad.toml"), "{err}");
    }

    /// Свой инструмент из `tools.toml` рядом с профилями появляется у агента и вызывает
    /// операцию со своими аргументами и фильтром.
    #[tokio::test]
    async fn serves_custom_tools_from_config_dir() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/pages/descendants"))
            .and(wiremock::matchers::query_param("slug", "team/project"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "results": [{"id": 1, "slug": "team/project/a", "title": "A"}]
            })))
            .expect(1)
            .mount(&api)
            .await;

        let (dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        std::fs::write(dir.path().join("tools.toml"), TOOLS_TOML).unwrap();
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        // В тестах текущий бинарник — тестовый, подкоманды `jq-worker` у него нет.
        setup.options.jq = wiki_mcp_core::JqRunner::in_process();
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(256 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|t| t.name == "team_pages"));
        assert!(!tools.iter().any(|t| t.name == "comment_on_home"));

        let result = client
            .call_tool(CallToolRequestParams::new("team_pages"))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let text = result.content[0].as_text().unwrap().text.clone();
        assert_eq!(text, r#"{"slug":"team/project/a","title":"A"}"#);

        client.cancel().await.unwrap();
        handle.await.unwrap();
    }

    /// Сервер из профиля целиком: initialize, tools/list и вызов через in-memory транспорт,
    /// API — wiremock.
    #[tokio::test]
    async fn serves_tools_from_profile() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/me"))
            .and(header("authorization", "OAuth serve-test-token"))
            .and(header("x-org-id", "12345"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "robot"})))
            .expect(1)
            .mount(&api)
            .await;

        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(256 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();

        let info = client.peer_info().unwrap();
        let instructions = info.instructions.as_deref().unwrap();
        assert!(
            instructions.contains("auth login --profile ci"),
            "{instructions}"
        );
        let tools = client.list_all_tools().await.unwrap();
        assert!(
            tools.iter().any(|t| t.name == "get_current_user"),
            "{:?}",
            tools.iter().map(|t| &t.name).collect::<Vec<_>>()
        );
        assert!(
            tools
                .iter()
                .all(|t| t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true))
        );

        let result = client
            .call_tool(CallToolRequestParams::new("get_current_user"))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false));

        client.cancel().await.unwrap();
        handle.await.unwrap();
    }

    /// Профиль с IAM-токенами из источника: токена в хранилище нет, `serve --status`
    /// описывает источник, у клиента API — источник токенов вместо токена.
    #[test]
    fn iam_source_profiles_need_no_stored_token() {
        use crate::login::key_testing::{KEY_ID, SA_ID, key_file};
        use crate::profiles::IamSourceKind;
        use wiki_api::service_account::ServiceAccountKey;

        let cloud = wiki_api::Organization::new(OrgKind::Cloud, "cloud-org").unwrap();
        let mut yc = ProfileSettings {
            yc_profile: Some("work".into()),
            org: Some(cloud.clone()),
            ..Default::default()
        };
        yc.use_iam_source(IamSourceKind::Yc);
        let (_dir, repo, store) = fixture(Some(yc), false);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains("IAM tokens from `yc iam create-token --profile work`"),
            "{text}"
        );
        assert!(!text.contains("cannot start"), "{text}");
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        assert!(setup.config.credentials.is_none());
        assert!(setup.config.iam.is_some());
        assert_eq!(setup.config.org, Some(OrgId::Cloud("cloud-org".into())));

        let mut sa = ProfileSettings {
            org: Some(cloud),
            ..Default::default()
        };
        sa.use_iam_source(IamSourceKind::ServiceAccountKey);
        let (_dir, repo, store) = fixture(Some(sa), false);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("--service-account-key <FILE>"), "{text}");
        assert!(text.contains("cannot start"), "{text}");
        assert!(Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).is_err());

        let key = ServiceAccountKey::from_json(key_file().0.as_bytes()).unwrap();
        store.set_service_account_key("ci", &key).unwrap();
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains(&format!("service account key `{KEY_ID}` of `{SA_ID}`")),
            "{text}"
        );
        assert!(!text.contains("cannot start"), "{text}");
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        assert!(setup.config.iam.is_some());
    }

    /// Сервер из профиля с `yc`: вызов инструмента получает IAM-токен из подменного `yc`
    /// и отправляет его как `Bearer`.
    #[cfg(unix)]
    #[tokio::test]
    async fn serves_with_iam_tokens_from_yc() {
        use crate::login::testing::FakeYc;
        use crate::profiles::IamSourceKind;

        let yc = FakeYc::new();
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/me"))
            .and(header("authorization", "Bearer iam-token-1"))
            .and(header("x-cloud-org-id", "cloud-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"login": "robot"})))
            .expect(2)
            .mount(&api)
            .await;
        let mut settings = ProfileSettings {
            yc_path: Some(yc.path()),
            org: Some(wiki_api::Organization::new(OrgKind::Cloud, "cloud-org").unwrap()),
            ..Default::default()
        };
        settings.use_iam_source(IamSourceKind::Yc);
        let (_dir, repo, store) = fixture(Some(settings), false);
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(256 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();
        for _ in 0..2 {
            let result = client
                .call_tool(CallToolRequestParams::new("get_current_user"))
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(false));
        }
        // Токен взят из памяти: yc запущен один раз.
        assert_eq!(yc.calls(), 1);
        client.cancel().await.unwrap();
        handle.await.unwrap();
    }
}
