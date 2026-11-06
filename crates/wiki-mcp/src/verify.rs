//! Проверка токена и организации запросом текущего пользователя (`GET /v1/users/me`).
//!
//! Это небольшой собственный вызов `reqwest` только для `auth`: заголовки авторизации и
//! организации формируются здесь же. Когда появится общий исполнитель запросов, проверку
//! стоит перевести на него.

use std::fmt;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use reqwest::header::{AUTHORIZATION, HeaderValue, USER_AGENT};
use serde_json::Value;
use wiki_api::{ApiToken, Organization};

use crate::profiles::TokenType;

/// Адрес Wiki API.
pub const WIKI_API_URL: &str = "https://api.wiki.yandex.net";

/// Путь запроса текущего пользователя.
pub const ME_PATH: &str = "/v1/users/me";

/// Предел длины сообщения об ошибке из ответа Wiki.
const MAX_DETAILS_LEN: usize = 300;

/// Пользователь, которому принадлежит токен, и его организация по ответу Wiki. Только то,
/// что нужно показать: логин и идентификаторы организации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurrentUser {
    /// `username`, иначе `identity.uid` или `identity.cloud_uid`.
    pub login: String,
    /// `org.dir_id` (Yandex 360) и `org.collab_id` в виде текста, если они есть в ответе.
    pub org: Option<String>,
}

impl fmt::Display for CurrentUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.login)
    }
}

/// Проверка токена. В тестах подменяется, чтобы не ходить в сеть.
pub trait TokenVerifier {
    /// Текущий пользователь для токена типа `token_type` в организации `org`.
    async fn current_user(
        &self,
        token: &ApiToken,
        token_type: TokenType,
        org: &Organization,
    ) -> anyhow::Result<CurrentUser>;
}

/// Проверка через Wiki API.
#[derive(Debug, Clone)]
pub struct ApiVerifier {
    base_url: String,
}

impl Default for ApiVerifier {
    fn default() -> Self {
        Self::with_base_url(WIKI_API_URL)
    }
}

impl ApiVerifier {
    /// Проверка через API по другому адресу (тесты).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    fn url(&self) -> String {
        format!("{}{ME_PATH}", self.base_url.trim_end_matches('/'))
    }
}

/// Заголовок `Authorization`: `OAuth <токен>` или `Bearer <токен>` (IAM). Помечен как
/// чувствительный, чтобы `reqwest` не выводил его значение; ошибка не содержит токена.
fn authorization(token: &ApiToken, token_type: TokenType) -> anyhow::Result<HeaderValue> {
    let mut value =
        HeaderValue::from_str(&format!("{} {}", token_type.auth_scheme(), token.expose()))
            .map_err(|_| {
                anyhow!("the token contains characters that are not allowed in HTTP headers")
            })?;
    value.set_sensitive(true);
    Ok(value)
}

impl TokenVerifier for ApiVerifier {
    async fn current_user(
        &self,
        token: &ApiToken,
        token_type: TokenType,
        org: &Organization,
    ) -> anyhow::Result<CurrentUser> {
        // Клиент создаётся только при проверке: без корневых сертификатов системы HTTPS-клиент
        // не создаётся, а `--skip-check` должен работать и там.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("could not create the HTTPS client")?;
        let (org_header, org_id) = org.header();
        let response = client
            .get(self.url())
            .header(AUTHORIZATION, authorization(token, token_type)?)
            .header(org_header, org_id)
            .header(USER_AGENT, concat!("wiki-mcp/", env!("CARGO_PKG_VERSION")))
            .send()
            .await
            .map_err(|err| anyhow!("could not reach Wiki: {err}"))?;
        let status = response.status().as_u16();
        let body = response.bytes().await.unwrap_or_default();
        let json: Option<Value> = serde_json::from_slice(&body).ok();
        if !(200..300).contains(&status) {
            return Err(status_error(status, json.as_ref(), token, org));
        }
        match json.as_ref().and_then(current_user) {
            Some(user) => Ok(user),
            None => bail!("unexpected response from Wiki {ME_PATH} (HTTP {status})"),
        }
    }
}

