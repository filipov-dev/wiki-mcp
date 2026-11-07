//! Авторизованный ключ сервисного аккаунта Yandex Cloud и обмен его на IAM-токен.
//!
//! Ключ создаётся командой `yc iam key create --service-account-name <имя> --output
//! key.json`: это JSON с полями `id` (ID ключа), `service_account_id` и `private_key`
//! (закрытый ключ RSA в PEM, PKCS#8). Сервер подписывает ключом JWT (PS256) и обменивает
//! его на IAM-токен запросом `POST https://iam.api.cloud.yandex.net/iam/v1/tokens`
//! ([`KeyExchange`]).
//!
//! Правила:
//! - закрытый ключ хранится только в системном хранилище; запись — компактная двоичная
//!   ([`ServiceAccountKey::to_secret`]), чтобы ключ RSA-4096 помещался в Credential Manager
//!   Windows (не больше 2560 байт);
//! - `Debug` не показывает закрытый ключ; ключ, JWT и токен не попадают в логи и тексты
//!   ошибок; из ответа IAM API с ошибкой берётся только короткое сообщение с
//!   замаскированными длинными значениями;
//! - редиректы при обмене не выполняются: JWT уходит только на адрес IAM API.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{RSA_PSS_SHA256, RsaKeyPair};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};

use crate::iam::{IamError, IssuedToken, summarize_stderr, token_from_json};

/// Адрес обмена JWT на IAM-токен. Он же — `aud` в JWT.
pub const IAM_TOKENS_URL: &str = "https://iam.api.cloud.yandex.net/iam/v1/tokens";

/// Предел размера файла ключа: настоящий ключ — около 2–4 КиБ.
pub const MAX_KEY_FILE_BYTES: usize = 64 * 1024;

/// Срок JWT: IAM API принимает JWT не дольше часа, нужен он только на один обмен.
const JWT_LIFETIME: Duration = Duration::from_secs(10 * 60);

/// Запас на расхождение часов: `iat` чуть в прошлом, иначе IAM API может счесть JWT
/// выпущенным в будущем.
const CLOCK_SKEW: Duration = Duration::from_secs(30);

/// Таймаут обмена по умолчанию.
pub const DEFAULT_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);

/// Предел тела ответа IAM API, байт.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// Предел длины ID ключа и сервисного аккаунта.
const MAX_ID_LEN: usize = 64;

/// Начало записи ключа в системном хранилище: формат и его версия.
const SECRET_MAGIC: &[u8] = b"wiki-mcp-sa-key/1\n";

const PEM_BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
const PEM_END: &str = "-----END PRIVATE KEY-----";

/// Авторизованный ключ сервисного аккаунта. `Debug` не показывает закрытый ключ.
#[derive(Clone)]
pub struct ServiceAccountKey {
    id: String,
    service_account_id: String,
    /// Закрытый ключ в DER (PKCS#8).
    pkcs8: Vec<u8>,
    key_pair: Arc<RsaKeyPair>,
}

impl PartialEq for ServiceAccountKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.service_account_id == other.service_account_id
            && self.pkcs8 == other.pkcs8
    }
}

impl Eq for ServiceAccountKey {}

impl fmt::Debug for ServiceAccountKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccountKey")
            .field("id", &self.id)
            .field("service_account_id", &self.service_account_id)
            .field("private_key", &"***")
            .finish()
    }
}

impl ServiceAccountKey {
    /// Ключ из ID ключа, ID сервисного аккаунта и закрытого ключа RSA в DER (PKCS#8).
    pub fn new(
        id: impl Into<String>,
        service_account_id: impl Into<String>,
        pkcs8: Vec<u8>,
    ) -> Result<Self, IamError> {
        let id = id.into();
        let service_account_id = service_account_id.into();
        validate_id("id", &id)?;
        validate_id("service_account_id", &service_account_id)?;
        let key_pair = RsaKeyPair::from_pkcs8(&pkcs8).map_err(|_| {
            IamError::InvalidKey("`private_key` is not an RSA private key in PKCS#8".to_owned())
        })?;
        Ok(Self {
            id,
            service_account_id,
            pkcs8,
            key_pair: Arc::new(key_pair),
        })
    }

