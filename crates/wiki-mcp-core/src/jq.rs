//! Фильтр ответа в стиле jq: необязательный аргумент `jq` у инструментов операций,
//! `api_request` и пользовательских инструментов.
//!
//! Порядок обработки ответа (см. `execute` в [`crate::server`]): вырезание учётных данных →
//! фильтр → лимит размера. Фильтр получает уже очищенный ответ, поэтому никакой фильтр
//! (например, `.. | strings`) не видит вырезанных значений.
//!
//! Фильтр применяется к телу ответа Wiki целиком. Курсоры постраничной выдачи Wiki
//! (`next_cursor`, `prev_cursor`) лежат в самом теле, поэтому фильтр их видит, но обычно
//! отбрасывает (`.results[] | {id, title}`). Чтобы агент не терял следующую страницу, у
//! ответа с курсорами результат фильтра оборачивается: `{"result": <результат фильтра>,
//! "pagination": {"next_cursor": ..., "prev_cursor": ...}}` — одинаково на любой странице.
//!
//! Фильтры выполняет [jaq](https://github.com/01mf02/jaq). Защита от патологических фильтров:
//!
//! - длина фильтра ограничена [`MAX_FILTER_LEN`]; синтаксис и имена проверяются до запроса к
//!   API ([`JqFilter::parse`]), чтобы ошибка в фильтре не появлялась после выполненной записи;
//! - из стандартной библиотеки убраны `env` (окружение процесса) и вывод в журнал
//!   (`debug`, `stderr` становятся тождественными), `input`/`inputs` и `$ENV` не определены,
//!   модули с диска не загружаются; `halt` превращается в ошибку фильтра;
//! - размер результата ограничен: вывод прекращается, как только превышен лимит;
//! - выполнение ограничено по времени ([`JqRunner`]). В рабочем режиме сервера фильтр
//!   выполняется в отдельном процессе с пустым окружением (скрытая подкоманда бинарника,
//!   [`run_worker`]): бесконечная рекурсия, переполняющая стек, или зацикливание завершают
//!   только этот процесс, а сервер возвращает управляемую ошибку.
//!
//! Несколько результатов фильтра возвращаются JSON-массивом, ни одного — пустым массивом,
//! ровно один — как есть.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use jaq_core::load::{self, Arena, File, Loader};
use jaq_core::native::{Fun, v};
use jaq_core::{Compiler, Ctx, Native, Vars, compile, data};
use jaq_json::Val;
use rmcp::model::JsonObject;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiki_api::spec::OperationDef;

use crate::error::ToolError;
use crate::request::takes_save_to;

/// Имя аргумента инструментов.
pub const JQ_ARG: &str = "jq";

/// Максимальная длина фильтра в байтах.
pub const MAX_FILTER_LEN: usize = 4096;

/// Таймаут выполнения фильтра по умолчанию.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// Сколько байт ответа API читается, когда задан фильтр. Лимит ответа сервера
/// (`--max-response-bytes`) применяется к результату фильтра, а не к исходному ответу:
/// фильтр как раз и нужен, чтобы из большого ответа вернуть агенту малую часть.
pub const FILTER_INPUT_LIMIT: usize = 16 * 1024 * 1024;

/// Стек потока, в котором разбирается и выполняется фильтр. Разбор и вычисление jaq
/// рекурсивны; стек основного потока (1 МиБ в Windows) и рабочих потоков tokio (2 МиБ)
/// для этого мал. Память выделяется по мере использования.
const STACK_SIZE: usize = 256 * 1024 * 1024;

/// Максимальная длина сообщения об ошибке фильтра: `error(.)` может включить в сообщение
/// весь ответ.
const MAX_ERROR_LEN: usize = 1000;

/// Нативные функции jaq, которые фильтру недоступны: `env` раскрывает окружение процесса
/// сервера. `debug_empty` и `stderr_empty` (основа `debug` и `stderr`) заменяются
/// функциями без вывода (см. [`funs`]): stderr сервера — журнал, а не канал для данных.
const EXCLUDED_FUNS: &[&str] = &["env", "debug_empty", "stderr_empty"];

