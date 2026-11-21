//! `wiki-mcp ui` — TUI профилей ([`mcp_kit::tui`]): профили и их настройки, права и
//! одобрение на macOS, регистрации в Claude Code и Codex, проверка токена в Wiki и
//! инструменты с вызовом.
//!
//! Здесь только сервисная часть ([`WikiAdapter`]): чтение и запись `profiles.toml`,
//! состояние токена (без самого токена), организация и источник IAM-токенов, пользователь
//! токена по `GET /v1/users/me`, сборка сервера и повторная регистрация без флагов доступа.
//! Сервер для инструментов собирается так же, как `serve` через stdio без флагов: права
//! профиля, урезанные до одобренных.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use clap::Args;
use mcp_kit::ProfileAccess;
use mcp_kit::approval::{self, ApprovalStore};
use mcp_kit::call::Session;
use mcp_kit::registrations::{self, Registration, ServerIdentity};
use mcp_kit::tui::{self, Field, ProfileView, ServiceAdapter};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use serde_json::{Value, json};
use wiki_api::auth::{KEYRING_SERVICE, KeyringStore, TokenStore, service_account_key_account};

use crate::cli::serve::{ServeArgs, Setup};
use crate::clients::claude::{self, SystemRunner};
use crate::clients::{EXE_STEM, Scope, ServerSpec, codex};
use crate::profiles::{DEFAULT_PROFILE, IamSourceKind, ProfileRepo, ProfileSettings};
use crate::ui::CaptureUi;
use crate::verify::{ME_PATH, current_user};

const UI_HELP: &str = "\
Shows the profiles in profiles.toml with their settings and the state of the token (never the \
token), the access of each profile and whether it is approved, where the server is registered \
in Claude Code and Codex and whether those registrations agree with the profile, the \
organization, the source of IAM tokens and the user of the token in Wiki, and the tools the \
profile gives the agent.

The access (read, write or destructive, and the extra directories for local files) is changed \
here: r, w and d set the level, a adds a directory, x removes one, s saves. On macOS, saving \
more access than was approved asks for Touch ID or the macOS password; access that is not \
approved does not take effect. f registers the selected entry again without access flags. \
Press ? for the keys.";

/// `serve` не читает профиль из переменной окружения: пустое имя не совпадёт ни с одной
/// переменной записи клиента, и профиль регистрации — `--profile`, иначе `default`.
const NO_PROFILE_ENV: &str = "";

#[derive(Debug, Clone, Args)]
#[command(after_help = UI_HELP)]
pub struct UiArgs {}

pub async fn run(_args: UiArgs) -> anyhow::Result<()> {
    let approvals = approval::platform_store(EXE_STEM);
    let adapter = WikiAdapter {
        repo: ProfileRepo::open_default()?,
        tokens: &KeyringStore,
        approvals: approvals.as_deref(),
        exe: std::env::current_exe()
            .map_err(|err| anyhow!("cannot determine the path of wiki-mcp: {err}"))?,
        codex_home: codex::default_codex_home().ok(),
    };
    tui::run(&adapter).await.map_err(|err| anyhow!(err))
}

/// Сервисная часть TUI для Yandex Wiki.
pub struct WikiAdapter<'a> {
    pub repo: ProfileRepo,
    pub tokens: &'a dyn TokenStore,
    /// Одобренные права (macOS) для сервера инструментов; `None` — платформа без одобрения.
    pub approvals: Option<&'a dyn ApprovalStore>,
    /// Путь к бинарнику для повторной регистрации.
    pub exe: PathBuf,
    /// `$CODEX_HOME`; `None` — не определить (тогда Codex не регистрируется заново).
    pub codex_home: Option<PathBuf>,
}

