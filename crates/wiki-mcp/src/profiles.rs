//! Профили: настройки в `~/.config/wiki-mcp/profiles.toml`, токен — в системном хранилище
//! под именем профиля. Профиль с IAM-токенами может вместо сохранённого токена получать их
//! сам: через `yc iam create-token` или по ключу сервисного аккаунта (ключ — в системном
//! хранилище отдельной записью); такие токены держатся только в памяти процесса.
//!
//! В файле профилей нет токенов, ключей и других секретов: только тип токена и источник
//! IAM-токенов, организация, `ClientID` OAuth-приложения пользователя (он не секретный), путь
//! к `yc` и имя его профиля, настройки доступа по умолчанию. Путь к файлу ключа сервисного
//! аккаунта не сохраняется.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use wiki_api::auth::{AuthError, KeyringStore, TokenStore};
use wiki_api::iam::{IamSource, KeyExchange, YcCommand};
use wiki_api::{ApiToken, OrgKind, Organization};

/// Переменная окружения, которая переопределяет каталог конфигурации (нужна для тестов).
pub const CONFIG_DIR_ENV: &str = "WIKI_MCP_CONFIG_DIR";

/// Имя файла профилей в каталоге конфигурации.
pub const PROFILES_FILE: &str = "profiles.toml";

/// Профиль по умолчанию.
pub const DEFAULT_PROFILE: &str = "default";

/// Максимальная длина имени профиля.
const MAX_PROFILE_LEN: usize = 64;

/// Тип токена профиля: определяет схему заголовка `Authorization`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum TokenType {
    /// OAuth-токен Яндекс OAuth: `Authorization: OAuth <токен>`.
    #[default]
    #[serde(rename = "oauth")]
    #[value(
        name = "oauth",
        help = "OAuth token, sent as Authorization: OAuth <token>"
    )]
    OAuth,
    /// IAM-токен Yandex Cloud: `Authorization: Bearer <токен>`. Живёт не больше 12 часов.
    #[serde(rename = "iam")]
    #[value(
        name = "iam",
        help = "Yandex Cloud IAM token, sent as Authorization: Bearer <token>"
    )]
    Iam,
}

impl TokenType {
    /// Схема заголовка `Authorization` для токена этого типа.
    pub const fn auth_scheme(self) -> &'static str {
        match self {
            Self::OAuth => "OAuth",
            Self::Iam => "Bearer",
        }
    }

    /// Название типа для пользователя.
    pub const fn label(self) -> &'static str {
        match self {
            Self::OAuth => "OAuth token",
            Self::Iam => "Yandex Cloud IAM token",
        }
    }
}

/// Откуда профиль с IAM-токенами (`token_type = "iam"`) берёт токены.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
pub enum IamSourceKind {
    /// Токен, сохранённый в системном хранилище (`auth login --token-type iam`). Живёт не
    /// больше 12 часов, потом его нужно сохранить заново.
    #[default]
    #[serde(rename = "token")]
    #[value(
        name = "token",
        help = "an IAM token you paste; it expires within 12 hours"
    )]
    Token,
    /// Новые токены от `yc iam create-token`, только в памяти процесса.
    #[serde(rename = "yc")]
    #[value(
        name = "yc",
        help = "new tokens from `yc iam create-token` when needed (federated accounts, yc profiles)"
    )]
    Yc,
    /// Новые токены по ключу сервисного аккаунта из системного хранилища, только в памяти.
    #[serde(rename = "service-account-key")]
    #[value(
        name = "service-account-key",
        help = "new tokens for a service account key stored in the system keychain"
    )]
    ServiceAccountKey,
}

impl IamSourceKind {
    pub fn is_token(&self) -> bool {
        *self == Self::Token
    }

    /// Токены получаются заново по сроку, а не хранятся.
    pub fn refreshes(self) -> bool {
        !self.is_token()
    }
}

