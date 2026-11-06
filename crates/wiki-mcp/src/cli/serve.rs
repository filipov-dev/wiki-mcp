//! `wiki-mcp serve` — запуск MCP-сервера через stdio или HTTP (`--http`).
//!
//! stdio: stdout занят протоколом MCP, всё остальное (логи, ошибки запуска) — только в stderr.
//! Токен и организация берутся только из профиля: токен — из системного хранилища, тип
//! токена и организация — из `profiles.toml`. Переменные окружения токен не задают.
//!
//! HTTP ([`http_server`]): профилей и хранилища нет, токен (и организация, если её не задаёт
//! флаг сервера) приходит в заголовках каждого запроса агента и нигде не сохраняется.
//! Уровень доступа и наборы — только флагами командной строки, локальные файлы выключены
//! (`ServerOptions::local_machine = false`).
//!
//! `serve --status` печатает действующие настройки (профиль, тип токена, организация, уровень
//! доступа, наборы, разрешённые каталоги) и выходит, не запуская сервер и не обращаясь к API.
//! С `--http` он недоступен: у HTTP-сервера нет профиля, а все его настройки — флаги запуска.
//!
//! Локальные файлы (загрузка части файла, скачивание вложений в `save_to`) разрешены только
//! в рабочем каталоге, временном каталоге и каталогах `--allow-path` (`FilePolicy`).

mod http_server;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use clap::Args;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;
use wiki_api::auth::{KEYRING_SERVICE, KeyringStore, TokenStore};
use wiki_api::executor::{ApiClient, ClientConfig, Credentials, OrgId};
use wiki_api::{AccessLevel, OrgKind, Organization};
use wiki_mcp_core::{
    ALL_TOOLSETS, FilePolicy, ServerOptions, ToolRegistry, WikiServer, access_from_flags,
    access_name, default_toolsets, normalize_toolsets,
};