impl WikiAdapter<'_> {
    /// Сервер с правами профиля без флагов (как `serve` без флагов), со всеми наборами.
    fn setup(&self, profile: &str) -> anyhow::Result<Setup> {
        let args = ServeArgs {
            toolsets: vec![wiki_mcp_core::ALL_TOOLSETS.to_owned()],
            ..ServeArgs::for_profile(Some(profile.to_owned()))
        };
        let mut setup = Setup::load(&args, self.tokens, &self.repo, self.approvals)?;
        setup.options.dynamic_toolsets = false;
        Ok(setup)
    }

    async fn session(&self, profile: &str) -> Result<Session, String> {
        let setup = self.setup(profile).map_err(|err| format!("{err:#}"))?;
        let server = setup.server().map_err(|err| format!("{err:#}"))?;
        Session::start(server).await.map_err(|err| err.to_string())
    }

    /// Откуда профиль берёт токен; значение токена и ключа не показывается.
    fn token_source(&self, name: &str, settings: &ProfileSettings) -> String {
        match settings.refreshing_source() {
            None => match self.tokens.contains(name) {
                Ok(true) => format!(
                    "{} stored in the system keychain (service `{KEYRING_SERVICE}`)",
                    settings.token_type.label()
                ),
                Ok(false) => {
                    format!("missing: run `wiki-mcp auth login --profile {name}`")
                }
                Err(err) => format!("cannot check the keychain: {err}"),
            },
            Some(IamSourceKind::Yc) => match settings.yc_command() {
                Ok(command) => format!(
                    "IAM tokens from `{}` when needed, kept in memory only",
                    command.display()
                ),
                Err(err) => format!("invalid yc settings: {err:#}"),
            },
            Some(_) => match self.tokens.get_service_account_key(name) {
                Ok(key) => format!(
                    "IAM tokens for service account key `{}` of `{}` (key in the system \
                     keychain, account `{}`), kept in memory only",
                    key.id(),
                    key.service_account_id(),
                    service_account_key_account(name)
                ),
                Err(err) => format!("service account key: {err}"),
            },
        }
    }

    fn profile_fields(&self, name: &str, settings: &ProfileSettings) -> Vec<Field> {
        let toolsets = if settings.toolsets.is_empty() {
            "default".to_owned()
        } else {
            settings.toolsets.join(",")
        };
        vec![
            Field::new("Token", self.token_source(name, settings)),
            Field::new(
                "Type",
                format!(
                    "{} (Authorization: {})",
                    settings.token_type.label(),
                    settings.token_type.auth_scheme()
                ),
            ),
            Field::new("Organization", organization(settings)),
            Field::new(
                "OAuth app",
                settings
                    .client_id
                    .as_deref()
                    .map_or_else(|| "-".to_owned(), |id| format!("ClientID {id}")),
            ),
            Field::new("Toolsets", toolsets),
            Field::new(
                "Connected to",
                settings.client.clone().unwrap_or_else(|| "-".into()),
            ),
        ]
    }
}

impl ServiceAdapter for WikiAdapter<'_> {
    fn bin(&self) -> &str {
        EXE_STEM
    }

    fn service_name(&self) -> &str {
        "Yandex Wiki"
    }

    fn identity(&self) -> ServerIdentity<'_> {
        ServerIdentity {
            exe_stem: EXE_STEM,
            default_profile: DEFAULT_PROFILE,
            profile_env: NO_PROFILE_ENV,
        }
    }

    fn load_profiles(&self) -> Result<Vec<ProfileView>, String> {
        let profiles = self.repo.list().map_err(|err| format!("{err:#}"))?;
        Ok(profiles
            .iter()
            .map(|(name, settings)| ProfileView {
                name: name.clone(),
                access: settings.access(),
                fields: self.profile_fields(name, settings),
            })
            .collect())
    }

    fn save_access(&self, profile: &str, access: &ProfileAccess) -> Result<(), String> {
        let mut settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        settings.set_access(access);
        self.repo
            .upsert(profile, settings)
            .map_err(|err| format!("{err:#}"))
    }

    async fn check_account(&self, profile: &str) -> Result<Vec<Field>, String> {
        let settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        let mut fields = vec![
            Field::new("Organization", organization(&settings)),
            Field::new("Token", self.token_source(profile, &settings)),
        ];
        let user = match self.session(profile).await {
            Ok(session) => {
                let arguments = object(json!({ "method": "GET", "path": ME_PATH }));
                let user = match session.call("api_request", arguments).await {
                    Ok(result) => user_fields(&result),
                    Err(err) => vec![Field::new("User", format!("failed: {err}"))],
                };
                session.close().await;
                user
            }
            Err(err) => vec![Field::new("User", format!("not checked: {err}"))],
        };
        fields.extend(user);
        fields.push(Field::new(
            "Token rights",
            "Wiki does not report them: the permissions of the OAuth application (wiki:read, \
             wiki:write) or of the Yandex Cloud account and the user's rights to pages limit \
             what the token can do",
        ));
        Ok(fields)
    }

    async fn tools(&self, profile: &str) -> Result<Vec<Tool>, String> {
        let session = self.session(profile).await?;
        let tools = session.tools().await.map_err(|err| err.to_string());
        session.close().await;
        tools
    }

    async fn call_tool(
        &self,
        profile: &str,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, String> {
        let session = self.session(profile).await?;
        let result = session
            .call(tool, arguments)
            .await
            .map_err(|err| err.to_string());
        session.close().await;
        result
    }

    fn reregister(&self, registration: &Registration) -> Result<String, String> {
        reregister(
            registration,
            &self.exe,
            self.codex_home.as_deref(),
            std::env::current_dir().ok().as_deref(),
        )
    }
}

