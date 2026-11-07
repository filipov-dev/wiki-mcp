//! `wiki-mcp auth` — токены и организации профилей.
//!
//! Токен принимается только скрытым вводом или из stdin (`--token-stdin`), хранится только в
//! системном хранилище и нигде не печатается. Команды, которая выводит значение токена, нет и
//! быть не должно.
//!
//! IAM-токены из источника (`--iam-source yc` или `--iam-source service-account-key`):
//! токена в хранилище нет; в профиль пишутся тип, источник, организация Yandex Cloud и
//! настройки `yc`, а ключ сервисного аккаунта читается из файла и сохраняется только в
//! системном хранилище (путь к файлу не сохраняется). Проверка получает токен из источника и
//! сразу его забывает.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use wiki_api::Organization;
use wiki_api::auth::service_account_key_account;
use wiki_api::auth::{AuthError, KEYRING_SERVICE, KeyringStore, TokenStore};
use wiki_api::iam::{IamSource, KeyExchange};

use crate::login::{
    apply_yc_flags, check_iam, check_new_token, org_from_flags, resolve_iam_org, resolve_org,
    save_service_account_key, save_token, service_account_key, validate_token,
};
use crate::oauth::{AUTHORIZE_URL, authorize_url};
use crate::profiles::{
    DEFAULT_PROFILE, IamSourceKind, ProfileRepo, ProfileSettings, TokenType, check_iam_org,
    validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};
use crate::verify::{ApiVerifier, TokenVerifier};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a Wiki token (OAuth or IAM) and the organization for a profile, or set the
    /// profile up to get Yandex Cloud IAM tokens itself (--iam-source).
    ///
    /// The token is read from a hidden prompt (or from stdin with --token-stdin), checked
    /// with GET /v1/users/me and saved in the system keychain. It is never written to
    /// profiles.toml. Replaces the stored token if there is one.
    ///
    /// Federated and service accounts of a Yandex Cloud organization: with --iam-source yc
    /// the server gets IAM tokens from `yc iam create-token`; with --service-account-key
    /// <FILE> it gets them for the authorized key of a service account, which is stored in
    /// the system keychain (the file path is not saved). No token is stored: tokens are
    /// renewed before they expire and kept in memory only.
    Login(LoginArgs),
    /// Show the profile, its organization and the Wiki user of the stored token.
    ///
    /// The token value is never printed.
    Status(StatusArgs),
    /// Remove the stored token and service account key of the profile from the system
    /// keychain.
    Logout(LogoutArgs),
}

#[derive(Debug, Clone, Args)]
pub struct LoginArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Yandex 360 for Business organization ID (sent as X-Org-Id).
    #[arg(long, value_name = "ID", conflicts_with = "cloud_org_id")]
    pub org_id: Option<String>,
    /// Yandex Cloud Organization ID (sent as X-Cloud-Org-Id).
    #[arg(long, value_name = "ID")]
    pub cloud_org_id: Option<String>,
    /// Token type [default: the type stored in the profile, otherwise oauth].
    ///
    /// A pasted IAM token expires within 12 hours; for long-running agents use --iam-source
    /// yc or --service-account-key instead.
    #[arg(long, value_enum, value_name = "TYPE")]
    pub token_type: Option<TokenType>,
    /// Where IAM tokens come from; implies --token-type iam [default: the source saved in the
    /// profile, otherwise token].
    #[arg(long, value_enum, value_name = "SOURCE")]
    pub iam_source: Option<IamSourceKind>,
    /// Authorized key file of a service account (`yc iam key create --output key.json`).
    /// Its content is stored in the system keychain, the path is not saved. Implies
    /// --iam-source service-account-key.
    #[arg(long, value_name = "FILE")]
    pub service_account_key: Option<PathBuf>,
    /// IAM from yc: absolute path to the yc CLI [default: yc from PATH]. Implies
    /// --iam-source yc.
    #[arg(long, value_name = "PATH")]
    pub yc_path: Option<PathBuf>,
    /// IAM from yc: yc profile to get tokens with (yc --profile) [default: the active yc
    /// profile]. Implies --iam-source yc.
    #[arg(long, value_name = "NAME")]
    pub yc_profile: Option<String>,
    /// Read the token from stdin instead of a hidden prompt.
    ///
    /// The organization is not asked interactively in this mode: pass --org-id or
    /// --cloud-org-id unless the profile already has one.
    #[arg(long)]
    pub token_stdin: bool,
    /// Store the token without checking it against the Wiki API (with --iam-source yc or a
    /// service account key: without getting an IAM token).
    #[arg(long)]
    pub skip_check: bool,
}

/// Как профиль получает токен после `auth login`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    /// Токен вводится и сохраняется в системном хранилище.
    Stored(TokenType),
    /// IAM-токены из источника (`yc` или ключ сервисного аккаунта).
    Iam(IamSourceKind),
}

