//! Составной инструмент [`crate::UPLOAD_ATTACHMENT_TOOL`]: локальный файл — вложение страницы
//! одним вызовом.
//!
//! Порядок шагов: `create_upload_session` (имя и точный размер файла) →
//! `upload_file_part` для каждой части → `finish_upload_session` → `attach_uploaded_files`.
//! Если любой шаг после создания сессии не удался, сессия отменяется
//! (`abort_upload_session`): незавершённые сессии занимают квоту. Ошибка называет шаг и
//! сообщает, удалось ли отменить сессию.
//!
//! Разбивка на части ([`plan_parts`]): файл до [`MAX_FILE_BODY_BYTES`] (16 МиБ) уходит одной
//! частью — так Wiki советует загружать небольшие файлы, и это единственный способ для
//! хранилищ без многочастной загрузки. Больший файл режется на части по 16 МиБ, последняя
//! — остаток; так все части, кроме последней, не меньше 5 МиБ
//! (`wiki_api::executor::MIN_UPLOAD_PART_BYTES`) и ни одна не больше 16 МиБ. Хранилища
//! сессии, кроме `s3` и `custom_s3`, принимают файл только одним
//! запросом, поэтому файл больше 16 МиБ туда не загрузить: сессия отменяется с понятной
//! ошибкой.
//!
//! Каждая часть отправляется телом [`RequestBody::FilePart`]: исполнитель сам открывает
//! файл по проверенному каноническому пути, сверяет, что его не подменили, и читает в
//! память только эту часть. Путь проверяет [`FilePolicy::check_read`] (разрешённые
//! каталоги) до создания сессии. Если файл изменил размер во время загрузки, сессия
//! отменяется.

use std::path::Path;

use rmcp::model::JsonObject;
use serde_json::{Value, json};
use wiki_api::executor::{ApiClient, ApiRequest, MAX_FILE_BODY_BYTES, RequestBody, render_path};

use crate::composite::{
    call_json, object, optional_string, reject_unknown, required, required_string, step_operation,
};
use crate::error::ToolError;
use crate::files::FilePolicy;
use crate::redact::Redactions;
use crate::request::build_request;
use crate::server::with_note;

const CREATE: &str = "create_upload_session";
const UPLOAD_PART: &str = "upload_file_part";
const FINISH: &str = "finish_upload_session";
const ABORT: &str = "abort_upload_session";
const ATTACH: &str = "attach_uploaded_files";

/// Операции шагов.
pub(crate) const STEPS: &[&str] = &[CREATE, UPLOAD_PART, FINISH, ATTACH, ABORT];

const ARGUMENTS: &[&str] = &["idx", "file", "file_name"];

/// Размер части большого файла.
pub const PART_BYTES: u64 = MAX_FILE_BODY_BYTES;

/// Наибольший номер части в сессии загрузки Wiki.
pub const MAX_PARTS: u64 = 10_000;

/// Хранилища, которые принимают файл частями (`storage_type` сессии).
const MULTIPART_STORAGES: &[&str] = &["s3", "custom_s3"];

const MIB: u64 = 1024 * 1024;

/// Часть файла: номер (с 1), смещение и длина в байтах.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Part {
    pub number: u64,
    pub offset: u64,
    pub len: u64,
}

/// Части файла размером `size` (см. описание модуля): до [`MAX_FILE_BODY_BYTES`] — одна
/// часть, иначе части по [`PART_BYTES`], последняя — остаток. Пустой файл — без частей.
pub fn plan_parts(size: u64) -> Vec<Part> {
    let part_bytes = if size <= MAX_FILE_BODY_BYTES {
        size
    } else {
        PART_BYTES
    };
    let mut parts = Vec::new();
    let mut offset = 0;
    while offset < size {
        let len = part_bytes.min(size - offset);
        parts.push(Part {
            number: parts.len() as u64 + 1,
            offset,
            len,
        });
        offset += len;
    }
    parts
}

