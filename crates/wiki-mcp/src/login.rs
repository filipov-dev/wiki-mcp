//! Общие шаги `auth login` и `connect`: организация, проверка формы токена, проверка
//! запросом `/v1/users/me` и сохранение в системное хранилище; для IAM-токенов из `yc` или
//! по ключу сервисного аккаунта — организация Yandex Cloud, настройки `yc`, чтение файла
//! ключа и проверка, что источник выдаёт токен, который принимает Wiki.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, anyhow, bail};
use wiki_api::auth::{AuthError, KEYRING_SERVICE, TokenStore, service_account_key_account};
use wiki_api::iam::{IamSource, YcCommand};
use wiki_api::service_account::{MAX_KEY_FILE_BYTES, ServiceAccountKey};
use wiki_api::{ApiToken, OrgKind, Organization};

use crate::profiles::{ProfileSettings, TokenType};
use crate::ui::Ui;
use crate::verify::TokenVerifier;

/// Предел длины токена: OAuth-токен — несколько десятков символов, IAM-токен — сотни.
const MAX_TOKEN_LEN: usize = 4096;

/// Организация из флагов `--org-id` / `--cloud-org-id`.
pub fn org_from_flags(
    org_id: Option<&str>,
    cloud_org_id: Option<&str>,
) -> anyhow::Result<Option<Organization>> {
    let (kind, id) = match (org_id, cloud_org_id) {
        (Some(_), Some(_)) => bail!("--org-id and --cloud-org-id cannot be used together"),
        (Some(id), None) => (OrgKind::Yandex360, id),
        (None, Some(id)) => (OrgKind::Cloud, id),
        (None, None) => return Ok(None),
    };
    Ok(Some(Organization::new(kind, id)?))
}

/// Спрашивает тип и ID организации.
pub fn ask_org(ui: &mut dyn Ui) -> anyhow::Result<Organization> {
    ui.say("Wiki needs the organization of every request:");
    ui.say("  1) Yandex 360 for Business (sent as X-Org-Id)");
    ui.say("  2) Yandex Cloud Organization (sent as X-Cloud-Org-Id)");
    ui.say(
        "The ID is shown in the Yandex 360 admin panel (Yandex 360) or in the Yandex Cloud \
         Organization console (Yandex Cloud).",
    );
    let answer = ui
        .ask("Organization type [1/2]: ")
        .context("failed to read the organization type")?;
    let kind = match answer.to_ascii_lowercase().as_str() {
        "1" | "360" | "yandex360" => OrgKind::Yandex360,
        "2" | "cloud" => OrgKind::Cloud,
        _ => {
            bail!("unknown organization type `{answer}`: answer 1 (Yandex 360) or 2 (Yandex Cloud)")
        }
    };
    let id = ui
        .ask("Organization ID: ")
        .context("failed to read the organization ID")?;
    Ok(Organization::new(kind, id)?)
}

/// Организация профиля: из флагов, иначе из профиля, иначе спрашивается у пользователя.
///
/// `cannot_ask` — текст ошибки, если спрашивать нельзя (ввод не интерактивный).
pub fn resolve_org(
    ui: &mut dyn Ui,
    from_flags: Option<Organization>,
    stored: Option<Organization>,
    cannot_ask: Option<String>,
) -> anyhow::Result<Organization> {
    match (from_flags, stored) {
        (Some(org), _) => Ok(org),
        (None, Some(org)) => {
            ui.say(&format!("Organization: {org} (from the profile)."));
            Ok(org)
        }
        (None, None) => match cannot_ask {
            Some(message) => bail!(message),
            None => ask_org(ui),
        },
    }
}

/// Проверяет форму токена до сохранения. Значение токена в ошибку не попадает.
pub fn validate_token(token: &ApiToken) -> anyhow::Result<()> {
    let value = token.expose();
    if value.is_empty() {
        bail!("empty token; nothing was stored");
    }
    if value.len() > MAX_TOKEN_LEN || !value.chars().all(|c| c.is_ascii_graphic()) {
        bail!(
            "this does not look like a Wiki token: expected a single line of printable \
             ASCII characters without spaces; nothing was stored"
        );
    }
    Ok(())
}

/// Проверяет новый токен запросом `/v1/users/me`; без проверяющего (`--skip-check`) только
/// сообщает об этом. Ошибка проверки означает, что ничего не сохранено.
pub async fn check_new_token<V: TokenVerifier>(
    ui: &mut dyn Ui,
    verifier: Option<&V>,
    token: &ApiToken,
    token_type: TokenType,
    org: &Organization,
) -> anyhow::Result<()> {
    match verifier {
        Some(verifier) => {
            let user = verifier
                .current_user(token, token_type, org)
                .await
                .map_err(|err| {
                    anyhow!(
                        "{err:#} Nothing was stored. Re-run with --skip-check to store the token \
                         without verification."
                    )
                })?;
            ui.say(&format!(
                "Token is valid: signed in to Wiki as `{user}` in {org}."
            ));
        }
        None => ui.say("Skipping the token check (--skip-check)."),
    }
    Ok(())
}

