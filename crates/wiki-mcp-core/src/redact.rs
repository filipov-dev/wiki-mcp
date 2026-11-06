//! Вырезание значений, похожих на учётные данные, из ответов API.
//!
//! В Wiki учётные данные попадают в ответы в основном через содержимое: токен или пароль
//! могут вставить в текст страницы, комментарий, ячейку таблицы или в поле с говорящим именем
//! (например, в таблице со столбцом `password`). Агенту эти значения не нужны, а в его
//! контексте они опасны: оттуда они легко попадают в логи, коммиты и документацию. Поэтому во
//! всех ответах — успешных и ошибках, JSON и текстовых — значения заменяются на [`REDACTED`],
//! а к результату добавляется пометка ([`Redactions::note`]).
//!
//! Правила:
//!
//! 1. **Поля по имени** ([`is_credential_key`]): строковое значение поля, имя которого похоже
//!    на пароль, токен, ключ или заголовок авторизации ([`CREDENTIAL_KEYS`]), на любой
//!    глубине. Исключения: идентификаторы постраничной выдачи (`next_cursor`, `page_token`),
//!    `recovery_token` удалённой страницы, признаки (`has_password`, `is_secret`) и пустые строки.
//! 2. **Значения по форме** ([`redact_secret_values`]) в любой строке: OAuth-токены Яндекса
//!    (`y0_…`, `AQAAAA…`), IAM-токены (`t1.…`), JWT, значения `OAuth …`, `Bearer …` и
//!    `Basic …`, блоки закрытых ключей PEM и пароли в URL (`https://user:пароль@host`).
//!    Заменяется только сам секрет, остальной текст остаётся. Значения полей постраничной
//!    выдачи (`cursor`, `next_cursor`, `prev_cursor`) не проверяются: они нужны агенту как есть.
//!
//! Для текстовых тел (не JSON) правило 1 применяется к парам `"ключ": "строка"` в тексте,
//! в том числе к незакрытой строке в конце (обрезанный JSON), правило 2 — ко всему тексту.
//!
//! В отличие от Tracker, в Wiki нет настроек интеграций с заголовками и способами
//! авторизации, поэтому правила для `authContext` и `headers` сюда не перенесены, а функции
//! не зависят от пути запроса. Они не зависят и от `OperationDef`, поэтому работают и для
//! сгенерированных инструментов, и для `api_request`.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;
use wiki_api::executor::{ApiError, ApiResponse, ResponseBody};

/// Чем заменяются скрытые значения.
pub const REDACTED: &str = "[redacted]";

/// Имена полей с учётными данными. Совпадение — имя целиком или суффикс после `_`
/// (`db_password`, `oauth_token`, `x_api_key`) без учёта регистра; camelCase, `-`, `.` и
/// пробелы приводятся к `_`.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "token",
    "access_token",
    "refresh_token",
    "private_key",
    "privatekey",
    "api_key",
    "apikey",
    "client_secret",
    "credentials",
    "authorization",
    "cookie",
];

/// Суффиксы имён, которые совпадают с [`CREDENTIAL_KEYS`], но не являются учётными данными:
/// идентификаторы постраничной выдачи нужны агенту для следующей страницы, а
/// `recovery_token` из ответа `delete_page` — UUID удалённой страницы для `recover_page`.
const NOT_CREDENTIAL_SUFFIXES: &[&str] = &["page_token", "next_token", "recovery_token"];

/// Поля постраничной выдачи: их значения не проверяются и по форме (правило 2).
const PAGINATION_SUFFIXES: &[&str] = &["cursor", "page_token", "next_token"];

/// Префиксы флагов (`has_password`, `is_secret`): это признаки, а не значения.
const FLAG_PREFIXES: &[&str] = &["has_", "is_"];

/// Что было скрыто в одном ответе.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactions {
    /// Имена полей, значения которых скрыты целиком (правило 1).
    pub credential_fields: BTreeSet<String>,
    /// Сколько значений, похожих на секреты по форме, скрыто внутри строк (правило 2).
    pub secret_values: usize,
}

impl Redactions {
    pub fn is_empty(&self) -> bool {
        self.credential_fields.is_empty() && self.secret_values == 0
    }