pub(crate) fn description() -> String {
    format!(
        "Upload a local file and attach it to a page in one call. The tool creates an upload \
         session (`{CREATE}`), uploads the file (`{UPLOAD_PART}`: a file of at most \
         {max} MiB as one part, a larger one in parts of {max} MiB), finishes the session \
         (`{FINISH}`) and attaches the file to the page (`{ATTACH}`). If a step fails, the \
         upload session is aborted (`{ABORT}`) and the error names the step; nothing is \
         attached then. Calling the tool again attaches one more copy of the file.\n\n\
         The result is the response of `{ATTACH}` (`results` with the attachment, including \
         `download_url`) plus `upload` (session ID, file name, size and number of parts). The \
         file does not appear in the page text by itself: to show it, add \
         `{{% file src=\"<download_url>\" name=\"<name>\" %}}` with `append_page_content`.\n\n\
         `file` takes a path to a local file (absolute, or relative to the server's working \
         directory) inside the directories allowed by the server (see the server \
         instructions). Empty files are rejected.",
        max = MAX_FILE_BODY_BYTES / MIB,
    )
}

pub(crate) fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "idx": {
                "type": "integer",
                "description": "Numeric ID of the page to attach the file to: the `id` field of \
                    `get_page`, `get_page_by_id` or `create_page`. Not a slug or a URL."
            },
            "file": {
                "type": "string",
                "description": "Path to the local file to upload (absolute, or relative to the \
                    server's working directory), inside the allowed directories."
            },
            "file_name": {
                "type": "string",
                "description": "Name of the attachment in Wiki. Defaults to the name of the \
                    local file."
            }
        },
        "required": ["idx", "file"],
        "additionalProperties": false
    })
}

/// Выполняет загрузку.
pub(crate) async fn run(
    client: &ApiClient,
    files: &FilePolicy,
    args: &JsonObject,
) -> Result<String, ToolError> {
    reject_unknown(args, ARGUMENTS)?;
    let idx = required(args, "idx")?;
    let file_arg = required_string(args, "file")?;
    // Запрос прикрепления собирается заранее: неверный `idx` (не число) отклоняется до
    // создания сессии и загрузки файла. Есть ли такая страница, скажет только Wiki на
    // последнем шаге — тогда сессия отменяется.
    let mut attach = build_request(
        step_operation(ATTACH)?,
        &object(json!({"idx": idx, "body": {"upload_sessions": []}})),
        files,
    )?;
    let path = files
        .check_read(file_arg)
        .map_err(|err| ToolError::new(format!("argument `file`: {err}")))?;
    let file_name = match optional_string(args, "file_name")? {
        Some(name) => name.to_owned(),
        None => default_file_name(file_arg, &path)?,
    };
    let size = file_size(&path).await?;
    if size == 0 {
        return Err(ToolError::new(format!(
            "local file `{file_arg}` is empty: Wiki does not accept empty files, nothing was \
             uploaded"
        )));
    }
    let parts = plan_parts(size);
    if parts.len() as u64 > MAX_PARTS {
        return Err(ToolError::new(format!(
            "local file `{file_arg}` has {size} bytes: more than {MAX_PARTS} parts of {} MiB, \
             the limit of a Wiki upload session; nothing was uploaded",
            PART_BYTES / MIB
        )));
    }

    let mut redactions = Redactions::default();
    let create = build_request(
        step_operation(CREATE)?,
        &object(json!({"body": {"file_name": file_name, "file_size": size}})),
        files,
    )?;
    let session = call_json(client, create, &mut redactions)
        .await
        .map_err(|err| step_failed(CREATE, &err, None))?;
    let Some(session_id) = session
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
    else {
        return Err(ToolError::new(format!(
            "step `{CREATE}` returned no `session_id`, so nothing was uploaded: {session}"
        )));
    };
    let storage = session
        .get("storage_type")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let upload = Upload {
        client,
        files,
        session_id: &session_id,
        path: &path,
        size,
    };
    let attached = match upload
        .send(&parts, storage.as_deref(), &mut attach, &mut redactions)
        .await
    {
        Ok(attached) => attached,
        Err(err) => {
            let aborted = upload.abort().await;
            return Err(ToolError::new(format!(
                "{}\n\n{}",
                err.message(),
                abort_report(&session_id, aborted)
            )));
        }
    };

    let summary = json!({
        "session_id": session_id,
        "file_name": file_name,
        "file_size": size,
        "parts": parts.len(),
    });
    let result = match attached {
        Value::Object(mut map) => {
            map.insert("upload".to_owned(), summary);
            Value::Object(map)
        }
        other => json!({"response": other, "upload": summary}),
    };
    Ok(with_note(result.to_string(), &redactions))
}

