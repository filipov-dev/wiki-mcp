//! Сопоставление произвольного запроса (`api_request`) с операциями спецификации.
//!
//! Уровень доступа `api_request` определяется не только HTTP-методом: если запрос совпадает
//! с известной операцией, действует её уровень `x-access-level`. В Wiki API `POST` означает
//! и чтение (`POST /v1/search`, проверка slug столбца `POST /v1/grids/{idx}/columns/suggest`),
//! и правку, а изменение прав доступа к странице через `POST` — уровень удаления.
//!
//! Правила сопоставления:
//!
//! - метод запроса должен совпадать с методом операции;
//! - сравнивается шаблон [`OperationDef::path`]; один завершающий `/` в пути запроса
//!   игнорируется;
//! - число сегментов совпадает; литеральный сегмент шаблона сравнивается без учёта регистра;
//! - `{param}` совпадает с одним непустым сегментом; литеральные префикс и суффикс вокруг
//!   параметра обязательны, а значение между ними не может быть пустым;
//! - сегмент запроса сравнивается и в исходном, и в percent-раскодированном виде
//!   (до нескольких раундов), чтобы `se%61rch` не обходил совпадение с `search`.
//!
//! Если совпало несколько операций (например, `/v1/pages/descendants` подходит и под
//! `/v1/pages/descendants`, и под `/v1/pages/{idx}`), требуется максимальный уровень доступа
//! среди них: решение не зависит от того, как маршрутизирует запрос сервер Wiki. Если не
//! совпало ничего — уровень по методу ([`default_access`]).

use std::borrow::Cow;

use percent_encoding::percent_decode_str;
use wiki_api::spec::{AccessLevel, HttpMethod, OperationDef, operations};

/// Сколько раундов percent-раскодирования проверять (как в `validate_relative_path`).
const MAX_DECODE_ROUNDS: usize = 3;

/// Операция, с которой совпал запрос.
#[derive(Debug, Clone, Copy)]
pub struct RouteMatch {
    pub op: &'static OperationDef,
    /// Число литеральных символов шаблона, совпавших с запросом: чем больше, тем точнее.
    pub specificity: usize,
}

/// Уровень доступа запроса к пути, которого нет в спецификации: `GET` — чтение, `DELETE` —
/// удаление, остальные методы — запись.
pub fn default_access(method: HttpMethod) -> AccessLevel {
    match method {
        HttpMethod::Get => AccessLevel::Read,
        HttpMethod::Delete => AccessLevel::Destructive,
        HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch => AccessLevel::Write,
    }
}

/// Все операции с методом `method`, шаблон которых совпадает с `path`.
///
/// Порядок: сначала самые точные совпадения (больше литеральных символов), затем по имени.
pub fn match_operations(method: HttpMethod, path: &str) -> Vec<RouteMatch> {
    let segments = request_segments(path);
    let mut matches: Vec<RouteMatch> = operations()
        .iter()
        .filter(|op| op.method == method)
        .filter_map(|op| {
            template_specificity(op.path, &segments)
                .map(|specificity| RouteMatch { op, specificity })
        })
        .collect();
    matches.sort_by(|a, b| {
        b.specificity
            .cmp(&a.specificity)
            .then_with(|| a.op.tool_name.cmp(b.op.tool_name))
    });
    matches
}

/// Решение о доступе для запроса `api_request`.
#[derive(Debug, Clone)]
pub struct AccessDecision {
    /// Требуемый уровень доступа.
    pub required: AccessLevel,
    /// Совпавшие операции, самые точные первыми; пусто — уровень взят по методу.
    pub matches: Vec<RouteMatch>,
}

impl AccessDecision {
    /// Самая точная совпавшая операция.
    pub fn operation(&self) -> Option<&'static OperationDef> {
        self.matches.first().map(|m| m.op)
    }

    /// Совпала ли операция с именем инструмента `tool_name`.
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        self.matches.iter().any(|m| m.op.tool_name == tool_name)
    }

    /// Операция, на которой требуемый уровень достигается (самая точная из таких).
    pub fn strictest(&self) -> Option<&'static OperationDef> {
        self.matches
            .iter()
            .find(|m| m.op.access == self.required)
            .map(|m| m.op)
    }
}

/// Какой уровень доступа нужен запросу `method path`.
pub fn required_access(method: HttpMethod, path: &str) -> AccessDecision {
    let matches = match_operations(method, path);
    let required = matches
        .iter()
        .map(|m| m.op.access)
        .max()
        .unwrap_or_else(|| default_access(method));
    AccessDecision { required, matches }
}