/// Понятная ошибка по коду ответа. Из тела берутся только `error_code` и `debug_message`
/// Wiki, без значения токена.
fn status_error(
    status: u16,
    body: Option<&Value>,
    token: &ApiToken,
    org: &Organization,
) -> anyhow::Error {
    let details = body.and_then(error_message).map(|text| {
        let text = if token.expose().is_empty() {
            text
        } else {
            text.replace(token.expose(), "***")
        };
        let mut short: String = text.chars().take(MAX_DETAILS_LEN).collect();
        if text.chars().count() > MAX_DETAILS_LEN {
            short.push('…');
        }
        format!(" Wiki says: {short}")
    });
    let details = details.unwrap_or_default();
    let header = org.kind().header_name();
    match status {
        401 => anyhow!(
            "Wiki rejected the token (HTTP 401). Make sure it was copied completely, has the \
             right type (OAuth or IAM) and has not expired or been revoked.{details}"
        ),
        403 => anyhow!(
            "Wiki denied access (HTTP 403). Check the organization ID and type ({header}) \
             and that the token has Wiki access (wiki:read) in this organization.{details}"
        ),
        _ => anyhow!("Wiki returned HTTP {status} for {ME_PATH}.{details}"),
    }
}

/// `error_code` и `debug_message` из ответа Wiki с ошибкой.
fn error_message(body: &Value) -> Option<String> {
    let text = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    match (text("error_code"), text("debug_message")) {
        (Some(code), Some(message)) => Some(format!("{code}: {message}")),
        (Some(text), None) | (None, Some(text)) => Some(text.to_owned()),
        (None, None) => None,
    }
}

/// Логин и организация из ответа `GET /v1/users/me` (схема `MeSchema`).
fn current_user(body: &Value) -> Option<CurrentUser> {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let null = Value::Null;
    let identity = body.get("identity").unwrap_or(&null);
    let login = text(body, "username")
        .or_else(|| text(identity, "uid"))
        .or_else(|| text(identity, "cloud_uid"))?;
    let org = body.get("org").unwrap_or(&null);
    let ids: Vec<String> = ["dir_id", "collab_id"]
        .into_iter()
        .filter_map(|key| text(org, key).map(|id| format!("{key} {id}")))
        .collect();
    Some(CurrentUser {
        login,
        org: (!ids.is_empty()).then(|| ids.join(", ")),
    })
}

#[cfg(test)]
pub mod testing {
    use super::*;

    /// Проверка без сети: принимает только заданное значение токена.
    #[derive(Debug)]
    pub struct FakeVerifier {
        pub valid_token: String,
    }

