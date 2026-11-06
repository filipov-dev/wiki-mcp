//! Проверка и сборка относительных путей Wiki API.
//!
//! Это граница безопасности: всё, что уходит в [`crate::executor::ApiClient::execute`],
//! проходит через [`validate_relative_path`], поэтому запрос не может уйти на другой хост
//! или выйти за пределы API через `..`, в том числе в percent-кодированном виде.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::executor::ApiError;

/// Версии API, с которых обязан начинаться путь: `/v1/...`.
pub const API_VERSIONS: [&str; 1] = ["v1"];

/// Символы, которые кодируются в значении параметра пути.
///
/// Оставляем только unreserved-символы RFC 3986, поэтому `/`, `?`, `#`, `%`, `:` и прочие
/// разделители внутри значения становятся `%XX` и не могут изменить структуру пути.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Сколько раз раскодировать сегмент при проверке. Защищает от двойного кодирования
/// (`%252e%252e`) на случай, если что-то между нами и API раскодирует путь повторно.
const MAX_DECODE_ROUNDS: usize = 3;

/// Проверяет относительный путь Wiki API, например `/v1/pages/123/comments`.
///
/// Требования:
/// - начинается с `/v1/` (см. [`API_VERSIONS`]), поэтому не может быть `//`,
///   схемой или хостом;
/// - содержит только символы `pchar` из RFC 3986 и `/`: `?`, `#`, `\`, пробелы,
///   управляющие и не-ASCII символы отклоняются (query передаётся отдельно);
/// - `%` всегда образует корректную тройку `%XX`;
/// - нет пустых сегментов (`//`) кроме завершающего `/`;
/// - ни один сегмент ни в исходном, ни в раскодированном виде не равен `.` или `..`,
///   а раскодированный сегмент не содержит `.`/`..` между `/` или `\` и управляющих символов.
pub fn validate_relative_path(path: &str) -> Result<(), ApiError> {
    let invalid = |reason: &str| {
        Err(ApiError::InvalidPath(format!(
            "{reason}: `{}`",
            shorten(path)
        )))
    };

    if !path.starts_with('/') {
        return invalid("путь должен начинаться с `/`");
    }
    if path.starts_with("//") {
        return invalid("путь не может начинаться с `//`");
    }
    if let Some(c) = path.chars().find(|c| !is_allowed_char(*c)) {
        return Err(ApiError::InvalidPath(format!(
            "недопустимый символ {c:?} в пути `{}`; query передаётся отдельно, \
             остальные символы кодируйте как %XX",
            shorten(path)
        )));
    }
    if !has_valid_percent_triplets(path) {
        return invalid("некорректная percent-последовательность");
    }

    let segments: Vec<&str> = path[1..].split('/').collect();
    let version_ok =
        segments.len() >= 2 && API_VERSIONS.contains(&segments[0]) && !segments[1].is_empty();
    if !version_ok {
        return invalid("путь должен начинаться с `/v1/` и указывать ресурс");
    }
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            // Пустой сегмент допустим только в конце (`/v1/pages/`).
            if index == last {
                continue;
            }
            return invalid("пустой сегмент пути");
        }
        if !segment_is_safe(segment) {
            return invalid("сегменты `.` и `..` запрещены, в том числе в закодированном виде");
        }
    }
    Ok(())
}

/// Подставляет параметры в шаблон пути вида `/v1/pages/{idx}/comments/{comment_id}`
/// и проверяет результат через [`validate_relative_path`].
///
/// Значение параметра кодируется как один сегмент пути: `a/b` превращается в `a%2Fb`,
/// поэтому значение не может добавить сегменты или выйти из пути. Отсутствующий или
/// пустой параметр даёт [`ApiError::MissingPathParam`]. Значения `.` и `..` отклоняются
/// проверкой итогового пути.
pub fn render_path<F>(template: &str, mut lookup: F) -> Result<String, ApiError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return Err(ApiError::InvalidPath(format!(
                "лишняя `}}` в шаблоне `{}`",
                shorten(template)
            )));
        }
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(ApiError::InvalidPath(format!(
                "незакрытая `{{` в шаблоне `{}`",
                shorten(template)
            )));
        };
        let name = &after[..close];
        if name.is_empty() || name.contains('{') {
            return Err(ApiError::InvalidPath(format!(
                "некорректный параметр в шаблоне `{}`",
                shorten(template)
            )));
        }
        let value = lookup(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::MissingPathParam(name.to_string()))?;
        out.extend(utf8_percent_encode(&value, SEGMENT));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    validate_relative_path(&out)?;
    Ok(out)
}

/// `pchar` из RFC 3986 плюс разделитель `/`; `%` проверяется отдельно.
fn is_allowed_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
                | '/'
                | '%'
        )
}