/// Описание аргумента `jq` в схеме инструмента. Подробности — в инструкциях сервера,
/// чтобы не повторять длинный текст в ~200 инструментах.
const ARG_DESCRIPTION: &str = "Optional jq filter for the JSON response body, e.g. \
    `.results[] | {id, title}` (see server instructions).";

/// Принимает ли инструмент операции аргумент `jq`: у операций с двоичным ответом (файл
/// сохраняется в `save_to`) фильтровать нечего.
pub fn takes_jq(op: &OperationDef) -> bool {
    !takes_save_to(op)
}

/// Добавляет аргумент `jq` в схему инструмента.
pub fn add_jq_argument(schema: &mut JsonObject) {
    if let Some(Value::Object(properties)) = schema.get_mut("properties") {
        properties.insert(
            JQ_ARG.to_owned(),
            json!({"type": "string", "description": ARG_DESCRIPTION}),
        );
    }
}

/// Проверенный фильтр: синтаксис и имена корректны, длина в пределах лимита.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JqFilter(String);

impl JqFilter {
    /// Проверяет фильтр (разбор и компиляция, без выполнения).
    pub fn parse(code: &str) -> Result<Self, ToolError> {
        let code = code.trim();
        if code.is_empty() {
            return Err(ToolError::new("argument `jq`: the filter is empty"));
        }
        if code.len() > MAX_FILTER_LEN {
            return Err(ToolError::new(format!(
                "argument `jq`: the filter is longer than {MAX_FILTER_LEN} bytes"
            )));
        }
        let owned = code.to_owned();
        on_big_stack(move || compile_filter(&owned).map(|_| ()))
            .and_then(|result| result)
            .map_err(|err| ToolError::new(format!("argument `jq`: {err}")))?;
        Ok(Self(code.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Извлекает и проверяет аргумент `jq`. `null` равен отсутствию аргумента.
pub fn take_jq(args: &mut JsonObject) -> Result<Option<JqFilter>, ToolError> {
    match args.remove(JQ_ARG) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(code)) => JqFilter::parse(&code).map(Some),
        Some(_) => Err(ToolError::new(
            "argument `jq` must be a string with a jq filter",
        )),
    }
}

/// Результат фильтра: компактный JSON (обрезанный, если превышен лимит).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterOutput {
    pub text: String,
    /// Результат превысил лимит и обрезан; `text` тогда не является валидным JSON.
    pub truncated: bool,
}

/// Как выполняются фильтры.
#[derive(Debug, Clone)]
pub struct JqRunner {
    worker: Option<WorkerCommand>,
    timeout: Duration,
}

#[derive(Debug, Clone)]
struct WorkerCommand {
    program: PathBuf,
    args: Vec<String>,
}

impl Default for JqRunner {
    fn default() -> Self {
        Self::in_process()
    }
}

impl JqRunner {
    /// Выполнение в отдельном потоке этого процесса: для тестов и встраивания.
    ///
    /// Ограничение: по таймауту поток не останавливается (jaq нельзя прервать), а
    /// переполнение стека бесконечной рекурсией аварийно завершает весь процесс.
    /// Сервер `wiki-mcp serve` использует [`Self::subprocess`].
    pub fn in_process() -> Self {
        Self {
            worker: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// Выполнение в дочернем процессе `program args...`, который обслуживает один запрос
    /// через [`run_worker`]. Процесс запускается с пустым окружением и завершается по
    /// таймауту.
    pub fn subprocess(
        program: impl Into<PathBuf>,
        args: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            worker: Some(WorkerCommand {
                program: program.into(),
                args: args.into_iter().map(Into::into).collect(),
            }),
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Применяет фильтр к `input`. Ошибка — текст для агента (без префикса `jq`).
    pub async fn run(
        &self,
        filter: &JqFilter,
        input: Value,
        max_output_bytes: usize,
    ) -> Result<FilterOutput, String> {
        let request = WorkerRequest {
            filter: filter.as_str().to_owned(),
            input,
            max_output_bytes,
        };
        match &self.worker {
            Some(command) => run_subprocess(command, &request, self.timeout).await,
            None => run_in_thread(request, self.timeout).await,
        }
    }
}

/// Запрос к процессу-исполнителю (stdin).
#[derive(Debug, Serialize, Deserialize)]
struct WorkerRequest {
    filter: String,
    input: Value,
    max_output_bytes: usize,
}

/// Ответ процесса-исполнителя (stdout).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkerResponse {
    Output(FilterOutput),
    Error(String),
}

/// Точка входа процесса-исполнителя: читает [`WorkerRequest`] из `input`, пишет
/// [`WorkerResponse`] в `output`. Вызывается скрытой подкомандой бинарника.
pub fn run_worker(mut input: impl Read, mut output: impl Write) -> std::io::Result<()> {
    let mut buf = Vec::new();
    input.read_to_end(&mut buf)?;
    let response = match serde_json::from_slice::<WorkerRequest>(&buf) {
        Ok(request) => evaluate_on_big_stack(request),
        Err(err) => Err(format!("invalid filter request: {err}")),
    };
    let response = match response {
        Ok(out) => WorkerResponse::Output(out),
        Err(err) => WorkerResponse::Error(err),
    };
    serde_json::to_writer(&mut output, &response).map_err(std::io::Error::other)?;
    output.flush()
}

async fn run_in_thread(request: WorkerRequest, timeout: Duration) -> Result<FilterOutput, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("jq-filter".into())
        .stack_size(STACK_SIZE)
        .spawn(move || {
            let _ = tx.send(evaluate(request));
        })
        .map_err(|err| format!("cannot start the filter: {err}"))?;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("the filter crashed".into()),
        Err(_) => Err(timeout_message(timeout)),
    }
}

async fn run_subprocess(
    command: &WorkerCommand,
    request: &WorkerRequest,
    timeout: Duration,
) -> Result<FilterOutput, String> {
    let payload =
        serde_json::to_vec(request).map_err(|err| format!("cannot encode the input: {err}"))?;
    let mut cmd = tokio::process::Command::new(&command.program);
    cmd.args(&command.args)
        // Исполнителю ничего не нужно из окружения сервера.
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Без SystemRoot в Windows не инициализируются системные библиотеки.
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        cmd.env("SystemRoot", root);
    }
    let mut child = cmd
        .spawn()
        .map_err(|err| format!("cannot start the filter process: {err}"))?;