/// Загрузка в созданную сессию.
struct Upload<'a> {
    client: &'a ApiClient,
    files: &'a FilePolicy,
    session_id: &'a str,
    /// Канонический путь, проверенный [`FilePolicy::check_read`].
    path: &'a Path,
    size: u64,
}

impl Upload<'_> {
    /// Части, завершение и прикрепление; ответ прикрепления.
    async fn send(
        &self,
        parts: &[Part],
        storage: Option<&str>,
        attach: &mut ApiRequest,
        redactions: &mut Redactions,
    ) -> Result<Value, ToolError> {
        if parts.len() > 1
            && let Some(storage) = storage.filter(|s| !MULTIPART_STORAGES.contains(s))
        {
            return Err(ToolError::new(format!(
                "the file has {} bytes, but the upload session uses storage `{storage}`, which \
                 accepts a file only in one request of at most {} MiB; nothing was uploaded",
                self.size,
                MAX_FILE_BODY_BYTES / MIB
            )));
        }
        let total = parts.len();
        for part in parts {
            let request = self.part_request(part)?;
            call_json(self.client, request, redactions)
                .await
                .map_err(|err| {
                    let context = format!("part {} of {total}", part.number);
                    step_failed(UPLOAD_PART, &err, Some(&context))
                })?;
        }
        // Файл, изменивший размер во время загрузки, в Wiki не нужен: части от разных версий.
        let now = file_size(self.path).await?;
        if now != self.size {
            return Err(ToolError::new(format!(
                "the local file changed during the upload ({} bytes before, {now} bytes now); \
                 nothing was attached",
                self.size
            )));
        }
        let finish = build_request(
            step_operation(FINISH)?,
            &object(json!({"session_id": self.session_id})),
            self.files,
        )?;
        call_json(self.client, finish, redactions)
            .await
            .map_err(|err| step_failed(FINISH, &err, None))?;
        attach.body = RequestBody::Json(json!({"upload_sessions": [self.session_id]}));
        call_json(self.client, attach.clone(), redactions)
            .await
            .map_err(|err| step_failed(ATTACH, &err, None))
    }

    /// Запрос загрузки части: путь и метод — из операции, тело — диапазон файла.
    fn part_request(&self, part: &Part) -> Result<ApiRequest, ToolError> {
        let op = step_operation(UPLOAD_PART)?;
        let path = render_path(op.path, |name| {
            (name == "session_id").then(|| self.session_id.to_owned())
        })?;
        let mut request = ApiRequest::new(op.method, path);
        request.query = vec![("part_number".to_owned(), part.number.to_string())];
        request.body = RequestBody::FilePart {
            path: self.path.to_path_buf(),
            offset: part.offset,
            len: part.len,
        };
        Ok(request)
    }

    /// Отменяет сессию; ответ отмены агенту не нужен.
    async fn abort(&self) -> Result<(), ToolError> {
        let request = build_request(
            step_operation(ABORT)?,
            &object(json!({"session_id": self.session_id})),
            self.files,
        )?;
        let mut ignored = Redactions::default();
        call_json(self.client, request, &mut ignored)
            .await
            .map(drop)
    }
}

