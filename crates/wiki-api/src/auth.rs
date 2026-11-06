//! Хранение токенов Wiki в системном хранилище.
//!
//! Каждый профиль (например, `claude-code` или `codex`) хранит свой токен отдельной записью:
//! сервис [`KEYRING_SERVICE`], имя учётной записи — имя профиля. Так токены разных агентов
//! можно отзывать и заменять независимо.
//!
//! Запасного файлового хранилища нет: если системное хранилище недоступно (Linux без
//! Secret Service), операции завершаются ошибкой [`AuthError::Unavailable`] с подсказкой.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::token::ApiToken;

/// Имя сервиса записей в Keychain / Credential Manager / Secret Service.
pub const KEYRING_SERVICE: &str = "dev.filipov.wiki-mcp";

/// Системное хранилище на этой платформе — Secret Service (D-Bus). На Linux и BSD keyring
/// работает только через него, и «хранилище недоступно» там почти всегда означает, что в
/// сессии нет демона Secret Service.
const USES_SECRET_SERVICE: bool = cfg!(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
));

/// Подсказка для Linux без Secret Service.
const SECRET_SERVICE_HINT: &str = "no Secret Service (org.freedesktop.secrets) is available \
on the D-Bus session bus. wiki-mcp stores tokens only in the system keychain. Run it in a \
desktop session with a keyring daemon (GNOME Keyring, KDE Wallet, KeePassXC with Secret \
Service integration). On a headless machine start a session bus and an unlocked keyring \
first, for example: dbus-run-session -- sh, then inside it \
gnome-keyring-daemon --unlock --components=secrets (reads the keyring password from stdin)";

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(
        "no Wiki token is stored for profile `{profile}`; store one with `wiki-mcp auth login --profile {profile}`"
    )]
    NotFound { profile: String },
    #[error("the system keychain is not available: {0}")]
    Unavailable(String),
    #[error("system keychain error: {0}")]
    Backend(String),
}

/// Хранилище токенов по профилям.
pub trait TokenStore: Send + Sync {
    fn get(&self, profile: &str) -> Result<ApiToken, AuthError>;
    fn set(&self, profile: &str, token: &ApiToken) -> Result<(), AuthError>;
    fn delete(&self, profile: &str) -> Result<(), AuthError>;