/// Сегменты пути без ведущего и одного завершающего `/`.
fn request_segments(path: &str) -> Vec<&str> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split('/').collect()
}

/// Точность совпадения шаблона с сегментами запроса; `None` — не совпадает.
fn template_specificity(template: &str, segments: &[&str]) -> Option<usize> {
    let template_segments = request_segments(template);
    if template_segments.len() != segments.len() || segments.is_empty() {
        return None;
    }
    template_segments
        .iter()
        .zip(segments)
        .map(|(pattern, segment)| segment_specificity(pattern, segment))
        .sum()
}

/// Совпадение одного сегмента шаблона: литерал или `префикс{param}суффикс`.
fn segment_specificity(pattern: &str, segment: &str) -> Option<usize> {
    if segment.is_empty() {
        return None;
    }
    let variants = decoded_variants(segment);
    let Some((prefix, rest)) = pattern.split_once('{') else {
        return variants
            .iter()
            .any(|value| value.eq_ignore_ascii_case(pattern))
            .then_some(pattern.len());
    };
    // В шаблонах спецификации не больше одного параметра на сегмент (проверяется тестом).
    let suffix = rest.split_once('}').map_or("", |(_, suffix)| suffix);
    variants
        .iter()
        .any(|value| {
            value.len() > prefix.len() + suffix.len()
                && starts_with_ignore_case(value, prefix)
                && ends_with_ignore_case(value, suffix)
        })
        .then_some(prefix.len() + suffix.len())
}

