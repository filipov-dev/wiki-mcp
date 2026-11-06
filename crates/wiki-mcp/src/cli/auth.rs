//! `wiki-mcp auth` — токены и организации профилей.
//!
//! Токен принимается только скрытым вводом или из stdin (`--token-stdin`), хранится только в
//! системном хранилище и нигде не печатается. Команды, которая выводит значение токена, нет и
//! быть не должно.

use anyhow::Context;
use clap::{Args, Subcommand};
use wiki_api::Organization;
use wiki_api::auth::{AuthError, KEYRING_SERVICE, KeyringStore, TokenStore};

use crate::login::{check_new_token, org_from_flags, resolve_org, save_token, validate_token};
use crate::oauth::{AUTHORIZE_URL, authorize_url};
use crate::profiles::{
    DEFAULT_PROFILE, ProfileRepo, ProfileSettings, TokenType, validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};
use crate::verify::{ApiVerifier, TokenVerifier};

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Store a Wiki token (OAuth or IAM) and the organization for a profile.
    ///
    /// The token is read from a hidden prompt (or from stdin with --token-stdin), checked
    /// with GET /v1/users/me and saved in the system keychain. It is never written to
    /// profiles.toml. Replaces the stored token if there is one.
    Login(LoginArgs),
    /// Show the profile, its organization and the Wiki user of the stored token.
    ///
    /// The token value is never printed.
    Status(StatusArgs),
    /// Remove the stored token of the profile from the system keychain.
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
    /// IAM tokens expire within 12 hours, so an OAuth token is more convenient for agents.
    #[arg(long, value_enum, value_name = "TYPE")]
    pub token_type: Option<TokenType>,
    /// Read the token from stdin instead of a hidden prompt.
    ///
    /// The organization is not asked interactively in this mode: pass --org-id or
    /// --cloud-org-id unless the profile already has one.
    #[arg(long)]
    pub token_stdin: bool,
    /// Store the token without checking it against the Wiki API.
    #[arg(long)]
    pub skip_check: bool,
}

#[derive(Debug, Clone, Args)]
pub struct StatusArgs {
    /// Profile name.
    #[arg(long, default_value = DEFAULT_PROFILE)]
    pub profile: String,
    /// Do not query the Wiki API for the current user.
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
             store a new one with `wiki-mcp auth login` when it does."
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

    let from_flags = org_from_flags(args.org_id.as_deref(), args.cloud_org_id.as_deref())?;
    let cannot_ask = args.token_stdin.then(|| {
        format!(
            "profile `{profile}` has no organization: pass --org-id <ID> (Yandex 360) or \
             --cloud-org-id <ID> (Yandex Cloud) together with --token-stdin"
        )
    });
    let org = resolve_org(ui, from_flags, settings.org.clone(), cannot_ask)?;
    let token_type = args.token_type.unwrap_or(settings.token_type);

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

    settings.token_type = token_type;
    settings.org = Some(org);
    repo.upsert(profile, settings)?;
    ui.say(&format!(
        "Profile `{profile}` saved to {}.",
        repo.path().display()
    ));
    Ok(())
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
    let token = match store.get(profile) {
        Ok(token) => Some(token),
        Err(AuthError::NotFound { .. }) => None,
        Err(err) => return Err(err.into()),
    };
    let settings = repo.get(profile)?;
    let org = settings.as_ref().and_then(|settings| settings.org.clone());
    let token_type = settings
        .as_ref()
        .map_or_else(TokenType::default, |settings| settings.token_type);

    ui.say(&format!("Profile:   {profile}"));
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
    ui.say(&format!(
        "Org:       {}",
        org.as_ref()
            .map_or_else(|| "not set".to_owned(), Organization::to_string)
    ));
    // Пользователь и организация — по ответу Wiki на запрос `/v1/users/me`.
    let (user, wiki_org) = match (&token, &org, verifier) {
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
    };
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

pub fn logout(
    store: &dyn TokenStore,
    repo: &ProfileRepo,
    ui: &mut dyn Ui,
    profile: &str,
    forget: bool,
) -> anyhow::Result<()> {
    validate_profile_name(profile)?;
    match store.delete(profile) {
        Ok(()) => ui.say(&format!(
            "Token of profile `{profile}` removed from the system keychain. If it is no longer \
             needed anywhere, also revoke it in your Yandex ID account."
        )),
        Err(AuthError::NotFound { .. }) => {
            ui.say(&format!("No token stored for profile `{profile}`."));
        }
        Err(err) => return Err(err.into()),
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
}