/// Организация профиля для показа.
fn organization(settings: &ProfileSettings) -> String {
    settings.org.as_ref().map_or_else(
        || "not set: run `wiki-mcp auth login` with --org-id or --cloud-org-id".to_owned(),
        ToString::to_string,
    )
}

/// Пользователь токена и его организация по ответу `GET /v1/users/me`; ошибка — первой
/// строкой.
fn user_fields(result: &CallToolResult) -> Vec<Field> {
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if result.is_error == Some(true) {
        let first = text.lines().next().unwrap_or("error");
        let first: String = first.chars().take(160).collect();
        return vec![Field::new("User", format!("failed: {first}"))];
    }
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    match current_user(&body) {
        Some(user) => {
            let mut fields = vec![Field::new("User", user.login)];
            if let Some(org) = user.org {
                fields.push(Field::new("Wiki org", org));
            }
            fields
        }
        None => vec![Field::new(
            "User",
            "the token works, but Wiki returned no login",
        )],
    }
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

/// Аргументы `serve` без флагов доступа: права — настройка профиля. Остальные флаги
/// регистрации (`--toolsets`, `--dynamic-toolsets`, `--tools-file`, таймауты) остаются.
fn without_access_flags(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--allow-write" | "--allow-destructive" => {}
            "--allow-path" => {
                iter.next();
            }
            other
                if other.starts_with("--allow-path=")
                    || other.starts_with("--allow-write=")
                    || other.starts_with("--allow-destructive=") => {}
            _ => out.push(arg.clone()),
        }
    }
    out
}