    /// Добавляет то, что скрыто в другом ответе (составные инструменты делают несколько
    /// запросов, а пометка нужна одна).
    pub fn merge(&mut self, other: Redactions) {
        self.credential_fields.extend(other.credential_fields);
        self.secret_values += other.secret_values;
    }

    /// Пояснение для агента; `None`, если ничего не скрыто.
    pub fn note(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.credential_fields.is_empty() {
            let fields: Vec<&str> = self.credential_fields.iter().map(String::as_str).collect();
            parts.push(format!(
                "Values of fields that look like credentials were replaced with \"{REDACTED}\": \
                 {}.",
                fields.join(", ")
            ));
        }
        if self.secret_values > 0 {
            parts.push(format!(
                "{} value(s) that look like tokens, private keys or passwords (OAuth or IAM \
                 tokens, Authorization values, passwords in URLs) were replaced with \
                 \"{REDACTED}\" inside the text.",
                self.secret_values
            ));
        }
        Some(format!(
            "[{} The server never shows them; do not ask the user for them, and do not try to \
             recover them in other ways.]",
            parts.join(" ")
        ))
    }
}

/// Похоже ли имя поля на поле с учётными данными (см. [`CREDENTIAL_KEYS`]).
pub fn is_credential_key(key: &str) -> bool {
    let key = normalize_key(key);
    if FLAG_PREFIXES.iter().any(|prefix| key.starts_with(prefix))
        || NOT_CREDENTIAL_SUFFIXES
            .iter()
            .any(|suffix| key.ends_with(suffix))
    {
        return false;
    }
    CREDENTIAL_KEYS.iter().any(|candidate| {
        key == *candidate
            || key
                .strip_suffix(candidate)
                .is_some_and(|rest| rest.ends_with('_'))
    })
}

fn is_pagination_key(key: &str) -> bool {
    let key = normalize_key(key);
    PAGINATION_SUFFIXES.iter().any(|suffix| {
        key == *suffix
            || key
                .strip_suffix(suffix)
                .is_some_and(|rest| rest.ends_with('_'))
    })
}

/// `accessToken`, `Access-Token` → `access_token`.
fn normalize_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous: Option<char> = None;
    for c in key.trim().chars() {
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            normalized.push('_');
        }
        if matches!(c, '-' | ' ' | '.') {
            normalized.push('_');
        } else {
            normalized.extend(c.to_lowercase());
        }
        previous = Some(c);
    }
    normalized
}

/// Вырезает значения из JSON-ответа.
pub fn redact_json(json: &mut Value) -> Redactions {
    let mut redactions = Redactions::default();
    walk(json, &mut redactions);
    redactions
}

fn walk(value: &mut Value, redactions: &mut Redactions) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                if let Value::String(text) = item {
                    redact_field(key, text, redactions);
                } else {
                    walk(item, redactions);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, redactions);
            }
        }
        Value::String(text) => {
            redactions.secret_values += redact_secret_values_in_place(text);
        }
        _ => {}
    }
}

/// Строковое значение поля `key`.
fn redact_field(key: &str, text: &mut String, redactions: &mut Redactions) {
    if text.is_empty() {
        return;
    }
    if is_credential_key(key) {
        if text != REDACTED {
            *text = REDACTED.to_owned();
            redactions.credential_fields.insert(key.to_owned());
        }
        return;
    }
    if !is_pagination_key(key) {
        redactions.secret_values += redact_secret_values_in_place(text);
    }
}

/// Значения по форме (правило 2): регулярные выражения и проверка найденного значения.
struct SecretPattern {
    regex: Regex,
    /// Группа с секретом; остальной текст совпадения сохраняется.
    group: usize,
    /// Дополнительная проверка секрета: отсекает похожие обычные слова.
    accept: fn(&str) -> bool,
}

fn any_value(_: &str) -> bool {
    true
}

/// Значение после `OAuth`/`Bearer`: токены содержат цифры, обычные слова — нет.
fn has_digit(value: &str) -> bool {
    value.chars().any(|c| c.is_ascii_digit())
}

/// Значение после `Basic`: base64 (длина кратна 4) с цифрой или выравниванием `=`.
fn looks_like_base64(value: &str) -> bool {
    value.len().is_multiple_of(4) && (value.ends_with('=') || has_digit(value))
}