    /// Ключ из файла `yc iam key create --output key.json`. Текст ошибки не содержит
    /// содержимого файла.
    pub fn from_json(bytes: &[u8]) -> Result<Self, IamError> {
        if bytes.len() > MAX_KEY_FILE_BYTES {
            return Err(IamError::InvalidKey(format!(
                "the key file is larger than {MAX_KEY_FILE_BYTES} bytes"
            )));
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| {
            IamError::InvalidKey(
                "the key file is not JSON; create it with `yc iam key create \
                 --service-account-name <NAME> --output key.json`"
                    .to_owned(),
            )
        })?;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .ok_or_else(|| {
                    IamError::InvalidKey(format!(
                        "the key file has no `{name}`; it must be an authorized key of a \
                         service account (`yc iam key create`)"
                    ))
                })
        };
        let id = field("id")?;
        let service_account_id = field("service_account_id")?;
        let pkcs8 = pem_to_der(field("private_key")?)?;
        Self::new(id, service_account_id, pkcs8)
    }

    /// ID ключа (`kid` в JWT).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// ID сервисного аккаунта (`iss` в JWT).
    pub fn service_account_id(&self) -> &str {
        &self.service_account_id
    }

    /// Запись для системного хранилища: метка формата, ID ключа, ID сервисного аккаунта и
    /// закрытый ключ в DER. Использовать только для записи в хранилище.
    pub fn to_secret(&self) -> Vec<u8> {
        let mut secret = Vec::with_capacity(
            SECRET_MAGIC.len()
                + self.id.len()
                + self.service_account_id.len()
                + 2
                + self.pkcs8.len(),
        );
        secret.extend_from_slice(SECRET_MAGIC);
        secret.extend_from_slice(self.id.as_bytes());
        secret.push(b'\n');
        secret.extend_from_slice(self.service_account_id.as_bytes());
        secret.push(b'\n');
        secret.extend_from_slice(&self.pkcs8);
        secret
    }

    /// Ключ из записи [`Self::to_secret`].
    pub fn from_secret(secret: &[u8]) -> Result<Self, IamError> {
        let broken = || IamError::InvalidKey("the stored key record is damaged".to_owned());
        let rest = secret.strip_prefix(SECRET_MAGIC).ok_or_else(broken)?;
        let (id, rest) = split_line(rest).ok_or_else(broken)?;
        let (service_account_id, pkcs8) = split_line(rest).ok_or_else(broken)?;
        Self::new(id, service_account_id, pkcs8.to_vec())
    }

    /// JWT для обмена на IAM-токен, подписанный ключом (PS256).
    pub fn jwt(&self, now: SystemTime) -> Result<String, IamError> {
        let issued_at = now
            .checked_sub(CLOCK_SKEW)
            .unwrap_or(now)
            .duration_since(UNIX_EPOCH)
            .map_err(|_| IamError::InvalidKey("the system clock is before 1970".to_owned()))?
            .as_secs();
        let expires = issued_at + (JWT_LIFETIME + CLOCK_SKEW).as_secs();
        let header = json!({"alg": "PS256", "typ": "JWT", "kid": self.id});
        let claims = json!({
            "iss": self.service_account_id,
            "aud": IAM_TOKENS_URL,
            "iat": issued_at,
            "exp": expires,
        });
        let mut jwt = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.key_pair.public_modulus_len()];
        self.key_pair
            .sign(
                &RSA_PSS_SHA256,
                &SystemRandom::new(),
                jwt.as_bytes(),
                &mut signature,
            )
            .map_err(|_| IamError::InvalidKey("could not sign the JWT".to_owned()))?;
        jwt.push('.');
        jwt.push_str(&URL_SAFE_NO_PAD.encode(signature));
        Ok(jwt)
    }
}

