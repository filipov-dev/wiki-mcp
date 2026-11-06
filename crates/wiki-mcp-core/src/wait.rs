//! Составной инструмент [`WAIT_FOR_OPERATION_TOOL`]: ожидание фоновой операции Wiki.
//!
//! Копирование страницы (`clone_page`), копирование таблицы (`clone_grid`) и перенос страниц
//! (`move_page`) выполняются фоном: ответ содержит `operation.type` и `operation.id`.
//! Инструмент опрашивает `get_*_operation_status` для этого типа, пока статус не станет
//! `success` или `failed`, но не дольше тайм-аута.
//!
//! Интервал опроса растёт ([`POLL_DELAYS`]): короткие операции завершаются быстро, а долгие
//! не нагружают API частыми запросами. Тайм-аут ограничен [`MAX_TIMEOUT_SECS`]: MCP-клиенты
//! сами обрывают долгие вызовы инструментов, а операция продолжается фоном и после
//! тайм-аута — агент может вызвать инструмент снова.
//!
//! Итог: `success` — ответ статуса как есть; `failed`, неизвестный статус и тайм-аут —
//! управляемая ошибка с последним ответом статуса; ошибка запроса статуса — ошибка Wiki.

use std::time::Duration;

use rmcp::model::JsonObject;
use serde_json::{Value, json};
use tokio::time::Instant;
use wiki_api::executor::ApiClient;

use crate::composite::{
    WAIT_FOR_OPERATION_TOOL, argument, call_json, object, reject_unknown, required_string,
    step_operation,
};
use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::redact::Redactions;
use crate::request::build_request;
use crate::server::with_note;

/// Типы операций (`operation.type` ответа) и инструменты их статуса.
const OPERATIONS: &[(&str, &str)] = &[
    ("clone", "get_clone_operation_status"),
    (
        "clone_inline_grid",
        "get_clone_inline_grid_operation_status",
    ),
    ("move", "get_move_operation_status"),
];

/// Операции шагов.
pub(crate) const STEPS: &[&str] = &[
    "get_clone_operation_status",
    "get_clone_inline_grid_operation_status",
    "get_move_operation_status",
];

const ARGUMENTS: &[&str] = &["operation_type", "operation_id", "timeout_seconds"];

/// Тайм-аут по умолчанию и наибольший, секунд.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
pub const MAX_TIMEOUT_SECS: u64 = 120;

/// Паузы между опросами по порядку; дальше повторяется последняя.
pub const POLL_DELAYS: &[Duration] = &[
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(3),
    Duration::from_secs(5),
];

/// Статусы незавершённой операции.
const PENDING: &[&str] = &["scheduled", "in_progress"];

pub(crate) fn description() -> String {
    format!(
        "Wait for a background Wiki operation and return its final status. Copying a page \
         (`clone_page`), copying a grid (`clone_grid`) and moving pages (`move_page`) run in \
         the background: their response has `operation.type` and `operation.id`. Pass them as \
         `operation_type` and `operation_id`; the tool polls the matching status tool \
         (`get_clone_operation_status`, `get_clone_inline_grid_operation_status` or \
         `get_move_operation_status`) with growing intervals until the status is `success` or \
         `failed`.\n\n\
         On `success` the result is the status response (`status`, `progress`, `result`). A \
         `failed` operation is returned as an error with the status response. If the \
         operation is still running after `timeout_seconds` (default {DEFAULT_TIMEOUT_SECS}, \
         at most {MAX_TIMEOUT_SECS}), the tool returns an error with the last status; the \
         operation keeps running in the background, so call the tool again instead of \
         repeating the operation. Some MCP clients stop waiting for a tool after about a \
         minute: keep `timeout_seconds` below that."
    )
}

pub(crate) fn input_schema() -> Value {
    let types: Vec<&str> = OPERATIONS.iter().map(|(kind, _)| *kind).collect();
    json!({
        "type": "object",
        "properties": {
            "operation_type": {
                "type": "string",
                "enum": types,
                "description": "Type of the operation: the `operation.type` field of the response \
                    that started it (`clone` for `clone_page`, `clone_inline_grid` for \
                    `clone_grid`, `move` for `move_page`)."
            },
            "operation_id": {
                "type": "string",
                "description": "ID of the operation: the `operation.id` field of the response \
                    that started it."
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_TIMEOUT_SECS,
                "description": format!(
                    "How long to wait, in seconds (default {DEFAULT_TIMEOUT_SECS}, at most \
                     {MAX_TIMEOUT_SECS})."
                )
            }
        },
        "required": ["operation_type", "operation_id"],
        "additionalProperties": false
    })
}