/// Регистрирует запись заново: те же аргументы `serve`, но без флагов доступа. Запись
/// проекта (Claude Code) — только из каталога проекта.
fn reregister(
    registration: &Registration,
    exe: &Path,
    codex_home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<String, String> {
    let serve = &registration.serve;
    if serve.subcommand.as_deref() != Some("serve") {
        return Err(format!(
            "`{}` does not run `wiki-mcp serve`; register it with `wiki-mcp connect`",
            registration.name
        ));
    }
    let mut spec = ServerSpec::new(
        &registration.name,
        exe,
        &serve.profile,
        &ProfileSettings::default(),
    )
    .map_err(|err| format!("{err:#}"))?;
    spec.args = without_access_flags(&registration.args);
    let mut ui = CaptureUi::default();
    let done = match registration.client {
        registrations::Client::ClaudeCode => {
            let scope = match &registration.scope {
                registrations::Scope::User => Scope::User,
                registrations::Scope::Project(dir) if Some(dir.as_path()) == cwd => Scope::Project,
                other => {
                    return Err(format!(
                        "`{}` is registered for {other}; run `wiki-mcp connect claude-code \
                         --scope project --name {} --profile {}` in that project",
                        registration.name, registration.name, serve.profile
                    ));
                }
            };
            claude::register(&mut SystemRunner, &mut ui, scope, &spec, true)
        }
        registrations::Client::Codex => {
            let home = codex_home.ok_or("cannot determine $CODEX_HOME")?;
            codex::register(&mut ui, home, &spec, true)
        }
    }
    .map_err(|err| format!("{err:#}"))?;
    if !done {
        return Err(ui
            .lines
            .last()
            .cloned()
            .unwrap_or_else(|| "not changed".into()));
    }
    Ok(format!(
        "{} now runs `{}` as `{}`; restart the client to pick it up",
        registration.client.display_name(),
        spec.display(),
        spec.name
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiki_api::auth::MemoryStore;
    use wiki_api::{ApiToken, OrgKind, Organization};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::clients::TEST_EXE;

    fn adapter<'a>(dir: &Path, tokens: &'a MemoryStore) -> WikiAdapter<'a> {
        WikiAdapter {
            repo: ProfileRepo::in_dir(dir.join("config")),
            tokens,
            approvals: None,
            exe: PathBuf::from(TEST_EXE),
            codex_home: Some(dir.join("codex")),
        }
    }

    fn work_settings() -> ProfileSettings {
        ProfileSettings {
            client_id: Some("0123456789abcdef0123456789abcdef".into()),
            allow_write: true,
            org: Some(Organization::new(OrgKind::Yandex360, "12345").unwrap()),
            ..ProfileSettings::default()
        }
    }

    #[test]
    fn shows_profiles_without_the_token_and_saves_access() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = MemoryStore::new();
        tokens.set("work", &ApiToken::new("ui-test-token")).unwrap();
        let adapter = adapter(dir.path(), &tokens);
        adapter.repo.upsert("work", work_settings()).unwrap();
        adapter
            .repo
            .upsert("empty", ProfileSettings::default())
            .unwrap();
        let mut yc = ProfileSettings {
            yc_profile: Some("fed".into()),
            org: Some(Organization::new(OrgKind::Cloud, "cloud-org").unwrap()),
            ..ProfileSettings::default()
        };
        yc.use_iam_source(IamSourceKind::Yc);
        adapter.repo.upsert("cloud", yc).unwrap();

        let profiles = adapter.load_profiles().unwrap();
        assert_eq!(profiles.len(), 3);
        let work = profiles.iter().find(|p| p.name == "work").unwrap();
        assert_eq!(work.access.level, mcp_kit::AccessLevel::Write);
        let text = format!("{:?}", work.fields);
        assert!(
            text.contains("OAuth token stored in the system keychain"),
            "{text}"
        );
        assert!(text.contains("Yandex 360 `12345` (X-Org-Id)"), "{text}");
        assert!(text.contains("ClientID 0123456789abcdef"), "{text}");
        assert!(!text.contains("ui-test-token"), "{text}");
        let empty = profiles.iter().find(|p| p.name == "empty").unwrap();
        let text = format!("{:?}", empty.fields);
        assert!(text.contains("auth login --profile empty"), "{text}");
        assert!(text.contains("Organization"), "{text}");
        let cloud = profiles.iter().find(|p| p.name == "cloud").unwrap();
        let text = format!("{:?}", cloud.fields);
        assert!(
            text.contains("IAM tokens from `yc iam create-token --profile fed`"),
            "{text}"
        );

        let root = dir.path().join("files");
        adapter
            .save_access(
                "work",
                &ProfileAccess::new(mcp_kit::AccessLevel::Destructive, vec![root.clone()]),
            )
            .unwrap();
        let saved = adapter.repo.get("work").unwrap().unwrap();
        assert!(saved.allow_destructive);
        assert_eq!(saved.allow_path, vec![root]);
        // Остальные настройки профиля не тронуты.
        assert_eq!(saved.client_id, work_settings().client_id);
        assert_eq!(saved.org, work_settings().org);
    }

    #[tokio::test]
    async fn check_account_shows_the_user_of_the_token() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ME_PATH))
            .and(header("authorization", "OAuth ui-test-token"))
            .and(header("x-org-id", "12345"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "username": "robot",
                "org": {"dir_id": "12345"}
            })))
            .expect(1)
            .mount(&api)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let tokens = MemoryStore::new();
        tokens.set("work", &ApiToken::new("ui-test-token")).unwrap();
        let adapter = adapter(dir.path(), &tokens);
        adapter.repo.upsert("work", work_settings()).unwrap();

        // Сервер адаптера ходит в настоящий Wiki API, поэтому в тесте вызов идёт через
        // такую же сессию с адресом wiremock.
        let mut setup = adapter.setup("work").unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        let session = Session::start(setup.server().unwrap()).await.unwrap();
        let result = session
            .call(
                "api_request",
                object(json!({ "method": "GET", "path": ME_PATH })),
            )
            .await
            .unwrap();
        session.close().await;
        let fields = user_fields(&result);
        assert_eq!(
            fields,
            [
                Field::new("User", "robot"),
                Field::new("Wiki org", "dir_id 12345")
            ]
        );

        // Без токена — поля профиля и причина, а не ошибка всей проверки.
        let fields = adapter.check_account("missing").await.unwrap();
        let text = format!("{fields:?}");
        assert!(text.contains("not checked"), "{text}");
        assert!(text.contains("auth login --profile missing"), "{text}");
    }

    #[test]
    fn user_errors_are_short() {
        let err = CallToolResult::error(vec![ContentBlock::text(
            "Wiki returned 401 Unauthorized\nmore",
        )]);
        assert_eq!(
            user_fields(&err),
            [Field::new("User", "failed: Wiki returned 401 Unauthorized")]
        );
    }

    fn registration(
        client: registrations::Client,
        scope: registrations::Scope,
        args: &[&str],
    ) -> Registration {
        let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
        Registration {
            client,
            scope,
            name: "wiki".into(),
            command: PathBuf::from(TEST_EXE),
            serve: registrations::parse_serve(&args, None, DEFAULT_PROFILE),
            args,
            env_names: vec![],
            enabled: true,
        }
    }

    #[test]
    fn access_flags_are_dropped_and_other_flags_kept() {
        let args: Vec<String> = [
            "serve",
            "--profile",
            "work",
            "--allow-write",
            "--allow-path",
            "/srv/a",
            "--allow-path=/srv/b",
            "--allow-destructive",
            "--toolsets",
            "pages,grids",
            "--dynamic-toolsets",
            "--timeout",
            "90",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        assert_eq!(
            without_access_flags(&args),
            [
                "serve",
                "--profile",
                "work",
                "--toolsets",
                "pages,grids",
                "--dynamic-toolsets",
                "--timeout",
                "90"
            ]
        );
    }

    #[test]
    fn reregisters_codex_without_access_flags() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            codex::config_path(&home),
            format!(
                "[mcp_servers.wiki]\ncommand = {TEST_EXE:?}\nargs = [\"serve\", \"--profile\", \"work\", \"--allow-write\", \"--allow-destructive\", \"--toolsets\", \"pages\"]\n"
            ),
        )
        .unwrap();
        let reg = registration(
            registrations::Client::Codex,
            registrations::Scope::User,
            &[
                "serve",
                "--profile",
                "work",
                "--allow-write",
                "--allow-destructive",
                "--toolsets",
                "pages",
            ],
        );
        assert!(reg.serve.has_access_flags());
        let message = reregister(&reg, Path::new(TEST_EXE), Some(&home), None).unwrap();
        assert!(message.contains("Codex now runs"), "{message}");
        let config = std::fs::read_to_string(codex::config_path(&home)).unwrap();
        assert!(!config.contains("--allow"), "{config}");
        assert!(
            config.contains(r#"args = ["serve", "--profile", "work", "--toolsets", "pages"]"#),
            "{config}"
        );
    }

    #[test]
    fn project_registration_elsewhere_and_other_commands_are_explained() {
        let (here, elsewhere) = if cfg!(windows) {
            (r"C:\here", r"C:\elsewhere")
        } else {
            ("/here", "/elsewhere")
        };
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Project(PathBuf::from(elsewhere)),
            &["serve", "--profile", "work"],
        );
        let err = reregister(&reg, Path::new(TEST_EXE), None, Some(Path::new(here))).unwrap_err();
        assert!(err.contains("--scope project"), "{err}");
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Local(PathBuf::from(here)),
            &["serve", "--profile", "work"],
        );
        assert!(reregister(&reg, Path::new(TEST_EXE), None, Some(Path::new(here))).is_err());
        let reg = registration(
            registrations::Client::Codex,
            registrations::Scope::User,
            &["auth", "status"],
        );
        let err = reregister(&reg, Path::new(TEST_EXE), None, None).unwrap_err();
        assert!(err.contains("does not run `wiki-mcp serve`"), "{err}");
    }

    #[test]
    fn identity_matches_serve_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let tokens = MemoryStore::new();
        let adapter = adapter(dir.path(), &tokens);
        let id = adapter.identity();
        assert_eq!(id.exe_stem, "wiki-mcp");
        assert_eq!(id.default_profile, DEFAULT_PROFILE);
        assert_eq!(adapter.service_name(), "Yandex Wiki");
    }
}