fn has_valid_percent_triplets(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let ok = bytes
                .get(i + 1..i + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
            if !ok {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// Проверяет сегмент и его раскодированные варианты.
///
/// Закодированный `/` внутри значения (`a%2Fb`) допустим — так `render_path` передаёт
/// значения со слешем. Но если сервер раскодирует `%2F` до маршрутизации, `..%2F..`
/// превратится в обход каталога, поэтому раскодированный сегмент дополнительно делим
/// по `/` и `\` и ищем `.`/`..`.
fn segment_is_safe(segment: &str) -> bool {
    let mut current = segment.to_string();
    for _ in 0..=MAX_DECODE_ROUNDS {
        if current
            .split(['/', '\\'])
            .any(|part| part == "." || part == "..")
        {
            return false;
        }
        if current.chars().any(char::is_control) {
            return false;
        }
        if !current.contains('%') {
            return true;
        }
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            return true;
        }
        current = decoded;
    }
    // Слишком глубокое вложенное кодирование — подозрительно, отклоняем.
    false
}

/// Укорачивает путь для текста ошибки, чтобы не раздувать ответ MCP.
fn shorten(path: &str) -> String {
    const MAX: usize = 200;
    if path.len() <= MAX {
        return path.escape_debug().to_string();
    }
    let mut end = MAX;
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", path[..end].escape_debug())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn accepts_valid_paths() {
        for path in [
            "/v1/users/me",
            "/v1/pages",
            "/v1/pages/123/comments",
            "/v1/pages/descendants",
            "/v1/pages/",
            "/v1/pages/1/attachments/2/download",
            "/v1/upload_sessions/0f8fad5b-d9cb-469f-a165-70867728950e/upload_part",
            "/v1/pages/1/append-content",
            "/v1/a/b..c/.hidden/d.",
            "/v1/grids/x/column/id:1@x",
            "/v1/pages/file%20name.png",
        ] {
            assert!(
                validate_relative_path(path).is_ok(),
                "{path} должен проходить"
            );
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "",
            "/",
            "/v1",
            "/v1/",
            "/v2/pages",
            "/v3/pages",
            "/V1/pages",
            "/pages",
            "v1/pages",
            "https://evil.example/v1/pages",
            "http://evil.example",
            "https://api.wiki.yandex.net/v1/pages",
            "//evil.example/v1/pages",
            "///evil",
            "/v1//pages",
            "/v1/pages/../admin",
            "/v1/../v2/pages",
            "/v1/./x",
            "/v1/pages/..",
            "/v1/pages/%2e%2e/admin",
            "/v1/pages/%2E%2E/admin",
            "/v1/pages/.%2e/admin",
            "/v1/pages/%2e/admin",
            "/v1/pages/..%2fadmin",
            "/v1/pages/x%2F..%2Fadmin",
            "/v1/pages/..%5cadmin",
            "/v1/pages/%252e%252e/admin",
            "/v1/pages/%25252e%25252e/admin",
            "/v1/pages?slug=x",
            "/v1/pages#frag",
            "/v1/pages\\..\\admin",
            "/v1/pages/a b",
            "/v1/pages/\n",
            "/v1/pages/%00",
            "/v1/pages/%0a",
            "/v1/pages/%zz",
            "/v1/pages/%2",
            "/v1/pages/{idx}",
            "/v1/pages/кириллица",
            "/v1/pages/x\u{0}",
        ] {
            assert!(
                matches!(validate_relative_path(path), Err(ApiError::InvalidPath(_))),
                "{path:?} должен отклоняться"
            );
        }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn render_substitutes_and_encodes() {
        let p = params(&[
            ("idx", "12"),
            ("access_id", "a/b"),
            ("column_slug", "K?#% x"),
        ]);
        let rendered = render_path("/v1/pages/{idx}/access/{access_id}/{column_slug}", |n| {
            p.get(n).cloned()
        })
        .unwrap();
        assert_eq!(rendered, "/v1/pages/12/access/a%2Fb/K%3F%23%25%20x");
    }

    #[test]
    fn render_encodes_non_ascii() {
        let p = params(&[("column_slug", "колонка")]);
        let rendered =
            render_path("/v1/grids/g/column/{column_slug}", |n| p.get(n).cloned()).unwrap();
        assert_eq!(
            rendered,
            "/v1/grids/g/column/%D0%BA%D0%BE%D0%BB%D0%BE%D0%BD%D0%BA%D0%B0"
        );
    }

    #[test]
    fn render_reports_missing_and_empty_params() {
        let p = params(&[("idx", "1"), ("comment_id", "")]);
        let lookup = |n: &str| p.get(n).cloned();
        assert!(matches!(
            render_path("/v1/pages/{idx}/{missing}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "missing"
        ));
        assert!(matches!(
            render_path("/v1/pages/{idx}/comments/{comment_id}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "comment_id"
        ));
    }

    #[test]
    fn render_rejects_traversal_values() {
        for value in ["..", ".", "../admin", "..\\x"] {
            let result = render_path("/v1/pages/{idx}/comments", |_| Some(value.to_string()));
            assert!(
                matches!(result, Err(ApiError::InvalidPath(_))),
                "значение {value:?} должно отклоняться"
            );
        }
    }

    #[test]
    fn render_rejects_malformed_templates() {
        for template in [
            "/v1/pages/{idx",
            "/v1/pages/idx}",
            "/v1/pages/{}",
            "/v1/pages/{{x}}",
        ] {
            assert!(
                matches!(
                    render_path(template, |_| Some("v".into())),
                    Err(ApiError::InvalidPath(_))
                ),
                "{template} должен отклоняться"
            );
        }
    }

    #[test]
    fn render_rejects_absolute_template() {
        assert!(render_path("https://evil/{x}", |_| Some("v".into())).is_err());
        // Значение параметра в версии не может подменить версию API.
        assert!(render_path("/{version}/pages", |_| Some("v2".into())).is_err());
    }
}