/// Сохраняет токен профиля в системное хранилище, заменяя прежний.
pub fn save_token(
    ui: &mut dyn Ui,
    store: &dyn TokenStore,
    profile: &str,
    token: &ApiToken,
) -> anyhow::Result<()> {
    let replaced = store.contains(profile)?;
    store.set(profile, token)?;
    ui.say(&format!(
        "Token {} in the system keychain (service `{KEYRING_SERVICE}`, account `{profile}`).",
        if replaced { "replaced" } else { "saved" }
    ));
    Ok(())
}

/// Таймаут `yc` в интерактивных командах (`auth login`, `connect`): для федеративного
/// аккаунта `yc` может открыть браузер и ждать входа через SSO.
pub const INTERACTIVE_YC_TIMEOUT: Duration = Duration::from_secs(180);

/// Организация профиля с IAM-токенами из `yc` или по ключу: только Yandex Cloud. Из
/// `--cloud-org-id`, иначе из профиля, иначе спрашивается ID (тип не спрашивается: он может
/// быть только один). `cannot_ask` — текст ошибки, если спрашивать нельзя.
pub fn resolve_iam_org(
    ui: &mut dyn Ui,
    profile: &str,
    org_id: Option<&str>,
    cloud_org_id: Option<&str>,
    stored: Option<Organization>,
) -> anyhow::Result<Organization> {
    if org_id.is_some() {
        bail!(
            "Yandex Cloud IAM tokens work only with a Yandex Cloud organization: use \
             --cloud-org-id instead of --org-id"
        );
    }
    if let Some(id) = cloud_org_id {
        return Ok(Organization::new(OrgKind::Cloud, id)?);
    }
    match stored {
        Some(org) if org.kind() == OrgKind::Cloud => {
            ui.say(&format!("Organization: {org} (from the profile)."));
            Ok(org)
        }
        Some(org) => bail!(
            "profile `{profile}` has the organization {org}, but Yandex Cloud IAM tokens work \
             only with a Yandex Cloud organization: pass --cloud-org-id <ID>"
        ),
        None => {
            ui.say("IAM tokens work with a Yandex Cloud Organization (sent as X-Cloud-Org-Id).");
            ui.say("The ID is shown in the Yandex Cloud Organization console.");
            let id = ui
                .ask("Yandex Cloud Organization ID: ")
                .context("failed to read the organization ID")?;
            Ok(Organization::new(OrgKind::Cloud, id)?)
        }
    }
}

/// Применяет `--yc-path` и `--yc-profile` к настройкам профиля (флаги важнее сохранённого)
/// и возвращает команду `yc`. Ошибка в настройках — до запуска `yc`.
pub fn apply_yc_flags(
    settings: &mut ProfileSettings,
    yc_path: Option<&Path>,
    yc_profile: Option<&str>,
) -> anyhow::Result<YcCommand> {
    if let Some(path) = yc_path {
        settings.yc_path = Some(path.to_owned());
    }
    if let Some(name) = yc_profile {
        settings.yc_profile = Some(name.trim().to_owned());
    }
    settings.yc_command()
}

/// Читает файл авторизованного ключа сервисного аккаунта. Путь нигде не сохраняется, а
/// содержимое файла не попадает в тексты ошибок.
pub fn read_service_account_key(path: &Path) -> anyhow::Result<ServiceAccountKey> {
    let shown = path.display();
    let metadata =
        std::fs::metadata(path).with_context(|| format!("cannot read the key file {shown}"))?;
    if !metadata.is_file() {
        bail!("the key file {shown} is not a regular file");
    }
    if metadata.len() > MAX_KEY_FILE_BYTES as u64 {
        bail!("the key file {shown} is larger than {MAX_KEY_FILE_BYTES} bytes");
    }
    let bytes = std::fs::read(path).with_context(|| format!("cannot read the key file {shown}"))?;
    ServiceAccountKey::from_json(&bytes).with_context(|| format!("key file {shown}"))
}

/// Ключ сервисного аккаунта профиля: из файла `--service-account-key`, иначе сохранённый.
/// Возвращает ключ и признак, что он прочитан из файла (его нужно сохранить).
pub fn service_account_key(
    store: &dyn TokenStore,
    profile: &str,
    file: Option<&Path>,
) -> anyhow::Result<(ServiceAccountKey, bool)> {
    if let Some(file) = file {
        return Ok((read_service_account_key(file)?, true));
    }
    match store.get_service_account_key(profile) {
        Ok(key) => Ok((key, false)),
        Err(AuthError::KeyNotFound { .. }) => bail!(
            "profile `{profile}` has no stored service account key: pass \
             --service-account-key <FILE> with the key file from `yc iam key create`"
        ),
        Err(err) => Err(err.into()),
    }
}

