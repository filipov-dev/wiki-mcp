//! Представление ответа API в результате инструмента.
//!
//! - JSON — компактный JSON ответа как есть: разбивка выдачи Wiki (`next_cursor`,
//!   `prev_cursor`) уже в теле ответа, обёртка не нужна;
//! - текст — как есть;
//! - сохранённый файл — `{"saved_to", "bytes", "content_type"}`;
//! - пустое тело — `{"status"}`.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;
use wiki_api::executor::{ApiResponse, ResponseBody};

use crate::error::ToolError;

/// Текст успешного ответа (см. описание модуля).
pub fn response_text(response: &ApiResponse) -> String {
    match &response.body {
        ResponseBody::Json(value) => value.to_string(),
        ResponseBody::Text(text) => text.clone(),
        ResponseBody::SavedToFile {
            path,
            bytes,
            content_type,
        } => json!({
            "saved_to": path.display().to_string(),
            "bytes": bytes,
            "content_type": content_type,
        })
        .to_string(),
        ResponseBody::Empty => json!({"status": response.status}).to_string(),
    }
}

/// Успешный результат инструмента.
pub fn success(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Результат инструмента с `isError: true`.
pub fn failure(err: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.message())])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn response(body: ResponseBody) -> ApiResponse {
        ApiResponse { status: 204, body }
    }

    #[test]
    fn formats_bodies() {
        assert_eq!(
            response_text(&response(ResponseBody::Json(
                json!({"results": [1, 2], "next_cursor": "c2"})
            ))),
            r#"{"next_cursor":"c2","results":[1,2]}"#
        );
        assert_eq!(
            response_text(&response(ResponseBody::Text("plain".into()))),
            "plain"
        );
        assert_eq!(
            response_text(&response(ResponseBody::Empty)),
            r#"{"status":204}"#
        );
        let saved = response_text(&response(ResponseBody::SavedToFile {
            path: PathBuf::from("/tmp/x.bin"),
            bytes: 42,
            content_type: Some("image/png".into()),
        }));
        assert_eq!(
            saved,
            r#"{"bytes":42,"content_type":"image/png","saved_to":"/tmp/x.bin"}"#
        );
    }

    #[test]
    fn error_result_is_flagged() {
        let result = failure(&ToolError::new("boom"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(success("ok".into()).is_error, Some(false));
    }
}