    // Ответ исполнителя — JSON-строка с результатом; экранирование может увеличить её
    // до 6 раз. Больше не читаем.
    let cap = request
        .max_output_bytes
        .saturating_mul(6)
        .saturating_add(64 * 1024);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let exchange = async {
        let write = async {
            if let Some(mut stdin) = stdin {
                // Ошибка записи (исполнитель завершился раньше) видна по его статусу.
                let _ = stdin.write_all(&payload).await;
                let _ = stdin.shutdown().await;
            }
        };
        let read = async {
            let mut out = Vec::new();
            if let Some(stdout) = stdout {
                let _ = stdout.take(cap as u64).read_to_end(&mut out).await;
            }
            out
        };
        let ((), out) = tokio::join!(write, read);
        let status = child.wait().await;
        (out, status)
    };
    let (out, status) = match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) => {
            let _ = child.start_kill();
            return Err(timeout_message(timeout));
        }
    };
    match serde_json::from_slice::<WorkerResponse>(&out) {
        Ok(WorkerResponse::Output(output)) => Ok(output),
        Ok(WorkerResponse::Error(err)) => Err(err),
        Err(_) => {
            let status = status.map_err(|err| format!("the filter process failed: {err}"))?;
            tracing::debug!(%status, "процесс jq-фильтра завершился без ответа");
            Err(
                "the filter process crashed (for example, because of unbounded recursion); \
                 simplify the filter"
                    .into(),
            )
        }
    }
}

fn timeout_message(timeout: Duration) -> String {
    format!(
        "the filter did not finish within {} s; simplify it",
        timeout.as_secs_f64()
    )
}