    /// Есть ли токен у профиля. Значение токена наружу не возвращается.
    fn contains(&self, profile: &str) -> Result<bool, AuthError> {
        match self.get(profile) {
            Ok(_) => Ok(true),
            Err(AuthError::NotFound { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// Хранилище на базе системного keyring (Keychain на macOS, Credential Manager на Windows,
/// Secret Service на Linux).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(profile: &str) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(KEYRING_SERVICE, profile).map_err(|err| map_error(profile, err))
    }
}

/// Переводит ошибку keyring в [`AuthError`]. Значение токена в текст ошибки не попадает.
fn map_error(profile: &str, err: keyring::Error) -> AuthError {
    map_error_for(profile, err, USES_SECRET_SERVICE)
}

/// [`map_error`] с явной платформой: так подсказку для Secret Service можно проверить в
/// тестах на любой ОС.
fn map_error_for(profile: &str, err: keyring::Error, secret_service: bool) -> AuthError {
    match err {
        keyring::Error::NoEntry => AuthError::NotFound {
            profile: profile.to_owned(),
        },
        // `BadEncoding` содержит байты сохранённого значения — их не выводим.
        keyring::Error::BadEncoding(_) => {
            AuthError::Backend("the stored token is not valid UTF-8".into())
        }
        // Нет сессионной шины D-Bus, нет демона Secret Service или хранилище заблокировано.
        keyring::Error::NoStorageAccess(details) | keyring::Error::PlatformFailure(details)
            if secret_service =>
        {
            AuthError::Unavailable(format!("{SECRET_SERVICE_HINT}. Details: {details}"))
        }
        keyring::Error::NoStorageAccess(details) => AuthError::Unavailable(details.to_string()),
        other => AuthError::Backend(other.to_string()),
    }
}

impl TokenStore for KeyringStore {
    fn get(&self, profile: &str) -> Result<ApiToken, AuthError> {
        let password = Self::entry(profile)?
            .get_password()
            .map_err(|err| map_error(profile, err))?;
        Ok(ApiToken::new(password))
    }

    fn set(&self, profile: &str, token: &ApiToken) -> Result<(), AuthError> {
        Self::entry(profile)?
            .set_password(token.expose())
            .map_err(|err| map_error(profile, err))
    }

    fn delete(&self, profile: &str) -> Result<(), AuthError> {
        Self::entry(profile)?
            .delete_credential()
            .map_err(|err| map_error(profile, err))
    }
}

/// Хранилище в памяти для тестов: системный keychain в автотестах не используется.
#[derive(Debug, Default)]
pub struct MemoryStore {
    tokens: Mutex<HashMap<String, ApiToken>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, ApiToken>>, AuthError> {
        self.tokens
            .lock()
            .map_err(|_| AuthError::Backend("the in-memory token store is poisoned".into()))
    }
}

impl TokenStore for MemoryStore {
    fn get(&self, profile: &str) -> Result<ApiToken, AuthError> {
        self.lock()?
            .get(profile)
            .cloned()
            .ok_or_else(|| AuthError::NotFound {
                profile: profile.to_owned(),
            })
    }

    fn set(&self, profile: &str, token: &ApiToken) -> Result<(), AuthError> {
        self.lock()?.insert(profile.to_owned(), token.clone());
        Ok(())
    }

    fn delete(&self, profile: &str) -> Result<(), AuthError> {
        self.lock()?
            .remove(profile)
            .map(|_| ())
            .ok_or_else(|| AuthError::NotFound {
                profile: profile.to_owned(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::new();
        assert!(!store.contains("codex").unwrap());
        assert!(matches!(
            store.get("codex"),
            Err(AuthError::NotFound { profile }) if profile == "codex"
        ));

        store.set("codex", &ApiToken::new("t1")).unwrap();
        store.set("claude-code", &ApiToken::new("t2")).unwrap();
        assert_eq!(store.get("codex").unwrap().expose(), "t1");
        assert!(store.contains("claude-code").unwrap());

        store.set("codex", &ApiToken::new("t3")).unwrap();
        assert_eq!(store.get("codex").unwrap().expose(), "t3");

        store.delete("codex").unwrap();
        assert!(!store.contains("codex").unwrap());
        assert!(matches!(
            store.delete("codex"),
            Err(AuthError::NotFound { .. })
        ));
        assert_eq!(store.get("claude-code").unwrap().expose(), "t2");
    }

    #[test]
    fn keyring_errors_are_mapped_without_values() {
        assert!(matches!(
            map_error("p", keyring::Error::NoEntry),
            AuthError::NotFound { profile } if profile == "p"
        ));
        let err = map_error("p", keyring::Error::BadEncoding(b"secret-bytes".to_vec()));
        assert!(!err.to_string().contains("secret"));
        let err = AuthError::NotFound {
            profile: "ci".into(),
        };
        assert!(err.to_string().contains("auth login --profile ci"), "{err}");
    }

    #[test]
    fn missing_secret_service_gets_a_hint() {
        let no_bus = || keyring::Error::PlatformFailure("no session bus".into());
        let err = map_error_for("p", no_bus(), true);
        assert!(matches!(err, AuthError::Unavailable(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("Secret Service"), "{text}");
        assert!(text.contains("dbus-run-session"), "{text}");
        assert!(text.contains("no session bus"), "{text}");

        let err = map_error_for("p", keyring::Error::NoStorageAccess("locked".into()), true);
        assert!(err.to_string().contains("Secret Service"), "{err}");

        // На macOS и Windows подсказки про D-Bus нет.
        let err = map_error_for("p", no_bus(), false);
        assert!(matches!(err, AuthError::Backend(_)), "{err:?}");
        assert!(!err.to_string().contains("Secret Service"), "{err}");
        let err = map_error_for("p", keyring::Error::NoStorageAccess("denied".into()), false);
        assert!(matches!(err, AuthError::Unavailable(_)), "{err:?}");
    }

    /// Проверка с настоящим системным хранилищем: запускать вручную (`cargo test -- --ignored`).
    #[test]
    #[ignore = "uses the real system keychain"]
    fn keyring_store_round_trip() {
        let store = KeyringStore;
        let profile = "wiki-mcp-test-profile";
        let _ = store.delete(profile);
        assert!(!store.contains(profile).unwrap());
        store.set(profile, &ApiToken::new("test-value")).unwrap();
        assert_eq!(store.get(profile).unwrap().expose(), "test-value");
        store.delete(profile).unwrap();
        assert!(!store.contains(profile).unwrap());
    }
}