static SECRET_PATTERNS: LazyLock<Vec<SecretPattern>> = LazyLock::new(|| {
    let pattern = |source: &str, group: usize, accept: fn(&str) -> bool| {
        Regex::new(source).ok().map(|regex| SecretPattern {
            regex,
            group,
            accept,
        })
    };
    [
        // Блок закрытого ключа PEM (до конца блока или до конца текста, если он обрезан).
        pattern(
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)",
            0,
            any_value,
        ),
        // Значение заголовка авторизации: схема остаётся, токен скрывается.
        pattern(
            r"\b(?:OAuth|Bearer)\s+([A-Za-z0-9._~+/=-][A-Za-z0-9._~+/=_-]{15,})",
            1,
            has_digit,
        ),
        pattern(r"\bBasic\s+([A-Za-z0-9+/]{8,}={0,2})", 1, looks_like_base64),
        // OAuth-токены Яндекса: новый формат `y0_…` и старый `AQAAAA…`.
        pattern(r"\by[0-9]_[A-Za-z0-9_-]{30,}", 0, any_value),
        pattern(r"\bAQAAAA[A-Za-z0-9_-]{25,}", 0, any_value),
        // IAM-токены Yandex Cloud.
        pattern(r"\bt1\.[A-Za-z0-9_-]+\.[A-Za-z0-9_=-]{40,}", 0, any_value),
        // JWT.
        pattern(
            r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}",
            0,
            any_value,
        ),
        // Пароль в URL: `scheme://user:пароль@host`.
        pattern(
            r#"\b[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@"'<>]+:([^\s/@"'<>]+)@"#,
            1,
            any_value,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
});

/// Заменяет в тексте значения, похожие на секреты по форме (правило 2). Возвращает текст
/// и число замен.
pub fn redact_secret_values(text: &str) -> (String, usize) {
    let mut current = text.to_owned();
    let count = redact_secret_values_in_place(&mut current);
    (current, count)
}

fn redact_secret_values_in_place(text: &mut String) -> usize {
    // Быстрый выход для обычных строк: у всех шаблонов есть одна из этих подстрок.
    const MARKERS: &[&str] = &[
        "PRIVATE KEY",
        "OAuth",
        "Bearer",
        "Basic",
        "_",
        "AQAAAA",
        "t1.",
        "eyJ",
        "://",
    ];
    if !MARKERS.iter().any(|marker| text.contains(marker)) {
        return 0;
    }
    let mut count = 0;
    for pattern in SECRET_PATTERNS.iter() {
        let replaced = pattern.regex.replace_all(text, |caps: &Captures<'_>| {
            let whole = caps.get(0).map_or("", |m| m.as_str());
            let Some(secret) = caps.get(pattern.group) else {
                return whole.to_owned();
            };
            if secret.as_str() == REDACTED || !(pattern.accept)(secret.as_str()) {
                return whole.to_owned();
            }
            count += 1;
            let start = secret.start() - caps.get(0).map_or(0, |m| m.start());
            let end = start + secret.len();
            format!("{}{REDACTED}{}", &whole[..start], &whole[end..])
        });
        if let std::borrow::Cow::Owned(updated) = replaced {
            *text = updated;
        }
    }
    count
}

/// Вырезает значения из текстового тела: строковые значения пар `"ключ": "строка"` с ключами
/// учётных данных, в том числе незакрытая строка в конце (обрезанный JSON), и значения,
/// похожие на секреты по форме.
pub fn redact_text(text: &str) -> (String, Redactions) {
    let mut redactions = Redactions::default();
    let mut text = redact_credential_pairs(text, &mut redactions);
    redactions.secret_values += redact_secret_values_in_place(&mut text);
    (text, redactions)
}

fn redact_credential_pairs(text: &str, redactions: &mut Redactions) -> String {
    let bytes = text.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut result = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while let Some(offset) = text.get(i..).and_then(|rest| rest.find('"')) {
        let start = i + offset;
        let (key_end, after_key) = string_end(bytes, start);
        i = after_key;
        let colon = skip_ws(after_key);
        if bytes.get(colon) != Some(&b':') {
            continue;
        }
        let value_start = skip_ws(colon + 1);
        if bytes.get(value_start) != Some(&b'"') {
            continue;
        }
        let (value_end, after_value) = string_end(bytes, value_start);
        i = after_value;
        let key = text.get(start + 1..key_end).unwrap_or_default();
        let value = text.get(value_start + 1..value_end).unwrap_or_default();
        if !value.is_empty() && value != REDACTED && is_credential_key(key) {
            result.push_str(text.get(copied..value_start).unwrap_or_default());
            result.push('"');
            result.push_str(REDACTED);
            result.push('"');
            copied = after_value;
            redactions.credential_fields.insert(key.to_owned());
        }
    }
    result.push_str(text.get(copied..).unwrap_or_default());
    result
}