/// Выполняет `f` в потоке с большим стеком. Паника в `f` — ошибка, а не аварийное
/// завершение (например, внутренние `panic!` jaq на неожиданных данных).
fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    std::thread::Builder::new()
        .name("jq-filter".into())
        .stack_size(STACK_SIZE)
        .spawn(f)
        .map_err(|err| format!("cannot start the filter: {err}"))?
        .join()
        .map_err(|_| "the filter crashed".to_owned())
}

fn evaluate_on_big_stack(request: WorkerRequest) -> Result<FilterOutput, String> {
    on_big_stack(move || evaluate(request)).and_then(|result| result)
}

type Data = data::JustLut<Val>;

/// Нативные функции: стандартная библиотека без [`EXCLUDED_FUNS`] и тихие `debug_empty`,
/// `stderr_empty`.
fn funs() -> impl Iterator<Item = Fun<Data>> {
    let silent: [Fun<Data>; 2] = [
        ("debug_empty", v(0), silent()),
        ("stderr_empty", v(0), silent()),
    ];
    jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs())
        .filter(|(name, _, _)| !EXCLUDED_FUNS.contains(name))
        .chain(silent)
}

fn silent() -> Native<Data> {
    Native::new(|_| Box::new(std::iter::empty()))
}

fn compile_filter(code: &str) -> Result<jaq_core::Filter<Data>, String> {
    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    // Загрузчик без путей поиска: `import`/`include` не читают файлы.
    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader
        .load(&arena, File { code, path: () })
        .map_err(|errs| describe_load_errors(&errs))?;
    Compiler::default()
        .with_funs(funs())
        .compile(modules)
        .map_err(|errs| describe_compile_errors(&errs))
}

fn evaluate(request: WorkerRequest) -> Result<FilterOutput, String> {
    let filter = compile_filter(&request.filter)?;
    let input: Val = serde_json::from_value(request.input)
        .map_err(|err| format!("cannot convert the response: {err}"))?;
    let ctx = Ctx::<Data>::new(&filter.lut, Vars::new([]));
    let mut writer = OutputWriter::new(request.max_output_bytes);
    for item in filter.id.run((ctx, input)) {
        match item {
            Ok(value) => {
                if !writer.push(&value) {
                    break;
                }
            }
            // `unwrap_valr` из jaq не используем: на `halt` он завершает процесс.
            Err(exn) => {
                return Err(match exn.get_err() {
                    Ok(err) => shorten(format!("runtime error: {err}")),
                    Err(exn) => match exn.get_halt() {
                        Ok(code) => format!("`halt` is not supported (exit code {code})"),
                        Err(_) => "internal filter error".to_owned(),
                    },
                });
            }
        }
    }
    Ok(writer.finish())
}

/// Собирает результаты в компактный JSON с ограничением размера.
struct OutputWriter {
    max: usize,
    /// Результаты через запятую.
    buf: Vec<u8>,
    count: usize,
    truncated: bool,
}

impl OutputWriter {
    fn new(max: usize) -> Self {
        Self {
            max,
            buf: Vec::new(),
            count: 0,
            truncated: false,
        }
    }

    /// Добавляет результат; `false` — лимит превышен, дальше результаты не нужны.
    fn push(&mut self, value: &Val) -> bool {
        if self.count > 0 {
            self.buf.push(b',');
        }
        self.buf.extend_from_slice(value.to_string().as_bytes());
        self.count += 1;
        // +2 — скобки массива.
        if self.buf.len() + 2 > self.max {
            self.truncated = true;
            return false;
        }
        true
    }

    fn finish(self) -> FilterOutput {
        let mut text = if self.count == 1 && !self.truncated {
            String::from_utf8_lossy(&self.buf).into_owned()
        } else {
            let mut text = String::with_capacity(self.buf.len() + 2);
            text.push('[');
            text.push_str(&String::from_utf8_lossy(&self.buf));
            if !self.truncated {
                text.push(']');
            }
            text
        };
        let truncated = truncate_text(&mut text, self.max) || self.truncated;
        FilterOutput { text, truncated }
    }
}