/// Настройки профиля. Токена здесь нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// MCP-клиент, в котором `connect` зарегистрировал профиль (`claude-code`, `codex`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// `ClientID` OAuth-приложения пользователя, через которое получен токен.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Тип токена: `oauth` (по умолчанию) или `iam`.
    pub token_type: TokenType,
    /// IAM: откуда брать токены. `token` (сохранённый токен, по умолчанию) не записывается:
    /// файлы прежних версий читаются и пишутся без изменений.
    #[serde(skip_serializing_if = "IamSourceKind::is_token")]
    pub iam_source: IamSourceKind,
    /// IAM через `yc`: абсолютный путь к `yc`; без него `yc` ищется в `PATH`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yc_path: Option<PathBuf>,
    /// IAM через `yc`: имя профиля `yc` (`yc --profile`); без него — активный профиль `yc`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yc_profile: Option<String>,
    /// Разрешены операции записи (`serve --allow-write`).
    pub allow_write: bool,
    /// Разрешены удаляющие операции (`serve --allow-destructive`).
    pub allow_destructive: bool,
    /// Включённые наборы инструментов (`serve --toolsets`); пустой список — набор по умолчанию.
    pub toolsets: Vec<String>,
    /// Организация Wiki: тип определяет заголовок (`X-Org-Id` или `X-Cloud-Org-Id`).
    /// Таблица `[profiles.<имя>.org]` с полями `type` и `id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<Organization>,
}

impl ProfileSettings {
    /// Действующее разрешение на запись: удаление подразумевает запись.
    pub fn writes_allowed(&self) -> bool {
        self.allow_write || self.allow_destructive
    }

    /// Приводит флаги доступа к действующему уровню: `allow_destructive` включает `allow_write`.
    pub fn normalized(mut self) -> Self {
        self.allow_write = self.writes_allowed();
        self
    }

    /// Источник IAM-токенов, который получает токены сам (`yc` или ключ), если он задан.
    /// Сохранённый токен (OAuth или IAM) — `None`.
    pub fn refreshing_source(&self) -> Option<IamSourceKind> {
        (self.token_type == TokenType::Iam && self.iam_source.refreshes())
            .then_some(self.iam_source)
    }

    /// Команда `yc` для IAM-токена по настройкам профиля.
    pub fn yc_command(&self) -> anyhow::Result<YcCommand> {
        Ok(YcCommand::new(
            self.yc_path.clone(),
            self.yc_profile.clone(),
        )?)
    }

    /// Переводит профиль на сохранённый токен типа `token_type`: настройки источника
    /// IAM-токенов больше не нужны.
    pub fn use_stored_token(&mut self, token_type: TokenType) {
        self.token_type = token_type;
        self.iam_source = IamSourceKind::Token;
        self.yc_path = None;
        self.yc_profile = None;
    }

    /// Переводит профиль на IAM-токены из источника `source`; настройки `yc` остаются
    /// только у `yc`.
    pub fn use_iam_source(&mut self, source: IamSourceKind) {
        self.token_type = TokenType::Iam;
        self.iam_source = source;
        if source != IamSourceKind::Yc {
            self.yc_path = None;
            self.yc_profile = None;
        }
    }
}

/// IAM-токены `yc` и ключа сервисного аккаунта выдаются для организации Yandex Cloud
/// (`X-Cloud-Org-Id`).
pub fn check_iam_org(profile: &str, org: &Organization) -> anyhow::Result<()> {
    if org.kind() != OrgKind::Cloud {
        bail!(
            "profile `{profile}` gets Yandex Cloud IAM tokens, which work only with a Yandex \
             Cloud organization, but its organization is {org}; set it with \
             `wiki-mcp auth login --profile {profile} --cloud-org-id <ID>`"
        );
    }
    Ok(())
}

/// Содержимое `profiles.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProfilesDocument {
    profiles: BTreeMap<String, ProfileSettings>,
}

/// Файл профилей.
#[derive(Debug, Clone)]
pub struct ProfileRepo {
    path: PathBuf,
}