    impl TokenVerifier for FakeVerifier {
        async fn current_user(
            &self,
            token: &ApiToken,
            _token_type: TokenType,
            org: &Organization,
        ) -> anyhow::Result<CurrentUser> {
            if token.expose() == self.valid_token {
                Ok(CurrentUser {
                    login: "test-user".to_owned(),
                    org: Some(format!("dir_id {}", org.id())),
                })
            } else {
                Err(status_error(401, None, token, org))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiki_api::OrgKind;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn org360() -> Organization {
        Organization::new(OrgKind::Yandex360, "12345").unwrap()
    }

    #[tokio::test]
    async fn sends_oauth_and_org_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ME_PATH))
            .and(header("authorization", "OAuth good-token"))
            .and(header("x-org-id", "12345"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "username": "jdoe",
                "home_cluster": null,
                "identity": {"uid": "1000", "cloud_uid": null},
                "org": {"dir_id": "12345", "collab_id": null}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(server.uri());
        let user = verifier
            .current_user(&ApiToken::new("good-token"), TokenType::OAuth, &org360())
            .await
            .unwrap();
        assert_eq!(user.to_string(), "jdoe");
        assert_eq!(user.org.as_deref(), Some("dir_id 12345"));
    }

    #[tokio::test]
    async fn iam_token_and_cloud_org_use_bearer_and_cloud_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ME_PATH))
            .and(header("authorization", "Bearer iam-token"))
            .and(header("x-cloud-org-id", "cloud-org"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "username": "",
                "identity": {"uid": null, "cloud_uid": "cloud-user"},
                "org": {"dir_id": null, "collab_id": "00000000-0000-0000-0000-000000000001"}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let org = Organization::new(OrgKind::Cloud, "cloud-org").unwrap();
        let user = ApiVerifier::with_base_url(format!("{}/", server.uri()))
            .current_user(&ApiToken::new("iam-token"), TokenType::Iam, &org)
            .await
            .unwrap();
        assert_eq!(user.to_string(), "cloud-user");
        assert_eq!(
            user.org.as_deref(),
            Some("collab_id 00000000-0000-0000-0000-000000000001")
        );
    }

    #[tokio::test]
    async fn maps_rejections_without_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(ME_PATH))
            .and(header("authorization", "OAuth bad-token"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "error_code": "NOT_AUTHENTICATED",
                "debug_message": "Token bad-token is invalid",
                "details": null
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(ME_PATH))
            .and(header("authorization", "OAuth other-org"))
            .respond_with(ResponseTemplate::new(403).set_body_string("not json"))
            .mount(&server)
            .await;
        let verifier = ApiVerifier::with_base_url(server.uri());

        let err = verifier
            .current_user(&ApiToken::new("bad-token"), TokenType::OAuth, &org360())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("rejected the token"), "{err}");
        assert!(
            err.contains("NOT_AUTHENTICATED: Token *** is invalid"),
            "{err}"
        );
        assert!(!err.contains("bad-token"), "{err}");

        let err = verifier
            .current_user(&ApiToken::new("other-org"), TokenType::OAuth, &org360())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(err.contains("X-Org-Id"), "{err}");
    }

    #[tokio::test]
    async fn transport_errors_and_bad_tokens() {
        // Порт 9 (discard) на localhost закрыт: соединение не устанавливается.
        let verifier = ApiVerifier::with_base_url("http://127.0.0.1:9");
        let err = verifier
            .current_user(&ApiToken::new("t"), TokenType::OAuth, &org360())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not reach Wiki"), "{err}");

        let err = verifier
            .current_user(&ApiToken::new("bad\nvalue"), TokenType::OAuth, &org360())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
        assert!(!err.to_string().contains("bad"), "{err}");
    }

    #[test]
    fn extracts_user() {
        assert_eq!(
            current_user(&json!({
                "username": "a",
                "identity": {"uid": "1"},
                "org": {"dir_id": "2", "collab_id": "3"}
            })),
            Some(CurrentUser {
                login: "a".into(),
                org: Some("dir_id 2, collab_id 3".into())
            })
        );
        assert_eq!(
            current_user(&json!({"identity": {"uid": "7"}})),
            Some(CurrentUser {
                login: "7".into(),
                org: None
            })
        );
        assert_eq!(current_user(&json!({})), None);
        assert_eq!(current_user(&json!({"identity": null, "org": null})), None);
    }

    #[test]
    fn error_messages() {
        assert_eq!(
            error_message(&json!({"error_code": "X", "debug_message": "y"})).as_deref(),
            Some("X: y")
        );
        assert_eq!(
            error_message(&json!({"debug_message": "only"})).as_deref(),
            Some("only")
        );
        assert_eq!(error_message(&json!({"error_code": ""})), None);
    }

    #[test]
    fn long_details_are_truncated() {
        let body = json!({"debug_message": "x".repeat(1000)});
        let err = status_error(500, Some(&body), &ApiToken::new("t"), &org360()).to_string();
        assert!(err.contains("HTTP 500"), "{err}");
        assert!(err.len() < 500, "{err}");
    }
}
