//! Наборы инструментов из верхнеуровневого `tags` спецификации.
//!
//! Каждый тег — набор: `name` — slug для `--toolsets`, `description` — описание набора,
//! `x-default-enabled` — включён ли набор без `--toolsets`. Наборы задаёт файл-дополнение,
//! поэтому признак включения обязателен: новый набор должен получить его осознанно.

use std::collections::BTreeSet;

use serde_json::Value;

/// Slug, который ядро MCP использует для «все наборы»; набору его давать нельзя.
const RESERVED_SLUGS: &[&str] = &["all"];

#[derive(Debug)]
pub struct Toolset {
    pub slug: String,
    pub description: String,
    pub default_enabled: bool,
}

/// Наборы инструментов в порядке `tags` спецификации.
pub fn parse_toolsets(spec: &Value) -> Vec<Toolset> {
    let tags = spec
        .get("tags")
        .and_then(Value::as_array)
        .expect("в спецификации нет tags: список наборов инструментов");
    let mut toolsets = Vec::new();
    let mut seen = BTreeSet::new();
    for tag in tags {
        let slug = tag
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("у набора инструментов в tags нет name: {tag}"))
            .to_owned();
        assert!(
            is_valid_slug(&slug),
            "недопустимый slug набора инструментов «{slug}»: нужны строчные латинские буквы, \
             цифры и `_`, первая — буква"
        );
        assert!(
            !RESERVED_SLUGS.contains(&slug.as_str()),
            "slug набора инструментов «{slug}» зарезервирован"
        );
        assert!(
            seen.insert(slug.clone()),
            "набор инструментов «{slug}» объявлен в tags дважды"
        );
        let description = tag
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_owned();
        let default_enabled = tag
            .get("x-default-enabled")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| {
                panic!("у набора инструментов «{slug}» нет булева x-default-enabled")
            });
        toolsets.push(Toolset {
            slug,
            description,
            default_enabled,
        });
    }
    assert!(!toolsets.is_empty(), "в tags спецификации нет наборов");
    toolsets
}

/// Slug: строчные латинские буквы, цифры и `_`, начинается с буквы.
pub fn is_valid_slug(slug: &str) -> bool {
    slug.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