impl ProfileRepo {
    /// Файл профилей в каталоге `dir`.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(PROFILES_FILE),
        }
    }

    /// Файл профилей в каталоге конфигурации пользователя.
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::in_dir(config_dir()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> anyhow::Result<ProfilesDocument> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfilesDocument::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        toml::from_str(&text).with_context(|| format!("failed to parse {}", self.path.display()))
    }

    fn save(&self, document: &ProfilesDocument) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(document).context("failed to serialize profiles")?;
        fs::write(&self.path, text)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    /// Настройки профиля, если он есть в файле.
    pub fn get(&self, name: &str) -> anyhow::Result<Option<ProfileSettings>> {
        Ok(self.load()?.profiles.remove(name))
    }

    /// Создаёт или заменяет профиль. Флаги доступа сохраняются нормализованными
    /// (см. [`ProfileSettings::normalized`]).
    pub fn upsert(&self, name: &str, settings: ProfileSettings) -> anyhow::Result<()> {
        validate_profile_name(name)?;
        let mut document = self.load()?;
        document
            .profiles
            .insert(name.to_owned(), settings.normalized());
        self.save(&document)
    }

    /// Все профили файла в порядке имён.
    pub fn list(&self) -> anyhow::Result<BTreeMap<String, ProfileSettings>> {
        Ok(self.load()?.profiles)
    }

    /// Удаляет профиль. Возвращает `true`, если профиль был.
    pub fn remove(&self, name: &str) -> anyhow::Result<bool> {
        let mut document = self.load()?;
        let removed = document.profiles.remove(name).is_some();
        if removed {
            self.save(&document)?;
        }
        Ok(removed)
    }
}

/// Каталог конфигурации: `WIKI_MCP_CONFIG_DIR`, иначе `~/.config/wiki-mcp`.
///
/// На macOS тоже используется `~/.config`, а не `~/Library/Application Support`: так путь
/// одинаков на macOS и Linux. На Linux учитывается `XDG_CONFIG_HOME`.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join(".config"))
    } else {
        dirs::config_dir()
    };
    base.map(|base| base.join("wiki-mcp"))
        .context("cannot determine the user configuration directory")
}

/// Проверяет имя профиля. Имя попадает в аргументы команд MCP-клиентов, ключи TOML и имя
/// записи в keychain, поэтому допускаются только `[A-Za-z0-9._-]` и имя не начинается с `-`.
pub fn validate_profile_name(name: &str) -> anyhow::Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if name.is_empty() || name.len() > MAX_PROFILE_LEN || !valid_chars || name.starts_with('-') {
        bail!(
            "invalid profile name `{name}`: use 1-{MAX_PROFILE_LEN} characters from A-Z, a-z, 0-9, `.`, `_`, `-`, not starting with `-`"
        );
    }
    Ok(())
}

/// Откуда профиль берёт токен.
#[derive(Debug, Clone)]
pub enum ProfileAuth {
    /// Токен из системного хранилища; тип — в `settings.token_type`.
    Stored(ApiToken),
    /// IAM-токены из источника (`yc` или ключ сервисного аккаунта): получаются при запросах
    /// и держатся в памяти.
    Iam(IamSource),
}

/// Профиль, готовый к работе: настройки, организация и источник токена.
#[derive(Debug, Clone)]
pub struct Profile {
    pub settings: ProfileSettings,
    pub org: Organization,
    pub auth: ProfileAuth,
}

/// Загружает профиль: настройки из `profiles.toml` и токен из системного хранилища (или
/// источник IAM-токенов: команду `yc`, ключ сервисного аккаунта из хранилища).
///
/// Отсутствие токена или организации — понятная пользователю ошибка с подсказкой, как их
/// сохранить.
// `serve` загружает профиль через `load_profile_with` с явным хранилищем.
#[allow(dead_code)]
pub fn load_profile(name: &str) -> anyhow::Result<Profile> {
    load_profile_with(&ProfileRepo::open_default()?, &KeyringStore, name)
}