/// Строка до `\n` и остаток.
fn split_line(bytes: &[u8]) -> Option<(String, &[u8])> {
    let end = bytes.iter().position(|b| *b == b'\n')?;
    let line = std::str::from_utf8(&bytes[..end]).ok()?.to_owned();
    Some((line, &bytes[end + 1..]))
}

/// ID ключа и сервисного аккаунта: латинские буквы и цифры (`aje...`).
fn validate_id(name: &str, id: &str) -> Result<(), IamError> {
    if id.is_empty() || id.len() > MAX_ID_LEN || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(IamError::InvalidKey(format!(
            "`{name}` must be 1-{MAX_ID_LEN} Latin letters and digits"
        )));
    }
    Ok(())
}

/// DER закрытого ключа из PEM. `yc` пишет перед PEM строку-предупреждение, она пропускается.
fn pem_to_der(pem: &str) -> Result<Vec<u8>, IamError> {
    if pem.contains("BEGIN RSA PRIVATE KEY") {
        return Err(IamError::InvalidKey(
            "`private_key` is in PKCS#1 format; use the key file created by `yc iam key create` \
             (PKCS#8)"
                .to_owned(),
        ));
    }
    let body = pem
        .split_once(PEM_BEGIN)
        .and_then(|(_, rest)| rest.split_once(PEM_END))
        .map(|(body, _)| body)
        .ok_or_else(|| {
            IamError::InvalidKey("`private_key` has no PEM private key block".to_owned())
        })?;
    let compact: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    STANDARD
        .decode(compact)
        .map_err(|_| IamError::InvalidKey("`private_key` is not valid base64 PEM".to_owned()))
}

/// Обмен JWT, подписанного ключом сервисного аккаунта, на IAM-токен.
#[derive(Debug, Clone)]
pub struct KeyExchange {
    key: ServiceAccountKey,
    endpoint: url::Url,
    timeout: Duration,
}

impl KeyExchange {
    /// Обмен в IAM API Yandex Cloud ([`IAM_TOKENS_URL`]).
    pub fn new(key: ServiceAccountKey) -> Result<Self, IamError> {
        let endpoint = url::Url::parse(IAM_TOKENS_URL)
            .map_err(|err| IamError::InvalidConfig(format!("IAM API address: {err}")))?;
        Ok(Self {
            key,
            endpoint,
            timeout: DEFAULT_EXCHANGE_TIMEOUT,
        })
    }

    /// Обмен по другому адресу: `https://`, а `http://` — только для loopback (тесты).
    pub fn with_endpoint(mut self, endpoint: url::Url) -> Result<Self, IamError> {
        let loopback = match endpoint.host() {
            Some(url::Host::Domain(name)) => name == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        };
        let allowed = match endpoint.scheme() {
            "https" => endpoint.host().is_some(),
            "http" => loopback,
            _ => false,
        };
        if !allowed || !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(IamError::InvalidConfig(
                "the IAM API address must be https:// (http:// only for loopback)".to_owned(),
            ));
        }
        self.endpoint = endpoint;
        Ok(self)
    }

    /// Тот же обмен с другим таймаутом.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn key(&self) -> &ServiceAccountKey {
        &self.key
    }

    /// Источник для пользователя: ID ключа и сервисного аккаунта (они не секретные).
    pub fn display(&self) -> String {
        format!(
            "service account key `{}` of `{}`",
            self.key.id, self.key.service_account_id
        )
    }

    /// Подписывает JWT и обменивает его на IAM-токен.
    pub async fn run(&self, now: SystemTime) -> Result<IssuedToken, IamError> {
        let failed = |details: String| IamError::ExchangeFailed {
            key: self.display(),
            details,
        };
        let jwt = self.key.jwt(now)?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("wiki-mcp/", env!("CARGO_PKG_VERSION")))
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| failed("could not create the HTTPS client".to_owned()))?;
        tracing::debug!(key = %self.key.id, "обмен JWT сервисного аккаунта на IAM-токен");
        let mut response = client
            .post(self.endpoint.clone())
            .json(&json!({ "jwt": jwt }))
            .send()
            .await
            .map_err(|err| failed(describe_transport_error(&err)))?;
        let status = response.status();
        let body = read_limited(&mut response)
            .await
            .map_err(|err| failed(describe_transport_error(&err)))?;
        let value = serde_json::from_slice::<Value>(&body).ok();
        if !status.is_success() {
            let message = value
                .as_ref()
                .and_then(|value| value.get("message"))
                .and_then(Value::as_str)
                .map(|text| summarize_stderr(text.as_bytes()))
                .filter(|text| !text.is_empty())
                .map(|text| format!(": {text}"))
                .unwrap_or_default();
            return Err(IamError::ExchangeRejected {
                key: self.display(),
                status: status.as_u16(),
                details: message,
            });
        }
        let value = value.ok_or_else(|| failed("the response is not JSON".to_owned()))?;
        token_from_json(&value).map_err(failed)
    }
}