/// Обрезает строку до `max` байт по границе символа. `true` — строка была длиннее.
pub fn truncate_text(text: &mut String, max: usize) -> bool {
    if text.len() <= max {
        return false;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

fn shorten(mut text: String) -> String {
    if truncate_text(&mut text, MAX_ERROR_LEN) {
        text.push('…');
    }
    text
}

/// Фрагмент фильтра, на котором случилась ошибка разбора (остаток входа).
fn near(rest: &str) -> String {
    let rest = rest.trim();
    if rest.is_empty() {
        return "at the end of the filter".to_owned();
    }
    let snippet: String = rest.chars().take(30).collect();
    format!("near `{snippet}`")
}

fn describe_load_errors(errs: &load::Errors<&str, ()>) -> String {
    let mut messages = Vec::new();
    for (_, err) in errs {
        match err {
            load::Error::Io(items) => {
                for (path, _) in items {
                    messages.push(format!(
                        "cannot load module `{path}`: modules are not supported"
                    ));
                }
            }
            load::Error::Lex(items) => {
                for (expect, rest) in items {
                    let what = match expect {
                        load::lex::Expect::Digit => "a digit",
                        load::lex::Expect::Ident => "an identifier",
                        load::lex::Expect::Delim("(") => "a closing parenthesis",
                        load::lex::Expect::Delim("[") => "a closing bracket",
                        load::lex::Expect::Delim("{") => "a closing brace",
                        load::lex::Expect::Delim("\"") => "a closing quote",
                        load::lex::Expect::Delim(_) => "a closing delimiter",
                        load::lex::Expect::Escape => "a string escape sequence",
                        load::lex::Expect::Unicode => "a 4-digit hexadecimal code point",
                        load::lex::Expect::Token => "a valid token",
                        _ => "a valid token",
                    };
                    messages.push(format!("syntax error: expected {what} {}", near(rest)));
                }
            }
            load::Error::Parse(items) => {
                for (expect, rest) in items {
                    messages.push(format!(
                        "syntax error: expected {} {}",
                        expect.as_str(),
                        near(rest)
                    ));
                }
            }
        }
    }
    if messages.is_empty() {
        messages.push("invalid filter".to_owned());
    }
    shorten(messages.join("; "))
}

fn describe_compile_errors(errs: &compile::Errors<&str, ()>) -> String {
    let mut messages = Vec::new();
    for (_, items) in errs {
        for (name, undefined) in items {
            let detail = match undefined {
                compile::Undefined::Filter(arity) => format!("filter `{name}/{arity}`"),
                other => format!("{} `{name}`", other.as_str()),
            };
            messages.push(format!("undefined {detail}"));
        }
    }
    if messages.is_empty() {
        messages.push("invalid filter".to_owned());
    }
    shorten(messages.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(code: &str) -> JqFilter {
        JqFilter::parse(code).unwrap()
    }

    async fn run(code: &str, input: Value, max: usize) -> Result<FilterOutput, String> {
        JqRunner::in_process().run(&filter(code), input, max).await
    }

    fn parse_err(code: &str) -> String {
        JqFilter::parse(code).unwrap_err().message().to_owned()
    }

    #[tokio::test]
    async fn selects_fields() {
        let input = json!([
            {"slug": "team/a", "title": "a", "content": "long"},
            {"slug": "team/b", "title": "b", "content": "long"},
        ]);
        let out = run(".[] | {slug, title}", input.clone(), 1024)
            .await
            .unwrap();
        assert_eq!(
            out,
            FilterOutput {
                text: r#"[{"slug":"team/a","title":"a"},{"slug":"team/b","title":"b"}]"#.into(),
                truncated: false
            }
        );
        let one = run(".[0].slug", input.clone(), 1024).await.unwrap();
        assert_eq!(one.text, r#""team/a""#);
        let none = run(".[] | select(.slug == \"team/c\")", input, 1024)
            .await
            .unwrap();
        assert_eq!(none.text, "[]");
    }

    #[tokio::test]
    async fn limits_output_size() {
        let out = run("range(100000)", json!(null), 64).await.unwrap();
        assert!(out.truncated);
        assert!(out.text.len() <= 64, "{}", out.text);
        assert!(out.text.starts_with("[0,1,2"));

        let out = run(".", json!({"s": "ж".repeat(100)}), 51).await.unwrap();
        assert!(out.truncated);
        assert!(out.text.len() <= 51);

        // Бесконечный, но ленивый поток останавливается на лимите.
        let out = run("repeat(1)", json!(null), 32).await.unwrap();
        assert!(out.truncated);
    }

    #[test]
    fn rejects_bad_filters_before_running() {
        assert!(
            parse_err(".a |").contains("syntax error"),
            "{}",
            parse_err(".a |")
        );
        assert!(parse_err("{a:").contains("syntax error"));
        assert!(parse_err("nosuchfn").contains("undefined filter `nosuchfn/0`"));
        assert!(parse_err("").contains("empty"));
        assert!(parse_err(&".a".repeat(MAX_FILTER_LEN)).contains("longer than"));
        // Окружение процесса и входные потоки недоступны.
        assert!(parse_err("env").contains("undefined filter `env/0`"));
        assert!(parse_err("$ENV").contains("undefined variable `$ENV`"));
        assert!(parse_err("input").contains("undefined filter `input/0`"));
        assert!(parse_err("inputs").contains("undefined filter `inputs/0`"));
        assert!(parse_err("include \"x\"; .").contains("module"));
        assert!(parse_err("import \"x\" as x; .").contains("module"));
        // Глубокая вложенность разбирается без переполнения стека.
        let deep = format!("{}.{}", "[".repeat(2000), "]".repeat(2000));
        assert!(JqFilter::parse(&deep).is_ok());
    }

    #[tokio::test]
    async fn runtime_errors_are_reported() {
        let err = run(".a[] | {x}", json!({"a": [1]}), 1024)
            .await
            .unwrap_err();
        assert!(err.contains("runtime error: cannot index 1"), "{err}");
        let err = run("error(\"boom\")", json!(null), 1024).await.unwrap_err();
        assert!(err.contains("boom"), "{err}");
        let err = run("halt", json!(null), 1024).await.unwrap_err();
        assert!(err.contains("`halt` is not supported"), "{err}");
        let err = run("halt_error", json!("x"), 1024).await.unwrap_err();
        assert!(err.contains("`halt` is not supported"), "{err}");
        let err = run("error(.)", json!("x".repeat(10_000)), 100_000)
            .await
            .unwrap_err();
        assert!(err.len() < MAX_ERROR_LEN + 10, "{}", err.len());
    }

    #[tokio::test]
    async fn debug_is_silent() {
        let out = run("debug | stderr | .a", json!({"a": 1}), 1024)
            .await
            .unwrap();
        assert_eq!(out.text, "1");
    }

    #[tokio::test]
    async fn in_process_timeout() {
        let runner = JqRunner::in_process().with_timeout(Duration::from_millis(100));
        let err = runner
            .run(&filter("last(range(1e15))"), json!(null), 1024)
            .await
            .unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
    }

    #[test]
    fn worker_protocol() {
        let request = json!({"filter": ".a", "input": {"a": [1, 2]}, "max_output_bytes": 100});
        let mut out = Vec::new();
        run_worker(request.to_string().as_bytes(), &mut out).unwrap();
        let response: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            response,
            json!({"output": {"text": "[1,2]", "truncated": false}})
        );

        let mut out = Vec::new();
        run_worker(&b"not json"[..], &mut out).unwrap();
        let response: Value = serde_json::from_slice(&out).unwrap();
        assert!(response["error"].as_str().unwrap().contains("invalid"));
    }

    #[test]
    fn takes_argument() {
        let mut args = JsonObject::new();
        args.insert("jq".into(), json!(".a"));
        args.insert("idx".into(), json!(42));
        assert_eq!(take_jq(&mut args).unwrap().unwrap().as_str(), ".a");
        assert!(!args.contains_key("jq"));
        args.insert("jq".into(), Value::Null);
        assert!(take_jq(&mut args).unwrap().is_none());
        args.insert("jq".into(), json!(1));
        assert!(take_jq(&mut args).is_err());
    }
}