impl LoginArgs {
    /// Способ получения токена: флаги, иначе сохранённый в профиле. Флаги источника
    /// IAM-токенов подразумевают `--token-type iam`.
    fn choice(&self, settings: &ProfileSettings) -> anyhow::Result<Choice> {
        let yc_flags = self.yc_path.is_some() || self.yc_profile.is_some();
        let from_flags = match (
            self.iam_source,
            self.service_account_key.is_some(),
            yc_flags,
        ) {
            (Some(source), true, _) if source != IamSourceKind::ServiceAccountKey => {
                bail!("--service-account-key is used only with --iam-source service-account-key")
            }
            (_, true, true) => {
                bail!("--yc-path and --yc-profile cannot be used with --service-account-key")
            }
            (_, true, false) => Some(IamSourceKind::ServiceAccountKey),
            (Some(source), false, true) if source != IamSourceKind::Yc => {
                bail!("--yc-path and --yc-profile are used only with --iam-source yc")
            }
            (Some(source), false, _) => Some(source),
            (None, false, true) => Some(IamSourceKind::Yc),
            (None, false, false) => None,
        };
        if let Some(source) = from_flags {
            if self.token_type == Some(TokenType::OAuth) {
                bail!(
                    "--iam-source, --service-account-key and the yc flags need IAM tokens, not --token-type oauth"
                );
            }
            return Ok(match source {
                IamSourceKind::Token => Choice::Stored(TokenType::Iam),
                source => Choice::Iam(source),
            });
        }
        let saved = settings.refreshing_source();
        Ok(match (self.token_type, saved) {
            (Some(TokenType::OAuth), _) => Choice::Stored(TokenType::OAuth),
            (Some(TokenType::Iam) | None, Some(source)) => Choice::Iam(source),
            (Some(TokenType::Iam), None) => Choice::Stored(TokenType::Iam),
            (None, None) => Choice::Stored(settings.token_type),
        })
    }
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Do not query the Wiki API for the current user (and do not get an IAM token).
    #[arg(long)]
    pub skip_check: bool,
}

#[derive(Debug, Clone, Args)]
pub struct LogoutArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Also remove the profile settings from profiles.toml.
    #[arg(long)]
    pub forget: bool,
}

pub async fn run(command: AuthCommand) -> anyhow::Result<()> {
    let store = KeyringStore;
    let repo = ProfileRepo::open_default()?;
    let mut ui = TerminalUi;
    match command {
        AuthCommand::Login(args) => {
            let verifier = (!args.skip_check).then(ApiVerifier::default);
            login(&store, &repo, verifier.as_ref(), &mut ui, &args).await
        }
        AuthCommand::Status(args) => {
            let verifier = (!args.skip_check).then(ApiVerifier::default);
            status(&store, &repo, verifier.as_ref(), &mut ui, &args.profile).await
        }
        AuthCommand::Logout(args) => logout(&store, &repo, &mut ui, &args.profile, args.forget),
    }
}

/// Подсказка, где взять токен.
fn token_hint(settings: &ProfileSettings, token_type: TokenType) -> String {
    match (token_type, &settings.client_id) {
        (TokenType::Iam, _) => "Get an IAM token of the Yandex Cloud account the agent will work \
             as, for example with `yc iam create-token`. IAM tokens expire within 12 hours; \
             store a new one with `wiki-mcp auth login` when it does, or let wiki-mcp get them \
             itself with --iam-source yc or --service-account-key <FILE>."
            .to_owned(),
        (TokenType::OAuth, Some(client_id)) => format!(
            "Get a token from your OAuth application: {}",
            authorize_url(client_id)
        ),
        (TokenType::OAuth, None) => format!(
            "Get an OAuth token from your own Yandex OAuth application with Wiki access \
             (wiki:read, plus wiki:write if the agent will change data): \
             {AUTHORIZE_URL}?response_type=token&client_id=<ClientID> \
             (`wiki-mcp connect` helps to create the application)"
        ),
    }
}

pub async fn login<V: TokenVerifier>(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    args: &LoginArgs,
) -> anyhow::Result<()> {
    let profile = args.profile.as_str();
    validate_profile_name(profile)?;
    let mut settings = repo.get(profile)?.unwrap_or_default();
    let token_type = match args.choice(&settings)? {
        Choice::Stored(token_type) => token_type,
        Choice::Iam(source) => {
            return login_iam(store, repo, verifier, ui, args, settings, source).await;
        }
    };

    let from_flags = org_from_flags(args.org_id.as_deref(), args.cloud_org_id.as_deref())?;
    let cannot_ask = args.token_stdin.then(|| {
        format!(
            "profile `{profile}` has no organization: pass --org-id <ID> (Yandex 360) or \
             --cloud-org-id <ID> (Yandex Cloud) together with --token-stdin"
        )
    });
    let org = resolve_org(ui, from_flags, settings.org.clone(), cannot_ask)?;

    let token = if args.token_stdin {
        ui.read_token_stdin()
            .context("failed to read the token from stdin")?
    } else {
        ui.say(&token_hint(&settings, token_type));
        ui.read_token(&format!(
            "Paste the token for profile `{profile}` (input is hidden): "
        ))
        .context("failed to read the token")?
    };
    validate_token(&token)?;
    check_new_token(ui, verifier, &token, token_type, &org).await?;
    save_token(ui, store, profile, &token)?;

    // Профиль, который получал IAM-токены сам, переходит на сохранённый токен: настройки
    // источника больше не нужны.
    let had_key = settings.refreshing_source() == Some(IamSourceKind::ServiceAccountKey);
    settings.use_stored_token(token_type);
    settings.org = Some(org);
    repo.upsert(profile, settings)?;
    ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        repo.path().display()
    ));
    if had_key {
        unused_key_hint(store, ui, profile);
    }
    Ok(())
}