/// Для строки, открытой кавычкой в `start`: позиция закрывающей кавычки (или конец текста)
/// и позиция сразу после строки. Обе позиции — границы символов UTF-8: это `"` или конец.
fn string_end(bytes: &[u8], start: usize) -> (usize, usize) {
    let mut j = start + 1;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => j += 2,
            b'"' => return (j, j + 1),
            _ => j += 1,
        }
    }
    (bytes.len(), bytes.len())
}

/// Вырезает значения из успешного ответа API и возвращает, что скрыто.
pub fn redact_response(response: &mut ApiResponse) -> Redactions {
    match &mut response.body {
        ResponseBody::Json(json) => redact_json(json),
        ResponseBody::Text(text) => {
            let (redacted, redactions) = redact_text(text);
            *text = redacted;
            redactions
        }
        // Файл агент не видит, пустое тело скрывать нечего.
        ResponseBody::SavedToFile { .. } | ResponseBody::Empty => Redactions::default(),
    }
}

/// Ошибка API с вырезанными значениями в теле, сообщениях и подробностях Wiki, и что скрыто.
pub fn redact_api_error(err: ApiError) -> (ApiError, Redactions) {
    let ApiError::Http(mut http) = err else {
        return (err, Redactions::default());
    };
    let mut redactions = Redactions::default();
    http.body = match serde_json::from_str::<Value>(&http.body) {
        Ok(mut json) => {
            let found = redact_json(&mut json);
            if found.is_empty() {
                http.body
            } else {
                redactions.merge(found);
                json.to_string()
            }
        }
        Err(_) => {
            let (text, found) = redact_text(&http.body);
            redactions.merge(found);
            text
        }
    };
    for message in [&mut http.debug_message, &mut http.message]
        .into_iter()
        .flatten()
    {
        let (text, found) = redact_text(message);
        redactions.merge(found);
        *message = text;
    }
    if let Some(details) = &mut http.details {
        redactions.merge(redact_json(details));
    }
    (ApiError::Http(http), redactions)
}

