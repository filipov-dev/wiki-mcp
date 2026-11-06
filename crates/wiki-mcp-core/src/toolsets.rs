//! Наборы инструментов (`--toolsets`).
//!
//! Набор — тег спецификации с коротким английским slug из `wiki_api::spec::toolsets()`.
//! Набор выбирается точным совпадением slug; `all` включает все наборы. Неизвестный набор —
//! ошибка запуска со списком допустимых.
//!
//! Без выбора включаются наборы с `x-default-enabled: true` ([`default_toolsets`]): страницы,
//! вложения, комментарии и динамические таблицы. Остальные (`access` — персональные права
//! доступа, `extra` — операции вне справочника) включаются только явно: права доступа —
//! чувствительная операция, а описания редких операций заметно увеличивают список
//! инструментов в контексте агента.

use wiki_api::spec::{self, find_toolset, operations, toolsets};

/// Значение `--toolsets`, которое включает все наборы.
pub const ALL_TOOLSETS: &str = "all";

/// Набор инструментов: slug, описание и число операций в спецификации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    pub slug: &'static str,
    pub description: &'static str,
    pub operations: usize,
    /// Включён без `--toolsets` и без настройки профиля.
    pub default: bool,
}

/// Наборы, включённые без `--toolsets` и без настройки профиля, в порядке спецификации.
pub fn default_toolsets() -> Vec<&'static str> {
    spec::default_toolsets()
        .map(|toolset| toolset.slug)
        .collect()
}

/// Все наборы в порядке спецификации.
pub fn available_toolsets() -> Vec<Toolset> {
    toolsets()
        .iter()
        .map(|toolset| Toolset {
            slug: toolset.slug,
            description: toolset.description,
            operations: operations()
                .iter()
                .filter(|op| op.toolset == toolset.slug)
                .count(),
            default: toolset.default_enabled,
        })
        .collect()
}

/// Проверяет имена наборов: пробелы по краям и пустые элементы отбрасываются, повторы
/// убираются, `all` раскрывается во все наборы. Неизвестный набор — ошибка.
pub fn normalize_toolsets<S: AsRef<str>>(names: &[S]) -> Result<Vec<String>, String> {
    let mut result: Vec<String> = Vec::new();
    for name in names {
        let name = name.as_ref().trim();
        if name.is_empty() {
            continue;
        }
        if name == ALL_TOOLSETS {
            return Ok(toolsets().iter().map(|t| t.slug.to_owned()).collect());
        }
        if find_toolset(name).is_none() {
            let list: Vec<&str> = toolsets().iter().map(|t| t.slug).collect();
            return Err(format!(
                "unknown toolset `{name}`; available toolsets: {}, or `{ALL_TOOLSETS}`",
                list.join(", ")
            ));
        }
        if !result.iter().any(|known| known == name) {
            result.push(name.to_owned());
        }
    }
    Ok(result)
}

/// Наборы, которые действуют при выборе `selected` (`None` или пусто — по умолчанию).
pub fn effective_toolsets(selected: Option<&[String]>) -> Vec<String> {
    match selected {
        Some(names) if !names.is_empty() => names.to_vec(),
        _ => default_toolsets()
            .into_iter()
            .map(ToOwned::to_owned)
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_main_work_and_skip_rare_toolsets() {
        assert_eq!(
            default_toolsets(),
            ["pages", "attachments", "comments", "grids"]
        );
        let all = available_toolsets();
        assert_eq!(
            all.iter().map(|t| t.operations).sum::<usize>(),
            operations().len()
        );
        for slug in ["access", "extra"] {
            let toolset = all.iter().find(|t| t.slug == slug).unwrap();
            assert!(!toolset.default, "{slug}");
            assert!(toolset.operations > 0, "{slug}");
        }
    }

    #[test]
    fn normalize_validates_names() {
        assert_eq!(
            normalize_toolsets(&[" pages ", "", "grids", "pages"]).unwrap(),
            ["pages", "grids"]
        );
        let err = normalize_toolsets(&["Страницы"]).unwrap_err();
        assert!(err.contains("unknown toolset `Страницы`"), "{err}");
        assert!(err.contains("pages") && err.contains("`all`"), "{err}");
        let all = normalize_toolsets(&["pages", "all"]).unwrap();
        assert_eq!(all.len(), toolsets().len());
        assert!(all.contains(&"access".to_owned()));
        assert!(normalize_toolsets::<&str>(&[]).unwrap().is_empty());
    }

    #[test]
    fn effective_falls_back_to_defaults() {
        assert_eq!(effective_toolsets(None), default_toolsets());
        assert_eq!(effective_toolsets(Some(&[])), default_toolsets());
        let chosen = vec!["access".to_owned()];
        assert_eq!(effective_toolsets(Some(&chosen)), ["access"]);
    }
}