/// Тело ответа не больше [`MAX_RESPONSE_BYTES`]; больше — обрезается (ответ с токеном —
/// сотни байт).
async fn read_limited(response: &mut reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = MAX_RESPONSE_BYTES.saturating_sub(body.len());
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() >= MAX_RESPONSE_BYTES {
            break;
        }
    }
    Ok(body)
}

/// Причина ошибки соединения без адреса и тела запроса.
fn describe_transport_error(err: &reqwest::Error) -> String {
    if err.is_timeout() {
        "the request timed out".to_owned()
    } else if err.is_connect() {
        "could not connect".to_owned()
    } else {
        "the request failed".to_owned()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::signature::{self, KeyPair, UnparsedPublicKey};

    use super::*;

    /// Новый ключ RSA-2048 для тестов: закрытые ключи в репозиторий не кладутся.
    pub(crate) fn test_key_pkcs8() -> Vec<u8> {
        let pair = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        pair.as_der().unwrap().as_ref().to_vec()
    }

    pub(crate) fn key_file(pkcs8: &[u8]) -> String {
        let body = STANDARD.encode(pkcs8);
        let lines: Vec<&str> = body
            .as_bytes()
            .chunks(64)
            .map(|chunk| std::str::from_utf8(chunk).unwrap())
            .collect();
        let pem = format!(
            "PLEASE DO NOT REMOVE THIS LINE! Yandex.Cloud SA Key ID <ajetestkey1>\n{PEM_BEGIN}\n{}\n{PEM_END}\n",
            lines.join("\n")
        );
        json!({
            "id": "ajetestkey1",
            "service_account_id": "ajetestsa1",
            "created_at": "2026-01-01T00:00:00Z",
            "key_algorithm": "RSA_2048",
            "public_key": "-----BEGIN PUBLIC KEY-----\n...\n-----END PUBLIC KEY-----\n",
            "private_key": pem,
        })
        .to_string()
    }

    fn decode_part(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn parses_key_file_and_round_trips_secret() {
        let pkcs8 = test_key_pkcs8();
        let key = ServiceAccountKey::from_json(key_file(&pkcs8).as_bytes()).unwrap();
        assert_eq!(key.id(), "ajetestkey1");
        assert_eq!(key.service_account_id(), "ajetestsa1");
        assert_eq!(key.pkcs8, pkcs8);

        let secret = key.to_secret();
        // Запись помещается в Credential Manager Windows и для ключа RSA-4096 (около 2,4 КиБ).
        assert!(secret.len() < 1500, "{}", secret.len());
        assert_eq!(ServiceAccountKey::from_secret(&secret).unwrap(), key);
        assert!(ServiceAccountKey::from_secret(b"other").is_err());
        assert!(ServiceAccountKey::from_secret(&secret[..40]).is_err());

        let debug = format!("{key:?}");
        assert!(debug.contains("ajetestkey1"), "{debug}");
        assert!(debug.contains("private_key: \"***\""), "{debug}");
        assert!(!debug.contains(&STANDARD.encode(&pkcs8[..32])), "{debug}");
    }

    #[test]
    fn rejects_bad_key_files_without_content() {
        let pkcs8 = test_key_pkcs8();
        let good: Value = serde_json::from_str(&key_file(&pkcs8)).unwrap();
        let with = |name: &str, value: Value| {
            let mut file = good.clone();
            file[name] = value;
            file.to_string()
        };
        let cases = [
            ("not json secret-text".to_owned(), "not JSON"),
            (with("id", Value::Null), "no `id`"),
            (
                with("service_account_id", json!("")),
                "no `service_account_id`",
            ),
            (with("id", json!("a/b")), "Latin letters"),
            (with("private_key", json!("secret-text")), "no PEM"),
            (
                with(
                    "private_key",
                    json!("-----BEGIN RSA PRIVATE KEY-----\nsecret-text\n"),
                ),
                "PKCS#1",
            ),
            (
                with(
                    "private_key",
                    json!(format!("{PEM_BEGIN}\nsecret-text!\n{PEM_END}")),
                ),
                "base64",
            ),
            (
                with(
                    "private_key",
                    json!(format!("{PEM_BEGIN}\nc2VjcmV0LXRleHQ=\n{PEM_END}")),
                ),
                "PKCS#8",
            ),
        ];
        for (file, expected) in cases {
            let err = ServiceAccountKey::from_json(file.as_bytes()).unwrap_err();
            let text = err.to_string();
            assert!(text.contains(expected), "{expected}: {text}");
            assert!(!text.contains("secret-text"), "{text}");
        }
        let large = vec![b' '; MAX_KEY_FILE_BYTES + 1];
        assert!(
            ServiceAccountKey::from_json(&large)
                .unwrap_err()
                .to_string()
                .contains("larger")
        );
    }

    #[test]
    fn jwt_is_signed_with_ps256() {
        let pkcs8 = test_key_pkcs8();
        let key = ServiceAccountKey::from_json(key_file(&pkcs8).as_bytes()).unwrap();
        let now = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        let jwt = key.jwt(now).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            decode_part(parts[0]),
            json!({"alg": "PS256", "typ": "JWT", "kid": "ajetestkey1"})
        );
        let claims = decode_part(parts[1]);
        assert_eq!(claims["iss"], "ajetestsa1");
        assert_eq!(claims["aud"], IAM_TOKENS_URL);
        assert_eq!(claims["iat"], 1_767_225_600 - 30);
        assert_eq!(claims["exp"], 1_767_225_600 + 600);

        let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        let public = UnparsedPublicKey::new(
            &signature::RSA_PSS_2048_8192_SHA256,
            key.key_pair.public_key().as_ref(),
        );
        let signed = format!("{}.{}", parts[0], parts[1]);
        public.verify(signed.as_bytes(), &signature).unwrap();
        assert!(public.verify(b"other", &signature).is_err());
    }

    #[test]
    fn endpoint_must_be_https_or_loopback() {
        let key = ServiceAccountKey::from_json(key_file(&test_key_pkcs8()).as_bytes()).unwrap();
        let exchange = KeyExchange::new(key).unwrap();
        assert_eq!(exchange.endpoint.as_str(), IAM_TOKENS_URL);
        assert_eq!(
            exchange.display(),
            "service account key `ajetestkey1` of `ajetestsa1`"
        );
        for good in [
            "https://iam.example.test/t",
            "http://127.0.0.1:1/t",
            "http://localhost/t",
        ] {
            exchange
                .clone()
                .with_endpoint(good.parse().unwrap())
                .unwrap();
        }
        for bad in [
            "http://iam.example.test/t",
            "ftp://127.0.0.1/t",
            "https://user:pw@iam.example.test/t",
        ] {
            assert!(
                exchange
                    .clone()
                    .with_endpoint(bad.parse().unwrap())
                    .is_err(),
                "{bad}"
            );
        }
    }
}