/// `auth login` с IAM-токенами из `yc` или по ключу сервисного аккаунта: тип, источник,
/// организация Yandex Cloud и настройки `yc` — в профиль, ключ — в системное хранилище,
/// токена в хранилище нет.
async fn login_iam<V: TokenVerifier>(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    args: &LoginArgs,
    mut settings: ProfileSettings,
    source: IamSourceKind,
) -> anyhow::Result<()> {
    let profile = args.profile.as_str();
    if args.token_stdin {
        bail!(
            "--token-stdin is not used with --iam-source {}: wiki-mcp gets IAM tokens itself",
            source_name(source)
        );
    }
    let previous = settings.refreshing_source();
    // Ключ из файла читается до вопросов: ошибка в файле — до организации.
    let (iam, new_key) = match source {
        IamSourceKind::Yc => {
            let command = apply_yc_flags(
                &mut settings,
                args.yc_path.as_deref(),
                args.yc_profile.as_deref(),
            )?;
            (IamSource::Yc(command), None)
        }
        _ => {
            let (key, from_file) =
                service_account_key(store, profile, args.service_account_key.as_deref())?;
            let exchange = KeyExchange::new(key.clone())?;
            (
                IamSource::ServiceAccountKey(exchange),
                from_file.then_some(key),
            )
        }
    };
    let org = resolve_iam_org(
        ui,
        profile,
        args.org_id.as_deref(),
        args.cloud_org_id.as_deref(),
        settings.org.clone(),
    )?;
    check_iam(ui, verifier, &iam, &org).await?;
    if let Some(key) = &new_key {
        save_service_account_key(ui, store, profile, key)?;
    }

    settings.use_iam_source(source);
    settings.org = Some(org);
    repo.upsert(profile, settings)?;
    ui.say(&format!(
        "Profile `{profile}` saved to {}: IAM tokens from {}, renewed before they expire; no \
         token is stored.",
        repo.path().display(),
        iam.display()
    ));
    // Ошибка хранилища здесь не важна: для `yc` оно не нужно.
    if previous.is_none() && matches!(store.contains(profile), Ok(true)) {
        ui.say(&format!(
            "A token of profile `{profile}` is still in the system keychain; it is not used \
             now. Remove it with `wiki-mcp auth logout --profile {profile}` (this also removes \
             the service account key) or keep it to switch back later."
        ));
    }
    if source == IamSourceKind::Yc && previous == Some(IamSourceKind::ServiceAccountKey) {
        unused_key_hint(store, ui, profile);
    }
    Ok(())
}

/// Подсказка про ключ сервисного аккаунта, который профиль больше не использует.
fn unused_key_hint(store: &dyn TokenStore, ui: &mut dyn Ui, profile: &str) {
    if matches!(store.contains_service_account_key(profile), Ok(true)) {
        ui.say(&format!(
            "The service account key of profile `{profile}` is still in the system keychain \
             (account `{}`); it is not used now. Remove it with `wiki-mcp auth logout \
             --profile {profile}` and store the token again, or keep it to switch back with \
             --iam-source service-account-key.",
            service_account_key_account(profile)
        ));
    }
}

