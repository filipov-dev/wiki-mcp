//! Имена MCP-инструментов.
//!
//! Имя инструмента — `operationId` объединённой спецификации как есть: короткие имена
//! в snake_case задаются в файле-дополнении (`spec/overlay.json`), а длинный исходный
//! `operationId` официальной спецификации сохраняется в `x-upstream-operation-id`.

/// Имя MCP-инструмента должно соответствовать `^[a-zA-Z0-9_-]{1,64}$`.
pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