/// Выполняет ожидание.
pub(crate) async fn run(
    client: &ApiClient,
    files: &FilePolicy,
    args: &JsonObject,
) -> Result<String, ToolError> {
    reject_unknown(args, ARGUMENTS)?;
    let kind = required_string(args, "operation_type")?;
    let Some(&(_, status_tool)) = OPERATIONS.iter().find(|(name, _)| *name == kind) else {
        let types: Vec<String> = OPERATIONS
            .iter()
            .map(|(name, _)| format!("`{name}`"))
            .collect();
        return Err(ToolError::new(format!(
            "argument `operation_type` must be one of {}, got `{kind}`",
            types.join(", ")
        )));
    };
    let id = required_string(args, "operation_id")?;
    let timeout = Duration::from_secs(parse_timeout(argument(args, "timeout_seconds"))?);
    let op = step_operation(status_tool)?;
    let status_args = object(json!({"task_id": id}));

    let started = Instant::now();
    let deadline = started + timeout;
    let mut polls: usize = 0;
    loop {
        let request = build_request(op, &status_args, files)?;
        let mut redactions = Redactions::default();
        let response = call_json(client, request, &mut redactions)
            .await
            .map_err(|err| {
                ToolError::new(format!(
                    "could not get the status of operation `{kind}` `{id}` (`{status_tool}`): {}",
                    err.message()
                ))
            })?;
        polls += 1;
        let status = response.get("status").and_then(Value::as_str);
        match status {
            Some("success") => return Ok(with_note(response.to_string(), &redactions)),
            Some("failed") => {
                return Err(ToolError::new(with_note(
                    format!("operation `{kind}` `{id}` failed: {response}"),
                    &redactions,
                )));
            }
            Some(status) if PENDING.contains(&status) => {}
            _ => {
                return Err(ToolError::new(with_note(
                    format!(
                        "`{status_tool}` returned an unexpected status for operation `{kind}` \
                         `{id}`: {response}"
                    ),
                    &redactions,
                )));
            }
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(ToolError::new(with_note(
                format!(
                    "operation `{kind}` `{id}` is not finished after {} s ({polls} status \
                     checks); it keeps running in the background. Call `{WAIT_FOR_OPERATION_TOOL}` \
                     again to keep waiting, or check `{status_tool}` later; do not repeat the \
                     operation. Last status: {response}",
                    timeout.as_secs()
                ),
                &redactions,
            )));
        }
        let delay = POLL_DELAYS
            .get(polls - 1)
            .or(POLL_DELAYS.last())
            .copied()
            .unwrap_or(Duration::from_secs(1));
        tokio::time::sleep(delay.min(deadline - now)).await;
    }
}

/// Тайм-аут в секундах: целое от 1 до [`MAX_TIMEOUT_SECS`] (число или строка из цифр).
fn parse_timeout(value: Option<&Value>) -> Result<u64, ToolError> {
    let seconds = match value {
        None => return Ok(DEFAULT_TIMEOUT_SECS),
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        Some(_) => None,
    };
    match seconds {
        Some(seconds) if (1..=MAX_TIMEOUT_SECS).contains(&seconds) => Ok(seconds),
        _ => Err(ToolError::new(format!(
            "argument `timeout_seconds` must be an integer from 1 to {MAX_TIMEOUT_SECS}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_match_the_spec() {
        for (kind, tool) in OPERATIONS {
            assert!(STEPS.contains(tool), "{tool}");
            let op = step_operation(tool).unwrap();
            assert!(
                op.path.contains(&format!("/operations/{kind}/")),
                "{}",
                op.path
            );
            assert!(op.params.iter().any(|p| p.name == "task_id"), "{tool}");
        }
        assert_eq!(STEPS.len(), OPERATIONS.len());
        let schema = input_schema();
        assert_eq!(
            schema["properties"]["operation_type"]["enum"],
            json!(["clone", "clone_inline_grid", "move"])
        );
    }

    #[test]
    fn timeout_is_bounded() {
        assert_eq!(parse_timeout(None).unwrap(), DEFAULT_TIMEOUT_SECS);
        assert_eq!(parse_timeout(Some(&json!(5))).unwrap(), 5);
        assert_eq!(parse_timeout(Some(&json!("7"))).unwrap(), 7);
        for bad in [
            json!(0),
            json!(MAX_TIMEOUT_SECS + 1),
            json!(-1),
            json!(1.5),
            json!(true),
        ] {
            assert!(parse_timeout(Some(&bad)).is_err(), "{bad}");
        }
    }
}