/// Ошибка шага: какой шаг и что ответил Wiki.
fn step_failed(step: &str, err: &ToolError, context: Option<&str>) -> ToolError {
    let context = context.map(|c| format!(" ({c})")).unwrap_or_default();
    ToolError::new(format!(
        "upload failed at step `{step}`{context}: {}",
        err.message()
    ))
}

/// Итог отмены сессии для текста ошибки.
fn abort_report(session_id: &str, aborted: Result<(), ToolError>) -> String {
    match aborted {
        Ok(()) => format!("The upload session `{session_id}` was aborted."),
        Err(err) => format!(
            "Aborting the upload session `{session_id}` failed too: {} Abort it with \
             `{ABORT}` (or `abort_all_upload_sessions`) when the API is available.",
            err.message()
        ),
    }
}

/// Имя файла по умолчанию: последний компонент пути, как его передал агент (у ссылки внутри
/// разрешённых каталогов — имя ссылки), иначе — канонического пути.
fn default_file_name(arg: &str, canonical: &Path) -> Result<String, ToolError> {
    Path::new(arg)
        .file_name()
        .or_else(|| canonical.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            ToolError::new(format!(
                "cannot take a file name from `{arg}`; pass `file_name`"
            ))
        })
}

async fn file_size(path: &Path) -> Result<u64, ToolError> {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.len())
        .map_err(|err| {
            tracing::warn!(%err, "не удалось прочитать размер файла");
            ToolError::new(
                "local file error: the file could not be read; check the path and the \
                 permissions",
            )
        })
}

#[cfg(test)]
mod tests {
    use wiki_api::executor::MIN_UPLOAD_PART_BYTES;

    use super::*;

    fn sizes(parts: &[Part]) -> Vec<u64> {
        parts.iter().map(|p| p.len).collect()
    }

    #[test]
    fn small_files_are_one_part() {
        assert!(plan_parts(0).is_empty());
        assert_eq!(
            plan_parts(1),
            [Part {
                number: 1,
                offset: 0,
                len: 1
            }]
        );
        assert_eq!(sizes(&plan_parts(11 * MIB)), [11 * MIB]);
        assert_eq!(
            sizes(&plan_parts(MAX_FILE_BODY_BYTES)),
            [MAX_FILE_BODY_BYTES]
        );
    }

    #[test]
    fn large_files_are_split_within_limits() {
        for size in [
            MAX_FILE_BODY_BYTES + 1,
            2 * MAX_FILE_BODY_BYTES,
            2 * MAX_FILE_BODY_BYTES + 3,
            100 * MIB + 7,
        ] {
            let parts = plan_parts(size);
            assert!(parts.len() > 1, "{size}");
            assert_eq!(parts.iter().map(|p| p.len).sum::<u64>(), size);
            let mut offset = 0;
            for (i, part) in parts.iter().enumerate() {
                assert_eq!(part.number, i as u64 + 1);
                assert_eq!(part.offset, offset);
                assert!(part.len >= 1 && part.len <= MAX_FILE_BODY_BYTES, "{part:?}");
                if i + 1 < parts.len() {
                    assert!(part.len >= MIN_UPLOAD_PART_BYTES, "{part:?}");
                }
                offset += part.len;
            }
        }
        assert_eq!(
            sizes(&plan_parts(2 * MAX_FILE_BODY_BYTES + 3)),
            [MAX_FILE_BODY_BYTES, MAX_FILE_BODY_BYTES, 3]
        );
    }

    #[test]
    fn default_name_comes_from_the_argument() {
        let canonical = Path::new("/tmp/real.pdf");
        assert_eq!(
            default_file_name("docs/link.pdf", canonical).unwrap(),
            "link.pdf"
        );
        assert_eq!(default_file_name("docs/..", canonical).unwrap(), "real.pdf");
    }

    #[test]
    fn description_names_steps_and_limits() {
        let text = description();
        for step in STEPS {
            assert!(text.contains(step), "{step}: {text}");
        }
        assert!(text.contains("16 MiB"), "{text}");
        assert!(text.contains("{% file src="), "{text}");
    }
}