/// Имя источника, как во флаге `--iam-source`.
fn source_name(source: IamSourceKind) -> &'static str {
    match source {
        IamSourceKind::Token => "token",
        IamSourceKind::Yc => "yc",
        IamSourceKind::ServiceAccountKey => "service-account-key",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

pub async fn status<V: TokenVerifier>(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let settings = repo.get(profile)?;
    let org = settings.as_ref().and_then(|settings| settings.org.clone());
    let token_type = settings
        .as_ref()
        .map_or_else(TokenType::default, |settings| settings.token_type);
    let source = settings
        .as_ref()
        .and_then(ProfileSettings::refreshing_source);

    ui.say(&format!("Profile:   {profile}"));
    let (user, wiki_org) = match source {
        None => status_stored(store, verifier, ui, profile, token_type, org.as_ref()).await?,
        Some(source) => {
            let settings = settings.clone().unwrap_or_default();
            status_iam(
                store,
                verifier,
                ui,
                profile,
                &settings,
                source,
                org.as_ref(),
            )
            .await
        }
    };
    ui.say(&format!(
        "Org:       {}",
        org.as_ref()
            .map_or_else(|| "not set".to_owned(), Organization::to_string)
    ));
    ui.say(&format!("User:      {user}"));
    ui.say(&format!("Wiki org:  {wiki_org}"));
    match &settings {
        Some(settings) => {
            ui.say(&format!(
                "Client ID: {}",
                settings.client_id.as_deref().unwrap_or("-")
            ));
            ui.say(&format!(
                "Client:    {}",
                settings.client.as_deref().unwrap_or("-")
            ));
            // Действующий уровень, как у `serve`: удаление подразумевает запись.
            ui.say(&format!("Write:     {}", yes_no(settings.writes_allowed())));
            ui.say(&format!(
                "Delete:    {}",
                yes_no(settings.allow_destructive)
            ));
            let toolsets = if settings.toolsets.is_empty() {
                "default".to_owned()
            } else {
                settings.toolsets.join(",")
            };
            ui.say(&format!("Toolsets:  {toolsets}"));
        }
        None => ui.say("Settings:  none (read-only defaults)"),
    }
    ui.say(&format!("Profiles:  {}", repo.path().display()));
    Ok(())
}

/// Строки `Token` и `Type` профиля с сохранённым токеном; возвращает строки `User` и
/// `Wiki org`.
async fn status_stored<V: TokenVerifier>(
    store: &dyn TokenStore,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
    token_type: TokenType,
    org: Option<&Organization>,
) -> anyhow::Result<(String, String)> {
    let token = match store.get(profile) {
        Ok(token) => Some(token),
        Err(AuthError::NotFound { .. }) => None,
        Err(err) => return Err(err.into()),
    };
    ui.say(&format!(
        "Token:     {}",
        if token.is_some() {
            format!("stored in the system keychain (service `{KEYRING_SERVICE}`)")
        } else {
            "not found".to_owned()
        }
    ));
    ui.say(&format!(
        "Type:      {} (Authorization: {})",
        token_type.label(),
        token_type.auth_scheme()
    ));
    // Пользователь и организация — по ответу Wiki на запрос `/v1/users/me`.
    Ok(match (&token, org, verifier) {
        (Some(token), Some(org), Some(verifier)) => {
            match verifier.current_user(token, token_type, org).await {
                Ok(user) => {
                    let wiki_org = user.org.clone().unwrap_or_else(|| "-".to_owned());
                    (user.to_string(), wiki_org)
                }
                Err(err) => (format!("check failed: {err:#}"), "-".to_owned()),
            }
        }
        (Some(_), Some(_), None) => ("not checked (--skip-check)".to_owned(), "-".to_owned()),
        _ => ("-".to_owned(), "-".to_owned()),
    })
}

/// Строки `Token`, `Type` и `Source` профиля с IAM-токенами из источника; возвращает строки
/// `User` и `Wiki org`. С проверкой токен получается из источника и сразу забывается.
async fn status_iam<V: TokenVerifier>(
    store: &dyn TokenStore,
    verifier: Option<&V>,
    ui: &mut dyn Ui,
    profile: &str,
    settings: &ProfileSettings,
    source: IamSourceKind,
    org: Option<&Organization>,
) -> (String, String) {
    ui.say("Token:     not stored; obtained when needed and kept in memory only");
    ui.say(&format!(
        "Type:      {} (Authorization: {})",
        TokenType::Iam.label(),
        TokenType::Iam.auth_scheme()
    ));
    let iam = match source {
        IamSourceKind::Yc => settings.yc_command().map(IamSource::Yc),
        _ => match store.get_service_account_key(profile) {
            Ok(key) => {
                ui.say(&format!(
                    "Key:       stored in the system keychain (service `{KEYRING_SERVICE}`, \
                     account `{}`)",
                    service_account_key_account(profile)
                ));
                KeyExchange::new(key)
                    .map(IamSource::ServiceAccountKey)
                    .map_err(anyhow::Error::from)
            }
            Err(err) => Err(err.into()),
        },
    };
    let none = || "-".to_owned();
    let iam = match iam {
        Ok(iam) => {
            ui.say(&format!("Source:    {}", iam.display()));
            iam
        }
        Err(err) => {
            ui.say(&format!("Source:    {} ({err:#})", source_name(source)));
            return (none(), none());
        }
    };
    let Some(org) = org else {
        return (none(), none());
    };
    if let Err(err) = check_iam_org(profile, org) {
        return (format!("check failed: {err:#}"), none());
    }
    let Some(verifier) = verifier else {
        return ("not checked (--skip-check)".to_owned(), none());
    };
    let iam = match iam {
        IamSource::ServiceAccountKey(exchange) => match verifier.key_exchange(exchange) {
            Ok(exchange) => IamSource::ServiceAccountKey(exchange),
            Err(err) => return (format!("check failed: {err:#}"), none()),
        },
        other => other,
    };
    let issued = match iam.issue(std::time::SystemTime::now()).await {
        Ok(issued) => issued,
        Err(err) => return (format!("check failed: {err}"), none()),
    };
    match verifier
        .current_user(&issued.token, TokenType::Iam, org)
        .await
    {
        Ok(user) => {
            let wiki_org = user.org.clone().unwrap_or_else(none);
            (user.to_string(), wiki_org)
        }
        Err(err) => (format!("check failed: {err:#}"), none()),
    }
}

pub fn logout(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    forget: bool,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    let source = repo
        .get(profile)?
        .and_then(|settings| settings.refreshing_source());
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "Token of profile `{profile}` removed from the system keychain. If it is no longer \
             needed anywhere, also revoke it in your Yandex ID account."
        )),
        Err(AuthError::NotFound { .. }) => {
            if source.is_none() {
                ui.say(&format!("No token stored for profile `{profile}`."));
            }
        }
        // Профилю с `yc` хранилище не нужно: его недоступность — не ошибка.
        Err(AuthError::Unavailable(_)) if source == Some(IamSourceKind::Yc) => {}
        Err(err) => return Err(err.into()),
    }
    match store.delete_service_account_key(profile) {
        Ok(()) => ui.say(&format!(
            "Service account key of profile `{profile}` removed from the system keychain. If \
             it is no longer needed anywhere, also delete the key of the service account in \
             Yandex Cloud (`yc iam key delete`)."
        )),
        Err(AuthError::KeyNotFound { .. }) => {
            if source == Some(IamSourceKind::ServiceAccountKey) {
                ui.say(&format!(
                    "No service account key stored for profile `{profile}`."
                ));
            }
        }
        Err(AuthError::Unavailable(_)) if source != Some(IamSourceKind::ServiceAccountKey) => {}
        Err(err) => return Err(err.into()),
    }
    if source == Some(IamSourceKind::Yc) {
        ui.say(&format!(
            "Profile `{profile}` gets IAM tokens from yc, which wiki-mcp never stores. To stop \
             using it, remove the profile with --forget or the MCP server from the client."
        ));
    }
    if forget {
        if repo.remove(profile)? {
            ui.say(&format!(
                "Profile `{profile}` removed from {}.",
                repo.path().display()
            ));
        } else {
            ui.say(&format!("Profile `{profile}` has no saved settings."));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiki_api::auth::MemoryStore;
    use wiki_api::{ApiToken, OrgKind};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::ui::testing::ScriptedUi;
    use crate::verify::ME_PATH;
    use crate::verify::testing::FakeVerifier;

    fn verifier() -> FakeVerifier {
        FakeVerifier {
            valid_token: "good-token".into(),
        }
    }

    fn args(profile: &str) -> LoginArgs {
        LoginArgs {
            profile: profile.into(),
            org_id: None,
            cloud_org_id: None,
            token_type: None,
            iam_source: None,
            service_account_key: None,
            yc_path: None,
            yc_profile: None,
            token_stdin: false,
            skip_check: false,
        }
    }

    fn setup() -> (tempfile::TempDir, ProfileRepo, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        (dir, repo, MemoryStore::new())
    }

    #[tokio::test]
    async fn login_status_logout() {
        let (_dir, repo, store) = setup();
        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier()), &mut ui, "ci")
            .await
            .unwrap();
        assert!(ui.text().contains("Token:     not found"), "{}", ui.text());
        assert!(ui.text().contains("Org:       not set"), "{}", ui.text());

        let mut ui = ScriptedUi {
            tokens: ["good-token".to_owned()].into(),
            ..Default::default()
        };
        let login_args = LoginArgs {
            org_id: Some("12345".into()),
            ..args("ci")
        };
        login(&store, &repo, Some(&verifier()), &mut ui, &login_args)
            .await
            .unwrap();
        assert!(ui.questions.is_empty());
        assert!(ui.text().contains("signed in to Wiki as `test-user`"));
        assert!(ui.text().contains("Token saved"), "{}", ui.text());
        assert_eq!(store.get("ci").unwrap().expose(), "good-token");
        let saved = repo.get("ci").unwrap().unwrap();
        assert_eq!(saved.org.unwrap().header(), ("X-Org-Id", "12345"));
        assert_eq!(saved.token_type, TokenType::OAuth);
        assert!(!saved.allow_write);
        // Токен в файл профилей не пишется.
        let file = std::fs::read_to_string(repo.path()).unwrap();
        assert!(!file.contains("good-token"), "{file}");

        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier()), &mut ui, "ci")
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("Token:     stored"), "{text}");
        assert!(
            text.contains("Org:       Yandex 360 `12345` (X-Org-Id)"),
            "{text}"
        );
        assert!(
            text.contains("Type:      OAuth token (Authorization: OAuth)"),
            "{text}"
        );
        assert!(text.contains("User:      test-user"), "{text}");
        assert!(text.contains("Wiki org:  dir_id 12345"), "{text}");
        assert!(text.contains("Write:     no"), "{text}");
        assert!(!text.contains("good-token"), "{text}");

        let mut ui = ScriptedUi::default();
        logout(&store, &repo, &mut ui, "ci", false).unwrap();
        assert!(!store.contains("ci").unwrap());
        assert!(repo.get("ci").unwrap().is_some());

        // Повторный logout — не ошибка; --forget удаляет настройки.
        logout(&store, &repo, &mut ui, "ci", true).unwrap();
        assert!(ui.text().contains("No token stored"));
        assert_eq!(repo.get("ci").unwrap(), None);
    }

    #[tokio::test]
    async fn login_asks_org_interactively() {
        let (_dir, repo, store) = setup();
        let mut ui = ScriptedUi {
            tokens: ["good-token".to_owned()].into(),
            answers: ["2".to_owned(), " cloud-org ".to_owned()].into(),
            ..Default::default()
        };
        login(
            &store,
            &repo,
            Some(&verifier()),
            &mut ui,
            &args(DEFAULT_PROFILE),
        )
        .await
        .unwrap();
        assert_eq!(ui.questions.len(), 2);
        let org = repo.get(DEFAULT_PROFILE).unwrap().unwrap().org.unwrap();
        assert_eq!(org.header(), ("X-Cloud-Org-Id", "cloud-org"));

        // Неизвестный тип — ошибка, ничего не сохраняется.
        let mut ui = ScriptedUi {
            answers: ["3".to_owned()].into(),
            ..Default::default()
        };
        let err = login(&store, &repo, Some(&verifier()), &mut ui, &args("other"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown organization type"),
            "{err}"
        );
        assert!(!store.contains("other").unwrap());
    }

    #[tokio::test]
    async fn token_stdin_keeps_settings_and_replaces_token() {
        let (_dir, repo, store) = setup();
        let settings = ProfileSettings {
            client_id: Some("abc123".into()),
            allow_write: true,
            toolsets: vec!["pages".into()],
            org: Some(Organization::new(OrgKind::Yandex360, "1").unwrap()),
            ..Default::default()
        };
        repo.upsert("codex", settings.clone()).unwrap();
        store.set("codex", &ApiToken::new("old")).unwrap();

        let mut ui = ScriptedUi {
            stdin_token: Some("new-token\n".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            token_stdin: true,
            skip_check: true,
            ..args("codex")
        };
        login::<FakeVerifier>(&store, &repo, None, &mut ui, &login_args)
            .await
            .unwrap();
        assert_eq!(store.get("codex").unwrap().expose(), "new-token");
        assert_eq!(repo.get("codex").unwrap(), Some(settings));
        let text = ui.text();
        assert!(text.contains("Token replaced"), "{text}");
        assert!(text.contains("--skip-check"), "{text}");
        assert!(ui.questions.is_empty());

        // Флаг меняет организацию сохранённого профиля.
        let mut ui = ScriptedUi {
            stdin_token: Some("new-token".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            cloud_org_id: Some("moved".into()),
            ..login_args
        };
        login::<FakeVerifier>(&store, &repo, None, &mut ui, &login_args)
            .await
            .unwrap();
        let org = repo.get("codex").unwrap().unwrap().org.unwrap();
        assert_eq!(org.header(), ("X-Cloud-Org-Id", "moved"));
    }

    #[tokio::test]
    async fn token_stdin_requires_org() {
        let (_dir, repo, store) = setup();
        let mut ui = ScriptedUi {
            stdin_token: Some("good-token".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            token_stdin: true,
            ..args("ci")
        };
        let err = login(&store, &repo, Some(&verifier()), &mut ui, &login_args)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--org-id"), "{err}");
        assert!(!store.contains("ci").unwrap());
    }

    #[tokio::test]
    async fn login_rejects_invalid_tokens() {
        let (_dir, repo, store) = setup();
        let login_args = LoginArgs {
            org_id: Some("1".into()),
            ..args("ci")
        };
        for (value, expected) in [
            ("bad", "rejected the token"),
            ("", "empty token"),
            ("two words", "does not look like"),
        ] {
            let mut ui = ScriptedUi {
                tokens: [value.to_owned()].into(),
                ..Default::default()
            };
            let err = login(&store, &repo, Some(&verifier()), &mut ui, &login_args)
                .await
                .unwrap_err();
            assert!(err.to_string().contains(expected), "{err}");
            if !value.is_empty() {
                assert!(!err.to_string().contains(value), "{err}");
            }
        }
        assert!(!store.contains("ci").unwrap());
        assert_eq!(repo.get("ci").unwrap(), None);

        // Неверный ID организации отклоняется до чтения токена.
        let mut ui = ScriptedUi::default();
        let bad_org = LoginArgs {
            org_id: Some("a b".into()),
            ..args("ci")
        };
        let err = login(&store, &repo, Some(&verifier()), &mut ui, &bad_org)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("organization ID"), "{err}");
    }

    #[tokio::test]
    async fn status_reports_failed_check_and_effective_access() {
        let (_dir, repo, store) = setup();
        std::fs::write(
            repo.path(),
            "[profiles.old]\nallow_destructive = true\n\n[profiles.old.org]\ntype = \"cloud\"\nid = \"o\"\n",
        )
        .unwrap();
        store.set("old", &ApiToken::new("stale-value")).unwrap();
        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier()), &mut ui, "old")
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("User:      check failed"), "{text}");
        assert!(text.contains("Write:     yes"), "{text}");
        assert!(text.contains("Delete:    yes"), "{text}");
        assert!(!text.contains("stale-value"), "{text}");

        let mut ui = ScriptedUi::default();
        status::<FakeVerifier>(&store, &repo, None, &mut ui, "old")
            .await
            .unwrap();
        assert!(ui.text().contains("not checked (--skip-check)"));
    }

    #[tokio::test]
    async fn iam_token_type_is_saved_and_kept() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ME_PATH))
            .and(header("authorization", "Bearer iam-value"))
            .and(header("x-cloud-org-id", "cloud-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "username": "robot",
                "identity": {"cloud_uid": "cloud-user"},
                "org": {}
            })))
            .expect(3)
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(server.uri());
        let (_dir, repo, store) = setup();

        let mut ui = ScriptedUi {
            tokens: ["iam-value".to_owned()].into(),
            ..Default::default()
        };
        let login_args = LoginArgs {
            cloud_org_id: Some("cloud-org".into()),
            token_type: Some(TokenType::Iam),
            ..args("cloud")
        };
        login(&store, &repo, Some(&verifier), &mut ui, &login_args)
            .await
            .unwrap();
        assert!(ui.text().contains("yc iam create-token"), "{}", ui.text());
        let saved = repo.get("cloud").unwrap().unwrap();
        assert_eq!(saved.token_type, TokenType::Iam);

        // Без флага остаётся тип из профиля.
        let mut ui = ScriptedUi {
            stdin_token: Some("iam-value".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            token_stdin: true,
            ..args("cloud")
        };
        login(&store, &repo, Some(&verifier), &mut ui, &login_args)
            .await
            .unwrap();
        assert_eq!(
            repo.get("cloud").unwrap().unwrap().token_type,
            TokenType::Iam
        );

        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier), &mut ui, "cloud")
            .await
            .unwrap();
        let text = ui.text();
        assert!(
            text.contains("Type:      Yandex Cloud IAM token (Authorization: Bearer)"),
            "{text}"
        );
        assert!(text.contains("User:      robot"), "{text}");
        assert!(text.contains("Wiki org:  -"), "{text}");
        assert!(!text.contains("iam-value"), "{text}");
    }

    /// Полный сценарий с настоящим HTTP-клиентом против локального wiremock.
    #[tokio::test]
    async fn login_and_status_against_mock_wiki() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ME_PATH))
            .and(header("authorization", "OAuth mock-token"))
            .and(header("x-cloud-org-id", "mock-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "username": "jdoe",
                "identity": {"uid": "1000"},
                "org": {"dir_id": null, "collab_id": "00000000-0000-0000-0000-000000000002"}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(server.uri());
        let (_dir, repo, store) = setup();

        let mut ui = ScriptedUi {
            stdin_token: Some("mock-token".to_owned()),
            ..Default::default()
        };
        let login_args = LoginArgs {
            cloud_org_id: Some("mock-org".into()),
            token_stdin: true,
            ..args(DEFAULT_PROFILE)
        };
        login(&store, &repo, Some(&verifier), &mut ui, &login_args)
            .await
            .unwrap();
        assert!(ui.text().contains("as `jdoe`"), "{}", ui.text());

        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier), &mut ui, DEFAULT_PROFILE)
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("User:      jdoe"), "{text}");
        assert!(
            text.contains("Wiki org:  collab_id 00000000-0000-0000-0000-000000000002"),
            "{text}"
        );
        assert!(!text.contains("mock-token"), "{text}");
    }

    #[test]
    fn iam_flags_choose_the_source() {
        let saved_yc = {
            let mut settings = ProfileSettings::default();
            settings.use_iam_source(IamSourceKind::Yc);
            settings
        };
        let plain = ProfileSettings::default();
        let with = |change: &dyn Fn(&mut LoginArgs)| {
            let mut args = args("p");
            change(&mut args);
            args
        };
        let cases: [(LoginArgs, &ProfileSettings, Choice); 7] = [
            (args("p"), &plain, Choice::Stored(TokenType::OAuth)),
            (args("p"), &saved_yc, Choice::Iam(IamSourceKind::Yc)),
            (
                with(&|a| a.token_type = Some(TokenType::Iam)),
                &plain,
                Choice::Stored(TokenType::Iam),
            ),
            (
                with(&|a| a.token_type = Some(TokenType::OAuth)),
                &saved_yc,
                Choice::Stored(TokenType::OAuth),
            ),
            (
                with(&|a| a.yc_profile = Some("work".into())),
                &plain,
                Choice::Iam(IamSourceKind::Yc),
            ),
            (
                with(&|a| a.service_account_key = Some("key.json".into())),
                &saved_yc,
                Choice::Iam(IamSourceKind::ServiceAccountKey),
            ),
            (
                with(&|a| a.iam_source = Some(IamSourceKind::Token)),
                &saved_yc,
                Choice::Stored(TokenType::Iam),
            ),
        ];
        for (args, settings, expected) in cases {
            assert_eq!(args.choice(settings).unwrap(), expected, "{args:?}");
        }
        for bad in [
            with(&|a| {
                a.iam_source = Some(IamSourceKind::Yc);
                a.service_account_key = Some("key.json".into());
            }),
            with(&|a| {
                a.yc_path = Some("/bin/yc".into());
                a.service_account_key = Some("key.json".into());
            }),
            with(&|a| {
                a.iam_source = Some(IamSourceKind::Token);
                a.yc_profile = Some("work".into());
            }),
            with(&|a| {
                a.token_type = Some(TokenType::OAuth);
                a.iam_source = Some(IamSourceKind::Yc);
            }),
        ] {
            assert!(args("p").choice(&plain).is_ok());
            assert!(bad.choice(&plain).is_err(), "{bad:?}");
        }
    }

    /// `yc` — подменный скрипт: настоящий `yc` не запускается, токен не сохраняется.
    #[cfg(unix)]
    #[tokio::test]
    async fn yc_source_saves_profile_without_token() {
        use crate::login::testing::FakeYc;

        let yc = FakeYc::new();
        let (_dir, repo, store) = setup();
        let verifier = FakeVerifier {
            valid_token: "iam-token-1".into(),
        };
        let login_args = LoginArgs {
            cloud_org_id: Some("cloud-org".into()),
            yc_path: Some(yc.path()),
            yc_profile: Some("work".into()),
            ..args("fed")
        };
        let mut ui = ScriptedUi::default();
        login(&store, &repo, Some(&verifier), &mut ui, &login_args)
            .await
            .unwrap();
        assert_eq!(yc.calls(), 1);
        assert_eq!(
            yc.args().trim(),
            "iam create-token --profile work --format json"
        );
        let text = ui.text();
        assert!(text.contains("signed in to Wiki as `test-user`"), "{text}");
        assert!(!text.contains("iam-token-1"), "{text}");
        assert!(!store.contains("fed").unwrap());
        let saved = repo.get("fed").unwrap().unwrap();
        assert_eq!(saved.token_type, TokenType::Iam);
        assert_eq!(saved.iam_source, IamSourceKind::Yc);
        assert_eq!(saved.yc_profile.as_deref(), Some("work"));
        let file = std::fs::read_to_string(repo.path()).unwrap();
        assert!(!file.contains("iam-token"), "{file}");

        // Статус получает токен из yc и не печатает его.
        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier), &mut ui, "fed")
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("Token:     not stored"), "{text}");
        assert!(text.contains("iam create-token --profile work"), "{text}");
        // Второй вызов yc выдал `iam-token-2`, а проверяющий принимает только первый.
        assert!(text.contains("User:      check failed"), "{text}");
        assert!(!text.contains("iam-token-2"), "{text}");

        // Ошибка yc — ничего не сохраняется, секрет из stderr не показывается.
        yc.fail();
        let mut ui = ScriptedUi::default();
        let err = login(&store, &repo, Some(&verifier), &mut ui, &login_args)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Nothing was saved"), "{err}");
        assert!(!err.to_string().contains("LeakedSecret"), "{err}");

        // Организация Yandex 360 с yc — ошибка.
        let wrong = LoginArgs {
            cloud_org_id: None,
            org_id: Some("1".into()),
            ..login_args.clone()
        };
        let err = login(&store, &repo, Some(&verifier), &mut ui, &wrong)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--cloud-org-id"), "{err}");

        let mut ui = ScriptedUi::default();
        logout(&store, &repo, &mut ui, "fed", false).unwrap();
        assert!(ui.text().contains("never stores"), "{}", ui.text());

        // Переход обратно на OAuth убирает настройки yc.
        let mut ui = ScriptedUi {
            tokens: ["good-token".to_owned()].into(),
            ..Default::default()
        };
        let oauth = LoginArgs {
            token_type: Some(TokenType::OAuth),
            ..args("fed")
        };
        login(
            &store,
            &repo,
            Some(&super::tests::verifier()),
            &mut ui,
            &oauth,
        )
        .await
        .unwrap();
        let saved = repo.get("fed").unwrap().unwrap();
        assert_eq!(saved.token_type, TokenType::OAuth);
        assert_eq!(saved.iam_source, IamSourceKind::Token);
        assert_eq!(saved.yc_path, None);
    }

    /// Ключ сервисного аккаунта: IAM API и Wiki API — на `wiremock`, ключ создаётся в тесте.
    #[tokio::test]
    async fn service_account_key_is_stored_in_keychain_only() {
        use crate::login::key_testing::{KEY_ID, SA_ID, key_file};

        let iam = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/iam/v1/tokens"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "iamToken": "sa-token",
                "expiresAt": "2099-01-01T00:00:00Z"
            })))
            .mount(&iam)
            .await;
        let wiki = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ME_PATH))
            .and(header("authorization", "Bearer sa-token"))
            .and(header("x-cloud-org-id", "cloud-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "username": "robot",
                "identity": {"cloud_uid": SA_ID},
                "org": {}
            })))
            .mount(&wiki)
            .await;
        let verifier = ApiVerifier::with_base_url(wiki.uri())
            .with_iam_tokens_url(format!("{}/iam/v1/tokens", iam.uri()));
        let (dir, repo, store) = setup();
        let (content, private) = key_file();
        let key_path = dir.path().join("authorized-key.json");
        std::fs::write(&key_path, &content).unwrap();

        let login_args = LoginArgs {
            cloud_org_id: Some("cloud-org".into()),
            service_account_key: Some(key_path.clone()),
            ..args("robot")
        };
        let mut ui = ScriptedUi::default();
        login(&store, &repo, Some(&verifier), &mut ui, &login_args)
            .await
            .unwrap();
        let text = ui.text();
        assert!(text.contains("signed in to Wiki as `robot`"), "{text}");
        assert!(text.contains("robot:service-account-key"), "{text}");
        assert!(!text.contains("sa-token"), "{text}");
        assert!(!text.contains(&private[..40]), "{text}");
        let stored = store.get_service_account_key("robot").unwrap();
        assert_eq!((stored.id(), stored.service_account_id()), (KEY_ID, SA_ID));
        assert!(!store.contains("robot").unwrap());
        let saved = repo.get("robot").unwrap().unwrap();
        assert_eq!(saved.iam_source, IamSourceKind::ServiceAccountKey);
        // В профиле нет ни пути к файлу ключа, ни ключа.
        let file = std::fs::read_to_string(repo.path()).unwrap();
        assert!(!file.contains("authorized-key"), "{file}");
        assert!(!file.contains(&private[..40]), "{file}");
        assert!(
            file.contains("iam_source = \"service-account-key\""),
            "{file}"
        );

        // Повторный вход без файла берёт сохранённый ключ.
        std::fs::remove_file(&key_path).unwrap();
        let mut ui = ScriptedUi::default();
        login(&store, &repo, Some(&verifier), &mut ui, &args("robot"))
            .await
            .unwrap();
        assert!(
            !ui.text().contains("Service account key `"),
            "{}",
            ui.text()
        );

        let mut ui = ScriptedUi::default();
        status(&store, &repo, Some(&verifier), &mut ui, "robot")
            .await
            .unwrap();
        let text = ui.text();
        assert!(
            text.contains("Key:       stored in the system keychain"),
            "{text}"
        );
        assert!(
            text.contains(&format!("service account key `{KEY_ID}` of `{SA_ID}`")),
            "{text}"
        );
        assert!(text.contains("User:      robot"), "{text}");
        assert!(!text.contains("sa-token"), "{text}");

        // Испорченный файл — ошибка без содержимого, ничего не меняется.
        std::fs::write(&key_path, "{\"id\": \"secret-value\"}").unwrap();
        let err = login(&store, &repo, Some(&verifier), &mut ui, &login_args)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("no `service_account_id`"),
            "{err:#}"
        );
        assert!(!format!("{err:#}").contains("secret-value"), "{err:#}");

        let mut ui = ScriptedUi::default();
        logout(&store, &repo, &mut ui, "robot", false).unwrap();
        assert!(ui.text().contains("yc iam key delete"), "{}", ui.text());
        assert!(!store.contains_service_account_key("robot").unwrap());
        let err = login(&store, &repo, Some(&verifier), &mut ui, &args("robot"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("--service-account-key <FILE>"),
            "{err}"
        );
    }
}