/// Сохраняет ключ сервисного аккаунта профиля в системное хранилище, заменяя прежний.
pub fn save_service_account_key(
    ui: &mut dyn Ui,
    store: &dyn TokenStore,
    profile: &str,
    key: &ServiceAccountKey,
) -> anyhow::Result<()> {
    let replaced = store.contains_service_account_key(profile)?;
    store.set_service_account_key(profile, key)?;
    ui.say(&format!(
        "Service account key `{}` {} in the system keychain (service `{KEYRING_SERVICE}`, \
         account `{}`). wiki-mcp no longer needs the key file: keep it safe or delete it.",
        key.id(),
        if replaced { "replaced" } else { "saved" },
        service_account_key_account(profile)
    ));
    Ok(())
}

/// Проверяет источник IAM-токенов: он выдаёт токен, и Wiki принимает его
/// (`GET /v1/users/me`). Токен нигде не сохраняется и не выводится; без проверяющего
/// (`--skip-check`) источник не спрашивается. Ошибка означает, что ничего не сохранено.
pub async fn check_iam<V: TokenVerifier>(
    ui: &mut dyn Ui,
    verifier: Option<&V>,
    source: &IamSource,
    org: &Organization,
) -> anyhow::Result<()> {
    let Some(verifier) = verifier else {
        ui.say("Skipping the IAM token check (--skip-check).");
        return Ok(());
    };
    ui.say(&format!(
        "Getting an IAM token from {} (it is not shown or stored)...",
        source.display()
    ));
    let source = match source {
        IamSource::Yc(command) => {
            IamSource::Yc(command.clone().with_timeout(INTERACTIVE_YC_TIMEOUT))
        }
        IamSource::ServiceAccountKey(exchange) => {
            IamSource::ServiceAccountKey(verifier.key_exchange(exchange.clone())?)
        }
    };
    let nothing_saved = "Nothing was saved. Re-run with --skip-check to save the profile \
                         without the check.";
    let issued = source
        .issue(SystemTime::now())
        .await
        .map_err(|err| anyhow!("{err}. {nothing_saved}"))?;
    let user = verifier
        .current_user(&issued.token, TokenType::Iam, org)
        .await
        .map_err(|err| anyhow!("{err:#} {nothing_saved}"))?;
    ui.say(&format!(
        "IAM token works: signed in to Wiki as `{user}` in {org}. wiki-mcp does not store it \
         and gets a new one when needed."
    ));
    Ok(())
}

#[cfg(all(test, unix))]
pub mod testing {
    use std::path::PathBuf;

    /// Подменный `yc` во временном каталоге: печатает `iam-token-<номер вызова>`, считает
    /// вызовы и запоминает аргументы; после [`FakeYc::fail`] падает с «секретом» в stderr.
    /// Настоящий `yc` в тестах не запускается.
    pub struct FakeYc {
        dir: tempfile::TempDir,
    }

    /// Пишет исполняемый скрипт через отдельный процесс `sh`: если тест держит
    /// открытый на запись дескриптор, параллельный тест может унаследовать его
    /// при fork, и запуск скрипта падает с «Text file busy» (ETXTBSY).
    fn write_script(path: &std::path::Path, content: &str) {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("/bin/sh")
            .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(content.as_bytes()).unwrap();
        drop(stdin);
        assert!(child.wait().unwrap().success());
    }

    impl FakeYc {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("yc");
            write_script(
                &script,
                "#!/bin/sh\ndir=$(dirname \"$0\")\nn=$(cat \"$dir/count\" 2>/dev/null || echo 0)\n\
                 n=$((n + 1))\necho \"$n\" > \"$dir/count\"\necho \"$@\" > \"$dir/args\"\n\
                 if [ -f \"$dir/fail\" ]; then echo 'denied t1.LeakedSecretValue0123456' >&2; exit 1; fi\n\
                 echo \"iam-token-$n\"\n",
            );
            Self { dir }
        }

        pub fn path(&self) -> PathBuf {
            self.dir.path().join("yc")
        }

        pub fn calls(&self) -> u32 {
            std::fs::read_to_string(self.dir.path().join("count"))
                .map(|text| text.trim().parse().unwrap())
                .unwrap_or(0)
        }

        pub fn args(&self) -> String {
            std::fs::read_to_string(self.dir.path().join("args")).unwrap_or_default()
        }

        pub fn fail(&self) {
            std::fs::write(self.dir.path().join("fail"), "").unwrap();
        }
    }
}

#[cfg(test)]
pub mod key_testing {
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::KeySize;
    use aws_lc_rs::signature::RsaKeyPair;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    /// ID ключа и сервисного аккаунта тестового ключа.
    pub const KEY_ID: &str = "ajekeytest01";
    pub const SA_ID: &str = "ajesatest01";

    /// Файл авторизованного ключа с новым ключом RSA-2048: закрытые ключи в репозиторий не
    /// кладутся. Возвращает содержимое файла и base64 закрытого ключа (для поиска утечек).
    pub fn key_file() -> (String, String) {
        let pair = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
        let body = STANDARD.encode(pair.as_der().unwrap().as_ref());
        let pem = format!("-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----\n");
        let file = serde_json::json!({
            "id": KEY_ID,
            "service_account_id": SA_ID,
            "private_key": pem,
        });
        (file.to_string(), body)
    }
}