use crate::profiles::{
    DEFAULT_PROFILE, ProfileRepo, ProfileSettings, TokenType, load_profile_with,
    validate_profile_name,
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
Access level and toolsets: flags given on the command line win; without them the settings \
saved for the profile are used. Without any, the server is read-only and exposes the default \
toolsets: pages, attachments, comments, grids. The `access` (personal access rights to pages) \
and `extra` (operations missing from the Wiki API reference) toolsets are enabled only \
explicitly. --allow-destructive implies --allow-write; changing access rights needs \
--allow-destructive. Use --toolsets all for every toolset; the error for an unknown name lists \
the available ones.

Token and organization: read only from the profile (the token from the system keychain, its \
type and the organization from profiles.toml); store them with `wiki-mcp auth login`. An OAuth \
token is sent as `Authorization: OAuth <token>`, a Yandex Cloud IAM token as \
`Authorization: Bearer <token>`. The token is never read from environment variables or \
command-line arguments.

Local files: file arguments of tools (`file` of upload_attachment and upload_file_part, `save_to` for attachment \
downloads) must be inside the server's working directory (unless it is the filesystem root or \
the home directory), the system temp directory, or a directory given with --allow-path. The \
temp directory is allowed for scratch files and downloads; other programs share it, so keep \
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
rejected). Access level and toolsets come from the command line only. Local files are \
disabled: there are no upload and download tools. The Host header must be a loopback name, the \
--listen address or an --allowed-host; browser requests are rejected unless their Origin is \
allowed with --allowed-origin. Outside loopback, put the server behind a TLS reverse proxy.")]
pub struct ServeArgs {
    /// Profile with the token and organization [default: default]. Not available with --http.
    #[arg(long, conflicts_with = "http")]
    pub profile: Option<String>,
    /// Register tools that create or modify data.
    #[arg(long)]
    pub allow_write: bool,
    /// Register tools that delete data or change access rights (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable, or `all`.
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Extra directory that tools may read files from or save files to. Repeatable.
    /// Not available with --http.
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
    fn profile(&self) -> &str {
        self.profile.as_deref().unwrap_or(DEFAULT_PROFILE)
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
            "настройки MCP-сервера Wiki (HTTP)"
        );
        let listen = setup.listen;
        let (server, http) = setup.server()?;
        return http_server::run(server, listen, http).await;
    }
    let repo = ProfileRepo::open_default()?;
    if args.status {
        return status(&KeyringStore, &repo, &args, &mut TerminalUi);
    }
    let setup = Setup::load(&args, &KeyringStore, &repo)?;
    tracing::info!(
        profile = %setup.profile,
        access = ?setup.options.max_access,
        toolsets = ?setup.options.toolsets,
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
fn init_logging(level: Option<&str>) -> anyhow::Result<()> {
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
}

impl Setup {
    /// Читает профиль (токен — из `store`) и собирает настройки сервера.
    pub fn load(
        args: &ServeArgs,
        store: &dyn TokenStore,
        repo: &ProfileRepo,
    ) -> anyhow::Result<Self> {
        args.validate()?;
        let profile_name = args.profile();
        let profile = load_profile_with(repo, store, profile_name)
            .with_context(|| format!("cannot load profile `{profile_name}`"))?;
        let options = effective_options(args, &profile.settings)?;
        let files = FilePolicy::with_defaults(&args.allow_path).map_err(|err| anyhow!(err))?;
        // Схема `Authorization` — по типу токена профиля.
        let credentials = match profile.settings.token_type {
            TokenType::OAuth => Credentials::oauth(profile.token),
            TokenType::Iam => Credentials::iam(profile.token),
        };
        let mut config = ClientConfig::new(Some(credentials), Some(OrgId::from(&profile.org)))
            .map_err(|err| anyhow!("cannot configure the Wiki API client: {err}"))?;
        config.timeout = Duration::from_secs(args.timeout);
        config.max_response_bytes = args.max_response_bytes;
        Ok(Self {
            profile: profile_name.to_owned(),
            options,
            files,
            config,
        })
    }

    pub fn server(self) -> anyhow::Result<WikiServer> {
        let client = ApiClient::new(self.config)
            .map_err(|err| anyhow!("cannot create the Wiki API client: {err}"))?;
        WikiServer::new(client, self.options, self.files).map_err(|err| anyhow!(err))
    }
}

/// Всё, что нужно для HTTP-режима: без профиля и хранилища, только флаги.
pub struct HttpSetup {
    pub options: ServerOptions,
    /// Настройки общего клиента API — без токена и организации: на каждый вызов ядро делает
    /// копию клиента с данными запроса агента.
    pub config: ClientConfig,
    pub listen: SocketAddr,
    pub http: http_server::HttpOptions,
}

impl HttpSetup {
    pub fn load(args: &ServeArgs) -> anyhow::Result<Self> {
        args.validate()?;
        let toolsets = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
        let options = ServerOptions {
            max_access: access_from_flags(args.allow_write, args.allow_destructive),
            toolsets: (!toolsets.is_empty()).then_some(toolsets),
            // Сервер не на машине агента: файловых инструментов нет, инструкции — для HTTP.
            local_machine: false,
            profile: None,
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
        .map_err(|err| anyhow!(err))?;
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

/// Уровень доступа: флаги командной строки, если заданы, иначе настройки профиля.
/// В обоих случаях `allow_destructive` подразумевает запись.
fn effective_access(args: &ServeArgs, settings: &ProfileSettings) -> (AccessLevel, Source) {
    if args.allow_write || args.allow_destructive {
        (
            access_from_flags(args.allow_write, args.allow_destructive),
            Source::Flags,
        )
    } else {
        let level = access_from_flags(settings.allow_write, settings.allow_destructive);
        let source = if level == AccessLevel::Read {
            Source::Default
        } else {
            Source::Profile
        };
        (level, source)
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
) -> anyhow::Result<ServerOptions> {
    let (max_access, _) = effective_access(args, settings);
    let (toolsets, _) = effective_toolsets(args, settings)?;
    Ok(ServerOptions {
        max_access,
        toolsets,
        local_machine: true,
        profile: Some(args.profile().to_owned()),
    })
}

/// `serve --status`: действующие настройки сервера без запуска и без запросов к API.
/// Значение токена не выводится: печатается только, сохранён ли он.
pub fn status(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    args: &ServeArgs,
    ui: &mut dyn Ui,
) -> anyhow::Result<()> {
    args.validate()?;
    let profile = args.profile();
    let settings = repo.get(profile)?.unwrap_or_default();
    let has_token = store.contains(profile)?;
    let (access, access_source) = effective_access(args, &settings);
    let (toolsets, toolsets_source) = effective_toolsets(args, &settings)?;
    let options = ServerOptions {
        max_access: access,
        toolsets,
        local_machine: true,
        profile: Some(profile.to_owned()),
    };
    let registry = ToolRegistry::new(&options).map_err(|err| anyhow!(err))?;
    let files = FilePolicy::with_defaults(&args.allow_path).map_err(|err| anyhow!(err))?;

    ui.say(&format!("Profile:   {profile}"));
    ui.say(&format!(
        "Token:     {}",
        if has_token {
            format!("stored in the system keychain (service `{KEYRING_SERVICE}`)")
        } else {
            format!("not found; store it with `wiki-mcp auth login --profile {profile}`")
        }
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
    let enabled = registry.enabled_toolsets();
    let toolsets_text = if enabled.len() == wiki_api::spec::toolsets().len() {
        ALL_TOOLSETS.to_owned()
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
        ui.say(&format!(
            "Disabled:  {} (enable with --toolsets; default: {})",
            disabled.join(","),
            default_toolsets().join(",")
        ));
    }
    ui.say(&format!("Tools:     {}", registry.len()));
    ui.say(&format!("Files:     {}", files.describe_roots()));
    ui.say(&format!("Profiles:  {}", repo.path().display()));
    if !has_token || settings.org.is_none() {
        ui.say("The server cannot start until the profile has a token and an organization.");
    }
    Ok(())
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
        effective_options(&parse(args), saved).unwrap()
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
        status(store, repo, &parse(args), &mut ui).unwrap();
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
    fn cli_flags_win_over_profile() {
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
        let opts = options(&["--allow-destructive"], &settings(false, false, &[]));
        assert_eq!(opts.max_access, AccessLevel::Destructive);

        let all = options(&["--toolsets", "all"], &saved);
        assert_eq!(
            all.toolsets.unwrap().len(),
            wiki_api::spec::toolsets().len()
        );
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
        let err = effective_options(
            &parse(&["--toolsets", "repos"]),
            &ProfileSettings::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("unknown toolset `repos`"), "{err}");
        let err = effective_options(&parse(&[]), &settings(false, false, &["repos"])).unwrap_err();
        assert!(err.to_string().contains("profile settings"), "{err}");
    }

    #[test]
    fn status_shows_destructive_without_write_flag() {
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
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
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo)
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains("auth login --profile ci"),
            "{err:#}"
        );

        let (_dir, repo, store) = fixture(None, true);
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo)
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("--org-id"), "{err:#}");
        assert!(!format!("{err:#}").contains("serve-test-token"));

        assert!(Setup::load(&parse(&["--timeout", "0"]), &store, &repo).is_err());
        // Несуществующий каталог --allow-path — ошибка запуска, а не тихий пропуск.
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let missing = repo.path().with_file_name("missing-dir");
        let missing = missing.display().to_string();
        let err = Setup::load(
            &parse(&["--profile", "ci", "--allow-path", &missing]),
            &store,
            &repo,
        )
        .err()
        .unwrap();
        assert!(format!("{err:#}").contains("not accessible"), "{err:#}");
        assert!(Setup::load(&parse(&["--profile", "a b"]), &store, &repo).is_err());
    }

    #[test]
    fn token_type_selects_the_auth_scheme() {
        let iam = ProfileSettings {
            token_type: TokenType::Iam,
            ..settings(false, false, &[])
        };
        let (_dir, repo, store) = fixture(Some(iam), true);
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        let credentials = setup.config.credentials.as_ref().unwrap();
        assert_eq!(
            credentials,
            &Credentials::iam(ApiToken::new("serve-test-token"))
        );

        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        assert_eq!(
            setup.config.credentials.as_ref().unwrap(),
            &Credentials::oauth(ApiToken::new("serve-test-token"))
        );
        assert_eq!(setup.config.org, Some(OrgId::Yandex360("12345".into())));
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
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
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
}