/// Текст сообщения для агента (ошибки, не связанные с телом ответа): значения по форме и
/// пары `"ключ": "строка"`. Возвращает текст с пометкой, если что-то скрыто.
pub fn redact_message(text: &str) -> String {
    let (mut text, redactions) = redact_text(text);
    if let Some(note) = redactions.note() {
        text.push_str("\n\n");
        text.push_str(&note);
    }
    text
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiki_api::executor::HttpError;

    use super::*;

    /// Значения, похожие на настоящие токены по форме (не настоящие).
    const OAUTH: &str = "y0_AgAAAAAexampleexampleexampleexampleexampleexample";
    const IAM: &str = "t1.9euelZqexample.exampleexampleexampleexampleexampleexample12";
    /// Заголовок, данные и подпись JWT по отдельности.
    const JWT_PARTS: [&str; 3] = [
        "eyJhbGciOiJIUzI1NiJ9",
        "eyJzdWIiOiIxMjM0NTY3ODkwIn0",
        "c2lnbmF0dXJlLXZhbHVl",
    ];
    const PEM_DASHES: &str = "-----";

    fn http_error(body: &str) -> HttpError {
        HttpError {
            status: 400,
            error_code: Some("VALIDATION_ERROR".into()),
            debug_message: None,
            message: None,
            details: None,
            body: body.into(),
            retry_after: None,
        }
    }

    #[test]
    fn credential_keys() {
        for key in [
            "password",
            "Password",
            "db_pwd",
            "token",
            "accessToken",
            "oauth_token",
            "client-secret",
            "Authorization",
            "Proxy-Authorization",
            "privateKey",
            "apiKey",
            "apikey",
            "X-Api-Key",
            "X-Auth-Token",
            "Set-Cookie",
            "credentials",
        ] {
            assert!(is_credential_key(key), "{key}");
        }
        for key in [
            "key",
            "keys",
            "type",
            "has_password",
            "isSecret",
            "next_page_token",
            "nextToken",
            "recovery_token",
            "recoveryToken",
            "next_cursor",
            "tokens_count",
            "token_type",
            "author",
            "tokenizer",
        ] {
            assert!(!is_credential_key(key), "{key}");
        }
    }

    #[test]
    fn redacts_credential_fields_in_json() {
        let mut body = json!({
            "id": 1,
            "slug": "users/name/page",
            "rows": [{"row": {"service": "db", "password": "p@ss", "api_key": "k-1"}}],
            "token": "",
            "settings": {"oauth_token": "t-1"}
        });
        let redactions = redact_json(&mut body);
        assert_eq!(body["rows"][0]["row"]["password"], REDACTED);
        assert_eq!(body["rows"][0]["row"]["api_key"], REDACTED);
        assert_eq!(body["rows"][0]["row"]["service"], "db");
        assert_eq!(body["settings"]["oauth_token"], REDACTED);
        assert_eq!(body["slug"], "users/name/page");
        // Пустое значение оставляем: скрывать нечего.
        assert_eq!(body["token"], "");
        assert_eq!(
            redactions.credential_fields.iter().collect::<Vec<_>>(),
            ["api_key", "oauth_token", "password"]
        );
        let note = redactions.note().unwrap();
        assert!(note.contains("api_key, oauth_token, password"), "{note}");
        assert!(Redactions::default().note().is_none());
    }

    #[test]
    fn secret_values_by_shape() {
        let cases = [
            (
                format!("token {OAUTH} in text"),
                "token [redacted] in text".to_owned(),
            ),
            (
                format!("Authorization: OAuth {OAUTH}"),
                "Authorization: OAuth [redacted]".to_owned(),
            ),
            (format!("Bearer {IAM}"), "Bearer [redacted]".to_owned()),
            (format!("iam={IAM};"), "iam=[redacted];".to_owned()),
            (
                "Basic Ym90OnBAc3M=".to_owned(),
                "Basic [redacted]".to_owned(),
            ),
            (
                "AQAAAAAexampleexampleexampleexample1".to_owned(),
                "[redacted]".to_owned(),
            ),
            (
                // Части JWT и ключа PEM собираются при выполнении: целиком в исходниках они
                // выглядели бы для сканеров секретов как настоящие.
                format!("jwt {}.{}.{}", JWT_PARTS[0], JWT_PARTS[1], JWT_PARTS[2]),
                "jwt [redacted]".to_owned(),
            ),
            (
                "clone https://bot:hunter2@git.example.test/repo.git".to_owned(),
                "clone https://bot:[redacted]@git.example.test/repo.git".to_owned(),
            ),
            (
                format!(
                    "key:\n{d}BEGIN OPENSSH PRIVATE KEY{d}\nAAAA\n{d}END OPENSSH PRIVATE KEY{d}\nend",
                    d = PEM_DASHES
                ),
                "key:\n[redacted]\nend".to_owned(),
            ),
            (
                format!(
                    "{d}BEGIN RSA PRIVATE KEY{d}\nMIIE (обрезано",
                    d = PEM_DASHES
                ),
                "[redacted]".to_owned(),
            ),
        ];
        for (input, expected) in cases {
            let (text, count) = redact_secret_values(&input);
            assert_eq!(text, expected, "{input}");
            assert_eq!(count, 1, "{input}");
        }
        // Обычный текст не меняется.
        for plain in [
            "Basic authentication is used",
            "Bearer tokens are described in the docs",
            "OAuth application for the user",
            "https://example.test/path?x=1",
            "mailto:user@example.test",
            "issue_key_value TEST-1",
            "y0_short",
            "dGVzdA==",
            "[redacted]",
        ] {
            assert_eq!(
                redact_secret_values(plain),
                (plain.to_owned(), 0),
                "{plain}"
            );
        }
    }

    #[test]
    fn secret_values_in_page_text_and_pagination_ids() {
        let mut body = json!({
            "slug": "users/name/page",
            "content": format!("Use `OAuth {OAUTH}` for the bot"),
            "comments": [format!("iam {IAM}")],
            "next_cursor": OAUTH,
            "prev_cursor": OAUTH
        });
        let redactions = redact_json(&mut body);
        assert_eq!(body["content"], "Use `OAuth [redacted]` for the bot");
        assert_eq!(body["comments"][0], "iam [redacted]");
        assert_eq!(body["slug"], "users/name/page");
        // Идентификаторы постраничной выдачи остаются как есть.
        assert_eq!(body["next_cursor"], OAUTH);
        assert_eq!(body["prev_cursor"], OAUTH);
        assert_eq!(redactions.secret_values, 2);
        assert!(redactions.credential_fields.is_empty());
        let note = redactions.note().unwrap();
        assert!(note.contains("2 value(s)"), "{note}");
    }

    #[test]
    fn redacts_text_and_truncated_json() {
        let (text, redactions) = redact_text(r#"{"a": "b", "password" : "x\"y", "token": "tail"#);
        assert_eq!(
            text,
            r#"{"a": "b", "password" : "[redacted]", "token": "[redacted]""#
        );
        assert_eq!(redactions.credential_fields.len(), 2);
        let (text, redactions) = redact_text(&format!("<html>{OAUTH}</html>"));
        assert_eq!(text, "<html>[redacted]</html>");
        assert_eq!(redactions.secret_values, 1);
        let (plain, redactions) = redact_text("plain text without pairs");
        assert_eq!(plain, "plain text without pairs");
        assert!(redactions.is_empty());
    }

    #[test]
    fn redacts_responses_and_errors() {
        let mut response = ApiResponse {
            status: 200,
            body: ResponseBody::Json(json!({"secret": "s"})),
        };
        assert!(!redact_response(&mut response).is_empty());
        assert_eq!(
            response.body,
            ResponseBody::Json(json!({"secret": REDACTED}))
        );
        let mut response = ApiResponse {
            status: 200,
            body: ResponseBody::Text(format!("token {IAM}")),
        };
        assert_eq!(redact_response(&mut response).secret_values, 1);
        assert_eq!(response.body, ResponseBody::Text("token [redacted]".into()));

        let mut http = http_error(&format!(
            r#"{{"error_code":"VALIDATION_ERROR","message":"bad {OAUTH}","details":{{"password":"p1"}}}}"#
        ));
        http.debug_message = Some(format!("header Bearer {IAM}"));
        http.message = Some(format!("bad {OAUTH}"));
        http.details = Some(json!({"password": "p1", "url": "https://bot:pw2@example.test/x"}));
        let (ApiError::Http(http), redactions) = redact_api_error(ApiError::Http(Box::new(http)))
        else {
            panic!("ожидалась ошибка HTTP");
        };
        assert!(
            !http.body.contains("p1") && !http.body.contains(OAUTH),
            "{}",
            http.body
        );
        assert_eq!(
            http.debug_message.as_deref(),
            Some("header Bearer [redacted]")
        );
        assert_eq!(http.message.as_deref(), Some("bad [redacted]"));
        let details = http.details.as_ref().unwrap();
        assert_eq!(details["password"], REDACTED);
        assert_eq!(details["url"], "https://bot:[redacted]@example.test/x");
        assert!(redactions.credential_fields.contains("password"));
        // Тело (1), debug_message, message и URL в details.
        assert_eq!(redactions.secret_values, 4);

        // Тело без секретов возвращается как есть (без переформатирования).
        let body = r#"{ "error_code": "NOT_FOUND", "debug_message": "Страница не найдена" }"#;
        let (ApiError::Http(http), redactions) =
            redact_api_error(ApiError::Http(Box::new(http_error(body))))
        else {
            panic!("ожидалась ошибка HTTP");
        };
        assert_eq!(http.body, body);
        assert!(redactions.is_empty());

        let (err, redactions) = redact_api_error(ApiError::MissingOrg);
        assert!(matches!(err, ApiError::MissingOrg));
        assert!(redactions.is_empty());
    }

    #[test]
    fn messages_get_a_note() {
        let text = redact_message(&format!(
            "could not reach https://bot:pw1@example.test ({OAUTH})"
        ));
        assert!(!text.contains("pw1") && !text.contains(OAUTH), "{text}");
        assert!(text.contains("2 value(s)"), "{text}");
        assert_eq!(redact_message("plain error"), "plain error");
    }
}