/// Исходный сегмент и его percent-раскодированные варианты.
fn decoded_variants(segment: &str) -> Vec<Cow<'_, str>> {
    let mut variants = vec![Cow::Borrowed(segment)];
    for _ in 0..MAX_DECODE_ROUNDS {
        let Some(last) = variants.last() else { break };
        let decoded = percent_decode_str(last).decode_utf8_lossy().into_owned();
        if decoded == **last {
            break;
        }
        variants.push(Cow::Owned(decoded));
    }
    variants
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ends_with_ignore_case(value: &str, suffix: &str) -> bool {
    value
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| value.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(method: HttpMethod, path: &str) -> Vec<&'static str> {
        match_operations(method, path)
            .iter()
            .map(|m| m.op.tool_name)
            .collect()
    }

    fn first(method: HttpMethod, path: &str) -> Option<&'static str> {
        required_access(method, path)
            .operation()
            .map(|op| op.tool_name)
    }

    fn access(method: HttpMethod, path: &str) -> AccessLevel {
        required_access(method, path).required
    }

    /// Шаблон операции с подставленными значениями параметров.
    fn concrete(template: &str) -> String {
        let mut out = String::new();
        let mut rest = template;
        while let Some((head, tail)) = rest.split_once('{') {
            out.push_str(head);
            out.push_str("X1");
            rest = tail.split_once('}').map_or("", |(_, after)| after);
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn templates_have_one_param_per_segment_and_a_version() {
        for op in operations() {
            let segments = request_segments(op.path);
            assert!(
                wiki_api::path::API_VERSIONS.contains(&segments[0]),
                "{}: {}",
                op.tool_name,
                op.path
            );
            for segment in segments {
                assert!(
                    segment.matches('{').count() <= 1,
                    "{}: {}",
                    op.tool_name,
                    op.path
                );
            }
        }
    }

    #[test]
    fn every_operation_matches_its_own_template() {
        for op in operations() {
            let path = concrete(op.path);
            let decision = required_access(op.method, &path);
            assert!(
                decision.matches_tool(op.tool_name),
                "{} не совпала с {path}",
                op.tool_name
            );
            assert!(decision.required >= op.access, "{}", op.tool_name);
        }
    }

    #[test]
    fn read_only_posts_stay_read_only() {
        // Поиск и проверка slug столбца через POST не должны совпадать с изменяющими
        // операциями: иначе в режиме чтения `api_request` отказал бы в них.
        let read_posts: Vec<_> = operations()
            .iter()
            .filter(|op| op.method == HttpMethod::Post && op.access == AccessLevel::Read)
            .collect();
        let names_of: Vec<&str> = read_posts.iter().map(|op| op.tool_name).collect();
        assert!(names_of.contains(&"search"), "{names_of:?}");
        assert!(names_of.contains(&"suggest_grid_column"), "{names_of:?}");
        for op in read_posts {
            let path = concrete(op.path);
            assert_eq!(
                access(HttpMethod::Post, &path),
                AccessLevel::Read,
                "{}: {path} {:?}",
                op.tool_name,
                names(HttpMethod::Post, &path)
            );
        }
    }

    #[test]
    fn literal_segments_win_over_parameters() {
        assert_eq!(first(HttpMethod::Post, "/v1/search"), Some("search"));
        assert_eq!(
            first(HttpMethod::Get, "/v1/pages/descendants"),
            Some("list_page_descendants")
        );
        assert_eq!(
            first(HttpMethod::Get, "/v1/pages/42"),
            Some("get_page_by_id")
        );
        assert_eq!(first(HttpMethod::Post, "/v1/pages/move"), Some("move_page"));
        assert_eq!(
            first(HttpMethod::Post, "/v1/grids/abc/rows/move"),
            Some("move_grid_rows")
        );
        assert_eq!(
            first(HttpMethod::Get, "/v1/pages/42/attachments/7/download"),
            Some("download_attachment")
        );
    }

    #[test]
    fn unknown_paths_and_methods() {
        assert!(names(HttpMethod::Get, "/v1/no/such/endpoint").is_empty());
        assert!(names(HttpMethod::Get, "/").is_empty());
        assert!(names(HttpMethod::Get, "").is_empty());
        // Путь операции, но другой метод: совпадения нет.
        assert!(names(HttpMethod::Put, "/v1/search").is_empty());
        // Другая версия API — не та операция.
        assert!(names(HttpMethod::Post, "/v2/search").is_empty());
    }

    #[test]
    fn trailing_slash_case_and_encoding() {
        assert_eq!(first(HttpMethod::Post, "/v1/search/"), Some("search"));
        assert_eq!(first(HttpMethod::Post, "/V1/Search"), Some("search"));
        assert_eq!(first(HttpMethod::Post, "/v1/se%61rch"), Some("search"));
        assert_eq!(first(HttpMethod::Post, "/v1/se%2561rch"), Some("search"));
        assert_eq!(
            first(HttpMethod::Delete, "/v1/pages/4%2532"),
            Some("delete_page")
        );
        // Только один завершающий `/`.
        assert_eq!(first(HttpMethod::Get, "/v1/users/me//"), None);
        assert_eq!(
            first(HttpMethod::Get, "/v1/users/me/"),
            Some("get_current_user")
        );
    }

    #[test]
    fn access_comes_from_operation_then_method() {
        assert_eq!(access(HttpMethod::Post, "/v1/pages"), AccessLevel::Write);
        assert_eq!(access(HttpMethod::Post, "/v1/search"), AccessLevel::Read);
        assert_eq!(
            access(HttpMethod::Post, "/v1/grids/abc/columns/suggest"),
            AccessLevel::Read
        );
        assert_eq!(
            access(HttpMethod::Delete, "/v1/pages/42"),
            AccessLevel::Destructive
        );
        // Права доступа меняются через POST, но это уровень удаления.
        assert_eq!(
            access(HttpMethod::Post, "/v1/pages/42/access/7"),
            AccessLevel::Destructive
        );
        assert_eq!(
            access(HttpMethod::Post, "/v1/pages/42/append-content"),
            AccessLevel::Write
        );
        // Неизвестные пути — по методу.
        for (method, expected) in [
            (HttpMethod::Get, AccessLevel::Read),
            (HttpMethod::Post, AccessLevel::Write),
            (HttpMethod::Put, AccessLevel::Write),
            (HttpMethod::Patch, AccessLevel::Write),
            (HttpMethod::Delete, AccessLevel::Destructive),
        ] {
            assert_eq!(access(method, "/v1/unknown/thing"), expected, "{method:?}");
            assert_eq!(default_access(method), expected);
        }
    }

    #[test]
    fn ambiguous_paths_need_the_strictest_level() {
        // `/v1/pages/{idx}` и литеральные пути на том же месте: решает максимальный уровень.
        let decision = required_access(HttpMethod::Post, "/v1/pages/move");
        assert!(decision.matches_tool("move_page"));
        assert!(decision.matches_tool("update_page"));
        assert_eq!(decision.required, AccessLevel::Write);

        let decision = required_access(HttpMethod::Delete, "/v1/pages/42");
        assert_eq!(
            decision.strictest().map(|op| op.tool_name),
            Some("delete_page")
        );
        assert!(
            required_access(HttpMethod::Get, "/v1/unknown")
                .strictest()
                .is_none()
        );
    }
}