/// То же, что [`load_profile`], с явными файлом профилей и хранилищем токенов.
pub fn load_profile_with(
    repo: &ProfileRepo,
    store: &dyn TokenStore,
    name: &str,
) -> anyhow::Result<Profile> {
    validate_profile_name(name)?;
    let settings = repo.get(name)?.unwrap_or_default();
    if settings.token_type != TokenType::Iam && settings.iam_source.refreshes() {
        bail!(
            "profile `{name}` in {} has `iam_source` but its `token_type` is not `iam`; fix it \
             with `wiki-mcp auth login --profile {name}`",
            repo.path().display()
        );
    }
    if let Some(source) = settings.refreshing_source() {
        let Some(org) = settings.org.clone() else {
            bail!(
                "profile `{name}` has no Wiki organization; set it with \
                 `wiki-mcp auth login --profile {name} --cloud-org-id <ID>`"
            );
        };
        check_iam_org(name, &org)?;
        let source = match source {
            IamSourceKind::Yc => IamSource::Yc(
                settings
                    .yc_command()
                    .with_context(|| format!("profile `{name}` has invalid yc settings"))?,
            ),
            _ => {
                let key = match store.get_service_account_key(name) {
                    Ok(key) => key,
                    Err(err @ AuthError::KeyNotFound { .. }) => return Err(err.into()),
                    Err(err) => {
                        return Err(anyhow::Error::new(err).context(format!(
                            "cannot load the service account key of profile `{name}`"
                        )));
                    }
                };
                IamSource::ServiceAccountKey(KeyExchange::new(key)?)
            }
        };
        return Ok(Profile {
            settings,
            org,
            auth: ProfileAuth::Iam(source),
        });
    }
    let token = match store.get(name) {
        Ok(token) => token,
        Err(err @ AuthError::NotFound { .. }) => return Err(err.into()),
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("cannot load the token of profile `{name}`")));
        }
    };
    let Some(org) = settings.org.clone() else {
        bail!(
            "profile `{name}` has no Wiki organization; set it with \
             `wiki-mcp auth login --profile {name} --org-id <ID>` (Yandex 360) or \
             `--cloud-org-id <ID>` (Yandex Cloud)"
        );
    };
    Ok(Profile {
        settings,
        org,
        auth: ProfileAuth::Stored(token),
    })
}

#[cfg(test)]
mod tests {
    use wiki_api::OrgKind;
    use wiki_api::auth::MemoryStore;

    use super::*;

    fn sample() -> ProfileSettings {
        ProfileSettings {
            client: Some("codex".into()),
            client_id: Some("0123456789abcdef".into()),
            token_type: TokenType::OAuth,
            iam_source: IamSourceKind::Token,
            yc_path: None,
            yc_profile: None,
            allow_write: true,
            allow_destructive: false,
            toolsets: vec!["pages".into(), "attachments".into()],
            org: Some(Organization::new(OrgKind::Cloud, "test-org").unwrap()),
        }
    }

    #[test]
    fn round_trip_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("nested"));
        assert_eq!(repo.get("codex").unwrap(), None);

        repo.upsert("codex", sample()).unwrap();
        let iam = ProfileSettings {
            token_type: TokenType::Iam,
            ..ProfileSettings::default()
        };
        repo.upsert("ci", iam.clone()).unwrap();
        repo.upsert(DEFAULT_PROFILE, ProfileSettings::default())
            .unwrap();
        assert_eq!(repo.get("codex").unwrap(), Some(sample()));
        assert_eq!(repo.get("ci").unwrap(), Some(iam));
        assert_eq!(
            repo.get(DEFAULT_PROFILE).unwrap(),
            Some(ProfileSettings::default())
        );

        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("[profiles.codex]"), "{text}");
        assert!(text.contains("allow_write = true"), "{text}");
        assert!(text.contains("token_type = \"oauth\""), "{text}");
        assert!(text.contains("token_type = \"iam\""), "{text}");
        assert!(text.contains("[profiles.codex.org]"), "{text}");
        assert!(text.contains("type = \"cloud\""), "{text}");
        // Пустые необязательные поля и сохранённый IAM-токен по умолчанию не записываются.
        assert_eq!(text.matches("client_id").count(), 1, "{text}");
        assert!(!text.contains("iam_source"), "{text}");
        assert!(!text.contains("yc_"), "{text}");

        assert!(repo.remove("codex").unwrap());
        assert!(!repo.remove("codex").unwrap());
        assert_eq!(repo.get("codex").unwrap(), None);
        assert!(repo.get(DEFAULT_PROFILE).unwrap().is_some());
    }

    #[test]
    fn parses_hand_written_file() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(
            repo.path(),
            "# comment\n[profiles.ci]\nallow_write = true\n\n[profiles.ci.org]\ntype = \"yandex360\"\nid = \"42\"\n\n[profiles.empty]\n\n[profiles.cloud]\ntoken_type = \"iam\"\n",
        )
        .unwrap();
        let ci = repo.get("ci").unwrap().unwrap();
        assert!(ci.allow_write);
        assert!(!ci.allow_destructive);
        assert!(ci.toolsets.is_empty());
        assert_eq!(ci.client_id, None);
        // Без `token_type` — OAuth-токен.
        assert_eq!(ci.token_type, TokenType::OAuth);
        let org = ci.org.unwrap();
        assert_eq!(org.header(), ("X-Org-Id", "42"));
        assert_eq!(repo.get("empty").unwrap(), Some(ProfileSettings::default()));
        let cloud = repo.get("cloud").unwrap().unwrap();
        assert_eq!(cloud.token_type, TokenType::Iam);
        assert_eq!(cloud.token_type.auth_scheme(), "Bearer");
    }

    #[test]
    fn destructive_implies_write() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        // Файл, написанный вручную: удаление без записи.
        fs::write(repo.path(), "[profiles.old]\nallow_destructive = true\n").unwrap();
        let old = repo.get("old").unwrap().unwrap();
        assert!(!old.allow_write);
        assert!(old.writes_allowed());

        let settings = ProfileSettings {
            allow_destructive: true,
            ..ProfileSettings::default()
        };
        repo.upsert("new", settings).unwrap();
        let saved = repo.get("new").unwrap().unwrap();
        assert!(saved.allow_write);
        assert!(saved.allow_destructive);
    }

    #[test]
    fn rejects_unknown_fields_and_bad_values() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        for text in [
            "[profiles.ci]\ntoken = \"x\"\n",
            "[profiles.ci]\ntoken_type = \"basic\"\n",
            "[profiles.ci.org]\ntype = \"yandex360\"\nid = \"a b\"\n",
            "[profiles.ci.org]\ntype = \"other\"\nid = \"1\"\n",
        ] {
            fs::write(repo.path(), text).unwrap();
            assert!(repo.get("ci").is_err(), "{text}");
        }
    }

    #[test]
    fn validates_profile_names() {
        for good in ["default", "claude-code", "ci_1", "a.b"] {
            validate_profile_name(good).unwrap();
        }
        for bad in ["", "-x", "a b", "a/b", "проф", &"x".repeat(65)] {
            assert!(validate_profile_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn load_profile_requires_token_and_org() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        repo.upsert("codex", sample()).unwrap();

        let err = load_profile_with(&repo, &store, "codex").unwrap_err();
        assert!(
            err.to_string().contains("auth login --profile codex"),
            "{err}"
        );

        store.set("codex", &ApiToken::new("t")).unwrap();
        let profile = load_profile_with(&repo, &store, "codex").unwrap();
        assert_eq!(profile.settings, sample());
        assert_eq!(profile.org.header(), ("X-Cloud-Org-Id", "test-org"));
        assert!(matches!(&profile.auth, ProfileAuth::Stored(token) if token.expose() == "t"));

        // Токен без организации — ошибка с подсказкой.
        store.set("bare", &ApiToken::new("t2")).unwrap();
        let err = load_profile_with(&repo, &store, "bare").unwrap_err();
        assert!(err.to_string().contains("--cloud-org-id"), "{err}");
        assert!(!err.to_string().contains("t2"));
    }

    /// Файл профилей прежней версии читается и записывается без изменений: рабочие профили
    /// не нужно подключать заново.
    #[test]
    fn existing_profiles_file_is_kept_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let old = "[profiles.codex]\nclient = \"codex\"\nclient_id = \"0123456789abcdef0123456789abcdef\"\ntoken_type = \"oauth\"\nallow_write = true\nallow_destructive = true\ntoolsets = []\n\n[profiles.codex.org]\ntype = \"cloud\"\nid = \"test-org\"\n";
        fs::write(repo.path(), old).unwrap();
        let codex = repo.get("codex").unwrap().unwrap();
        assert_eq!(codex.iam_source, IamSourceKind::Token);
        assert_eq!(codex.refreshing_source(), None);
        repo.upsert("codex", codex).unwrap();
        assert_eq!(fs::read_to_string(repo.path()).unwrap(), old);

        let store = MemoryStore::new();
        store.set("codex", &ApiToken::new("t")).unwrap();
        let profile = load_profile_with(&repo, &store, "codex").unwrap();
        assert!(matches!(profile.auth, ProfileAuth::Stored(_)));
    }

    #[test]
    fn iam_sources_need_cloud_org_and_no_stored_token() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        let mut yc = ProfileSettings {
            yc_profile: Some("work".into()),
            yc_path: Some(dir.path().join("yc")),
            org: Some(Organization::new(OrgKind::Cloud, "cloud-org").unwrap()),
            ..ProfileSettings::default()
        };
        yc.use_iam_source(IamSourceKind::Yc);
        repo.upsert("fed", yc.clone()).unwrap();
        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("token_type = \"iam\""), "{text}");
        assert!(text.contains("iam_source = \"yc\""), "{text}");
        assert!(text.contains("yc_profile = \"work\""), "{text}");
        assert_eq!(repo.get("fed").unwrap(), Some(yc.clone()));

        // Токен в хранилище не нужен и не читается.
        let profile = load_profile_with(&repo, &store, "fed").unwrap();
        let ProfileAuth::Iam(IamSource::Yc(command)) = &profile.auth else {
            panic!("ожидался yc: {:?}", profile.auth);
        };
        assert_eq!(command.profile(), Some("work"));
        assert_eq!(command.program(), dir.path().join("yc"));

        // Организация Yandex 360 с IAM из источника — ошибка.
        let wrong = ProfileSettings {
            org: Some(Organization::new(OrgKind::Yandex360, "1").unwrap()),
            ..yc.clone()
        };
        repo.upsert("fed", wrong).unwrap();
        let err = load_profile_with(&repo, &store, "fed").unwrap_err();
        assert!(err.to_string().contains("--cloud-org-id"), "{err}");

        let relative = ProfileSettings {
            yc_path: Some(PathBuf::from("relative/yc")),
            ..yc.clone()
        };
        repo.upsert("fed", relative).unwrap();
        let err = load_profile_with(&repo, &store, "fed").unwrap_err();
        assert!(format!("{err:#}").contains("absolute"), "{err:#}");

        // Ключ сервисного аккаунта — из хранилища; без ключа — подсказка.
        let mut sa = yc.clone();
        sa.use_iam_source(IamSourceKind::ServiceAccountKey);
        assert_eq!(sa.yc_profile, None);
        repo.upsert("robot", sa).unwrap();
        let err = load_profile_with(&repo, &store, "robot").unwrap_err();
        assert!(err.to_string().contains("--service-account-key"), "{err}");

        // `iam_source` без `token_type = "iam"` — ошибка.
        fs::write(repo.path(), "[profiles.x]\niam_source = \"yc\"\n").unwrap();
        let err = load_profile_with(&repo, &store, "x").unwrap_err();
        assert!(err.to_string().contains("token_type"), "{err}");
        fs::write(repo.path(), "[profiles.x]\niam_source = \"password\"\n").unwrap();
        assert!(repo.get("x").is_err());

        let mut back = yc;
        back.use_stored_token(TokenType::OAuth);
        assert_eq!(back.iam_source, IamSourceKind::Token);
        assert_eq!(back.yc_path, None);
    }
}
