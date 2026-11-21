//! Инструмент `api_search`: поиск по всем операциям Wiki API.
//!
//! Ищет и по незарегистрированным операциям (выше `max_access` или вне `--toolsets`), чтобы
//! агент мог узнать, что операция существует и какой флаг её включает, а не изобретал обходные
//! пути. Работает локально по таблице операций, в API не обращается.
//!
//! Оценка простая и предсказуемая: запрос делится на слова (всё, что не буква и не цифра, —
//! разделитель), у каждого слова есть варианты ([`variants`]), каждый вариант ищется
//! подстрокой без учёта регистра в полях операции с весами ([`score`]); слово получает вес
//! лучшего поля среди вариантов. Сначала идут операции, совпавшие по большему числу слов,
//! затем по сумме весов, затем с более коротким именем, затем по алфавиту.
//!
//! Описания операций — на русском, а имена, пути и `operationId` — на английском, поэтому
//! варианты слова такие:
//!
//! - само слово (`ё` приводится к `е`);
//! - для русских слов — основа без окончания ([`russian_stem`]): «таблица» → «таблиц»
//!   находит «таблицы» и «таблицу» в описаниях;
//! - английский перевод русской основы по словарю [`RU_EN`]: «таблица» → `grid` находит пути
//!   `/v1/grids…` и имена инструментов;
//! - для английских слов — единственное число (`attachments` → `attachment`).
//!
//! Составные инструменты ([`Composite`]) ищутся теми же словами по имени, названию,
//! описанию и русским ключевым словам и выводятся отдельным списком `composite_tools`: они
//! делают несколько вызовов за один, и агенту стоит увидеть их раньше отдельных шагов.
//! Чтобы общие слова («страница», «файл») не тянули их в каждую выдачу, составной
//! инструмент показывается, только если с началом слов его полей совпали все слова запроса
//! или хотя бы два; слова короче трёх букв (предлоги) не учитываются.

use std::sync::Arc;

use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};
use wiki_api::spec::{AccessLevel, OperationDef, find, operations};

use crate::composite::Composite;
use crate::dynamic_toolsets::ENABLE_TOOLSET_TOOL;
use crate::error::ToolError;
use crate::registry::{
    ToolRegistry, access_hint, access_name, access_requirement, uses_local_files,
};
use crate::toolsets::available_toolsets;

/// Имя инструмента.
pub const API_SEARCH_TOOL: &str = "api_search";

/// Сколько результатов по умолчанию и максимум.
pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;

const ARGUMENTS: &[&str] = &["query", "tool_name", "toolset", "limit"];

/// Бонус за точное совпадение всего запроса с именем инструмента или `operationId`.
const EXACT_BONUS: u32 = 100;

/// Минимальная длина слова запроса при поиске составных инструментов.
const MIN_COMPOSITE_WORD_CHARS: usize = 3;

/// Минимальная длина русской основы: короче — слово ищется как есть («файл», «прав»).
const MIN_STEM_CHARS: usize = 4;

/// Окончания русских слов, самые длинные первыми. Это не морфология, а грубое отсечение,
/// которого хватает для поиска подстрокой по описаниям операций.
const RUSSIAN_ENDINGS: &[&str] = &[
    "иями", "ями", "ами", "его", "ого", "ему", "ому", "ыми", "ими", "иях", "ах", "ях", "ов", "ев",
    "ей", "ой", "ий", "ый", "ая", "яя", "ое", "ее", "ые", "ие", "ую", "юю", "ом", "ем", "ам", "ям",
    "а", "я", "о", "е", "ы", "и", "у", "ю", "ь", "й",
];

/// Перевод основ русских слов в слова, которые встречаются в путях и именах операций.
/// Ключ сравнивается с началом основы (или слова) запроса.
const RU_EN: &[(&str, &[&str])] = &[
    ("страниц", &["page"]),
    ("подстраниц", &["descendant"]),
    ("дочерн", &["descendant"]),
    ("потомк", &["descendant"]),
    ("вложен", &["attachment"]),
    ("файл", &["attachment", "file", "upload"]),
    ("загруз", &["upload"]),
    ("скача", &["download"]),
    ("скачив", &["download"]),
    ("превью", &["preview"]),
    ("миниатюр", &["preview"]),
    ("коммент", &["comment"]),
    ("таблиц", &["grid"]),
    ("грид", &["grid"]),
    ("строк", &["row"]),
    ("столбц", &["column"]),
    ("столбе", &["column"]),
    ("колонк", &["column"]),
    ("ячейк", &["cell"]),
    ("ячеек", &["cell"]),
    ("доступ", &["access"]),
    ("прав", &["access"]),
    ("истори", &["revision"]),
    ("верси", &["revision"]),
    ("ревизи", &["revision"]),
    ("обратн", &["backlink"]),
    ("ссылк", &["backlink", "url"]),
    ("перенос", &["move"]),
    ("перенест", &["move"]),
    ("перемест", &["move"]),
    ("копи", &["clone"]),
    ("скопир", &["clone"]),
    ("клон", &["clone"]),
    ("поиск", &["search"]),
    ("найт", &["search"]),
    ("пространств", &["space"]),
    ("пользовател", &["user"]),
    ("сесси", &["session", "upload"]),
    ("восстанов", &["recover"]),
    ("удал", &["delete", "remove"]),
    ("созда", &["create"]),
    ("измен", &["update"]),
    ("правк", &["update"]),
    ("редакт", &["update"]),
    ("обнов", &["update"]),
    ("дописа", &["append"]),
    ("добав", &["add", "append"]),
    ("ресурс", &["resource"]),
    ("операци", &["operation"]),
    ("статус", &["status"]),
    ("текущ", &["current"]),
];

/// Описание инструмента.
pub fn api_search_tool() -> Tool {
    let schema = json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Words to search for in operation names, summaries, descriptions, \
                    paths and toolsets, in English or Russian, e.g. `attachment`, `grid rows`, \
                    `таблица` or `/v1/pages/{idx}/comments`."
            },
            "tool_name": {
                "type": "string",
                "description": "Exact tool name of one operation (from a previous search) to get \
                    its full input schema instead of searching. Mutually exclusive with `query`."
            },
            "toolset": {
                "type": "string",
                "description": "Limit the search to one toolset (e.g. `grids`). Without `query`, \
                    lists all operations of the toolset."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": format!("Maximum number of results (default {DEFAULT_LIMIT}, max {MAX_LIMIT}).")
            }
        },
        "additionalProperties": false
    });
    let schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    let description = "Search all Yandex Wiki API operations, including those whose tools are \
        not enabled on this server. Each result shows the tool name, HTTP method and path, \
        access level, toolset, required arguments, the API reference page when there is one, \
        whether the tool is registered here and, if not, what enables it (`enable_with`). Pass \
        `tool_name` to get the full input schema of one operation. Matching composite tools, \
        which run several operations in one call (uploading a file as an attachment, waiting \
        for a background operation), are listed separately in `composite_tools`.\n\n\
        If the tool is registered, call it directly. If only its toolset is disabled, enable it \
        with `enable_toolset` when the server offers that tool, or call the same method and path \
        with `api_request`, or ask the user to add the toolset. If it needs a higher access \
        level, ask the user to allow it as `note` says. Registered operation \
        tools and `api_request` accept an optional `jq` filter to shrink the response.\n\n\
        Access: read-only (does not call the API).";
    let mut tool = Tool::new(API_SEARCH_TOOL, description, Arc::new(schema));
    tool.title = Some("Search Wiki API operations".to_owned());
    tool.annotations = Some(
        ToolAnnotations::with_title("Search Wiki API operations")
            .read_only(true)
            .open_world(false),
    );
    tool
}

/// Выполняет поиск; результат — компактный JSON.
pub fn search(args: &JsonObject, registry: &ToolRegistry) -> Result<String, ToolError> {
    reject_unknown(args)?;
    let query = optional_string(args, "query")?.unwrap_or_default();
    let tool_name = optional_string(args, "tool_name")?;
    let toolset = optional_string(args, "toolset")?.filter(|slug| !slug.is_empty());
    let limit = parse_limit(args.get("limit"))?;

    if let Some(name) = tool_name.filter(|name| !name.is_empty()) {
        if !query.is_empty() {
            return Err(ToolError::new(
                "`query` and `tool_name` are mutually exclusive; pass `tool_name` to inspect one \
                 operation or `query` to search",
            ));
        }
        return describe(name, registry);
    }

    if let Some(slug) = toolset
        && !available_toolsets().iter().any(|t| t.slug == slug)
    {
        let list: Vec<&str> = available_toolsets().iter().map(|t| t.slug).collect();
        return Err(ToolError::new(format!(
            "unknown toolset `{slug}`; available toolsets: {}",
            list.join(", ")
        )));
    }
    if query.is_empty() && toolset.is_none() {
        return Err(ToolError::new(
            "pass `query` (words to search for), `toolset`, or `tool_name`",
        ));
    }

    let composites: Vec<Value> = rank_composites(query, toolset)
        .into_iter()
        .map(|composite| composite_entry(composite, registry, false))
        .collect();
    let hits = rank(query, toolset);
    let total = hits.len();
    let results: Vec<Value> = hits
        .into_iter()
        .take(limit)
        .map(|(op, _)| summary_entry(op, registry))
        .collect();
    let mut out = Map::new();
    out.insert("total_matches".into(), json!(total));
    out.insert("returned".into(), json!(results.len()));
    out.insert("results".into(), Value::Array(results));
    if !composites.is_empty() {
        out.insert("composite_tools".into(), Value::Array(composites));
        out.insert(
            "composite_note".into(),
            json!("`composite_tools` run several of the operations above in one call (see `calls`); prefer them when they fit the task."),
        );
    }
    if total > limit {
        out.insert(
            "note".into(),
            json!("More operations match: refine the query, filter by `toolset` or raise `limit`."),
        );
    } else if total == 0 {
        out.insert(
            "note".into(),
            json!("Nothing matches: try other words (English API names such as `page`, `attachment`, `grid` work best) or list a toolset with `toolset`."),
        );
    }
    Ok(Value::Object(out).to_string())
}

/// Операции, отсортированные по релевантности, с оценкой.
pub fn rank(query: &str, toolset: Option<&str>) -> Vec<(&'static OperationDef, u32)> {
    let tokens = tokenize(query);
    let whole = normalize(query.trim());
    let mut hits: Vec<(&'static OperationDef, usize, u32)> = operations()
        .iter()
        .filter(|op| toolset.is_none_or(|slug| op.toolset == slug))
        .filter_map(|op| {
            if tokens.is_empty() {
                return Some((op, 0, 0));
            }
            let (matched, mut score) = score(op, &tokens);
            if matched == 0 {
                return None;
            }
            if whole == op.tool_name
                || op
                    .upstream_operation_id
                    .is_some_and(|id| whole == id.to_lowercase())
            {
                score += EXACT_BONUS;
            }
            Some((op, matched, score))
        })
        .collect();
    hits.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| b.2.cmp(&a.2))
            // При равной оценке короче имя — меньше посторонних слов. Без запроса (только
            // набор) список идёт по алфавиту.
            .then_with(|| {
                if tokens.is_empty() {
                    std::cmp::Ordering::Equal
                } else {
                    a.0.tool_name.len().cmp(&b.0.tool_name.len())
                }
            })
            .then_with(|| a.0.tool_name.cmp(b.0.tool_name))
    });
    hits.into_iter().map(|(op, _, score)| (op, score)).collect()
}

/// Нижний регистр и `ё` → `е`.
fn normalize(text: &str) -> String {
    text.to_lowercase().replace('ё', "е")
}

/// Слова запроса без повторов, у каждого — список вариантов (см. описание модуля).
fn tokenize(query: &str) -> Vec<Vec<String>> {
    let mut words: Vec<String> = Vec::new();
    for word in query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = normalize(word);
        if !words.contains(&word) {
            words.push(word);
        }
    }
    words.iter().map(|word| variants(word)).collect()
}

/// Варианты одного слова: само слово, русская основа и её перевод, английское единственное
/// число.
fn variants(word: &str) -> Vec<String> {
    let mut out = vec![word.to_owned()];
    let mut push = |value: &str| {
        if !value.is_empty() && !out.iter().any(|known| known == value) {
            out.push(value.to_owned());
        }
    };
    if word.chars().any(is_cyrillic) {
        let stem = russian_stem(word);
        push(&stem);
        for (key, translations) in RU_EN {
            if word.starts_with(key) || stem.starts_with(key) {
                for translation in *translations {
                    push(translation);
                }
            }
        }
    } else if word.len() > 3 {
        if let Some(stem) = word.strip_suffix("ies") {
            push(&format!("{stem}y"));
        } else if let Some(singular) = word.strip_suffix('s') {
            push(singular);
        }
    }
    out
}

fn is_cyrillic(c: char) -> bool {
    matches!(c, 'а'..='я' | 'А'..='Я' | 'ё' | 'Ё')
}

/// Основа русского слова: без самого длинного подходящего окончания, если остаётся не меньше
/// [`MIN_STEM_CHARS`] букв.
fn russian_stem(word: &str) -> String {
    let length = word.chars().count();
    for ending in RUSSIAN_ENDINGS {
        if let Some(stem) = word.strip_suffix(ending)
            && length - ending.chars().count() >= MIN_STEM_CHARS
        {
            return stem.to_owned();
        }
    }
    word.to_owned()
}

/// Сколько слов совпало и сумма весов лучших полей операции.
fn score(op: &OperationDef, tokens: &[Vec<String>]) -> (usize, u32) {
    let fields = [
        (6, op.tool_name.to_lowercase()),
        (
            4,
            op.upstream_operation_id.unwrap_or_default().to_lowercase(),
        ),
        (3, op.path.to_lowercase()),
        (3, op.toolset.to_lowercase()),
        (2, normalize(op.summary)),
        (
            1,
            normalize(&format!(
                "{} {} {}",
                op.description,
                op.method.as_str(),
                toolset_description(op)
            )),
        ),
    ];
    score_fields(&fields, tokens, false)
}

/// Составные инструменты, которые подходят под запрос (см. описание модуля), лучшие первыми.
/// Без запроса — все составные инструменты набора `toolset`.
fn rank_composites(query: &str, toolset: Option<&str>) -> Vec<Composite> {
    // Предлоги и союзы («к», «на», `to`) совпадают с началом слишком многих слов.
    let all = tokenize(query);
    let listing = all.is_empty();
    let tokens: Vec<Vec<String>> = all
        .into_iter()
        .filter(|variants| variants[0].chars().count() >= MIN_COMPOSITE_WORD_CHARS)
        .collect();
    if !listing && tokens.is_empty() {
        return Vec::new();
    }
    let mut hits: Vec<(Composite, usize, u32)> = Composite::ALL
        .into_iter()
        .filter(|composite| toolset.is_none_or(|slug| composite.toolset() == slug))
        .filter_map(|composite| {
            if listing {
                return Some((composite, 0, 0));
            }
            let fields = [
                (6, composite.name().to_owned()),
                (3, composite.toolset().to_owned()),
                (2, normalize(composite.summary())),
                (2, normalize(composite.search_keywords())),
                (1, normalize(&composite.description())),
            ];
            let (matched, score) = score_fields(&fields, &tokens, true);
            (matched == tokens.len() || matched >= 2).then_some((composite, matched, score))
        })
        .collect();
    hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)));
    hits.into_iter()
        .map(|(composite, _, _)| composite)
        .collect()
}

/// Сколько слов совпало и сумма весов лучших полей. С `word_starts_only` считаются только
/// совпадения с начала слова: у составных инструментов длинные английские описания, и
/// совпадение внутри слова (`row` в `growing`) там случайно.
fn score_fields(
    fields: &[(u32, String)],
    tokens: &[Vec<String>],
    word_starts_only: bool,
) -> (usize, u32) {
    let mut matched = 0;
    let mut total = 0;
    for variants in tokens {
        let best = variants
            .iter()
            .flat_map(|needle| {
                fields.iter().map(move |(weight, text)| {
                    let score = field_score(text, needle, *weight);
                    if word_starts_only && score < weight * 2 {
                        0
                    } else {
                        score
                    }
                })
            })
            .max()
            .unwrap_or(0);
        if best > 0 {
            matched += 1;
            total += best;
        }
    }
    (matched, total)
}

/// Вес совпадения слова с полем: начало слова поля (`grid` в `get_grid`, `/v1/grids`) —
/// двойной вес, середина слова (`page` в `list_subpages`) — одинарный, нет совпадения — 0.
fn field_score(text: &str, needle: &str, weight: u32) -> u32 {
    let mut best = 0;
    for (index, _) in text.match_indices(needle) {
        let word_start = text[..index]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        if word_start {
            return weight * 2;
        }
        best = weight;
    }
    best
}

/// Описание набора, к которому относится операция.
fn toolset_description(op: &OperationDef) -> &'static str {
    wiki_api::spec::find_toolset(op.toolset)
        .map(|t| t.description)
        .unwrap_or_default()
}

/// Краткая запись операции для результатов поиска.
fn summary_entry(op: &'static OperationDef, registry: &ToolRegistry) -> Value {
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(op.tool_name));
    entry.insert("method".into(), json!(op.method.as_str()));
    entry.insert("path".into(), json!(op.path));
    entry.insert("summary".into(), json!(op.summary.trim()));
    entry.insert("access".into(), json!(access_name(op.access)));
    entry.insert("toolset".into(), json!(op.toolset));
    entry.insert("required_params".into(), json!(required_params(op)));
    if let Some(url) = op.docs_url {
        entry.insert("reference".into(), json!(url));
    }
    insert_availability(&mut entry, Availability::of_operation(op), registry);
    Value::Object(entry)
}

/// Запись составного инструмента; `full` — с описанием и схемой аргументов.
fn composite_entry(composite: Composite, registry: &ToolRegistry, full: bool) -> Value {
    let schema = Value::Object(composite.input_schema());
    let required: Vec<Value> = schema["required"].as_array().cloned().unwrap_or_default();
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(composite.name()));
    entry.insert("composite".into(), json!(true));
    entry.insert("summary".into(), json!(composite.summary()));
    if full {
        entry.insert("description".into(), json!(composite.description()));
    }
    entry.insert("access".into(), json!(access_name(composite.access())));
    entry.insert("toolset".into(), json!(composite.toolset()));
    entry.insert("required_params".into(), Value::Array(required));
    entry.insert("calls".into(), json!(composite.steps()));
    insert_availability(
        &mut entry,
        Availability {
            name: composite.name(),
            access: composite.access(),
            toolset: composite.toolset(),
            local_files: composite.uses_local_files(),
            remote_only: composite.remote_only(),
        },
        registry,
    );
    if full {
        entry.insert("input_schema".into(), schema);
    }
    Value::Object(entry)
}

/// Полное описание одной операции со схемой аргументов.
fn describe(name: &str, registry: &ToolRegistry) -> Result<String, ToolError> {
    if let Some(composite) = Composite::find(name) {
        return Ok(composite_entry(composite, registry, true).to_string());
    }
    let Some(op) = find(name) else {
        let similar: Vec<&str> = rank(name, None)
            .into_iter()
            .take(5)
            .map(|(op, _)| op.tool_name)
            .collect();
        let hint = if similar.is_empty() {
            "use `query` to search".to_owned()
        } else {
            format!("similar: {}", similar.join(", "))
        };
        return Err(ToolError::new(format!(
            "unknown operation `{name}`; {hint}"
        )));
    };
    let schema: Value = serde_json::from_str(op.input_schema).unwrap_or(Value::Null);
    let mut entry = Map::new();
    entry.insert("tool_name".into(), json!(op.tool_name));
    if let Some(id) = op.upstream_operation_id {
        entry.insert("operation_id".into(), json!(id));
    }
    entry.insert("method".into(), json!(op.method.as_str()));
    entry.insert("path".into(), json!(op.path));
    entry.insert("summary".into(), json!(op.summary.trim()));
    entry.insert("description".into(), json!(op.description.trim()));
    entry.insert("access".into(), json!(access_name(op.access)));
    entry.insert("toolset".into(), json!(op.toolset));
    if let Some(url) = op.docs_url {
        entry.insert("reference".into(), json!(url));
    }
    insert_availability(&mut entry, Availability::of_operation(op), registry);
    entry.insert("input_schema".into(), schema);
    Ok(Value::Object(entry).to_string())
}

/// Что нужно, чтобы инструмент был зарегистрирован.
struct Availability {
    name: &'static str,
    access: AccessLevel,
    toolset: &'static str,
    local_files: bool,
    /// Только для HTTP-режима ([`Composite::remote_only`]).
    remote_only: bool,
}

impl Availability {
    fn of_operation(op: &'static OperationDef) -> Self {
        Self {
            name: op.tool_name,
            access: op.access,
            toolset: op.toolset,
            local_files: uses_local_files(op),
            remote_only: false,
        }
    }
}

/// `registered`, а для незарегистрированных — что их включает (`enable_with`: уровень доступа
/// профиля, флаг `--toolsets`) и что делать (`note`).
fn insert_availability(entry: &mut Map<String, Value>, op: Availability, registry: &ToolRegistry) {
    let registered = registry.get(op.name).is_some();
    entry.insert("registered".into(), json!(registered));
    if registered {
        return;
    }
    if !registry.local_files() && op.local_files {
        entry.insert(
            "note".into(),
            json!("Not available on this server: it reads or writes a local file, and this server runs on another machine (HTTP mode). To attach a file from your machine use `create_upload_url`; to download one, ask the user to do it in Wiki, or to use a local (stdio) `wiki-mcp serve`."),
        );
        return;
    }
    if registry.local_files() && op.remote_only {
        entry.insert(
            "note".into(),
            json!("Only on a remote server (`wiki-mcp serve --http`). This server runs on your machine: attach a local file with `upload_attachment`."),
        );
        return;
    }
    // Что нужно, чтобы инструмент появился: уровень доступа профиля и (без динамических
    // наборов) флаг `--toolsets`.
    let mut needs = Vec::new();
    let access_ok = registry.max_access().allows(op.access);
    if !access_ok {
        needs.push(access_requirement(op.access, registry.local_files()));
    }
    if registry.is_dynamic() {
        // Набор агент включает сам; флаг нужен только для уровня доступа.
        if access_ok {
            entry.insert(
                "enable_with".into(),
                json!(format!(
                    "{ENABLE_TOOLSET_TOOL} {{\"toolset\": \"{}\"}}",
                    op.toolset
                )),
            );
            // У составного инструмента нет одного метода и пути для `api_request`.
            let note = if Composite::find(op.name).is_some() {
                format!(
                    "Its toolset is not enabled: call `{ENABLE_TOOLSET_TOOL}` with this toolset."
                )
            } else {
                format!(
                    "Its toolset is not enabled: call `{ENABLE_TOOLSET_TOOL}` with this toolset, \
                     or call the same method and path with `api_request`."
                )
            };
            entry.insert("note".into(), json!(note));
            return;
        }
    } else if !registry.toolset_enabled(op.toolset) {
        needs.push(format!("--toolsets {}", op.toolset));
    }
    entry.insert("enable_with".into(), json!(needs.join(" and ")));
    let note = if access_ok {
        "Its toolset is not enabled: call the same method and path with `api_request`, or ask \
         the user to add the toolset to --toolsets."
            .to_owned()
    } else {
        format!(
            "Needs a higher access level than this server allows: ask the user to {} if the \
             change is intended.",
            access_hint(op.access, registry.local_files())
        )
    };
    entry.insert("note".into(), json!(note));
}

/// Имена обязательных параметров операции.
fn required_params(op: &OperationDef) -> Vec<&'static str> {
    op.params
        .iter()
        .filter(|param| param.required)
        .map(|param| param.name)
        .collect()
}

fn reject_unknown(args: &JsonObject) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !ARGUMENTS.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    Err(ToolError::new(format!(
        "unknown argument(s) {}; allowed arguments: `query`, `tool_name`, `toolset`, `limit`",
        unknown.join(", ")
    )))
}

fn optional_string<'a>(args: &'a JsonObject, name: &str) -> Result<Option<&'a str>, ToolError> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim())),
        Some(_) => Err(ToolError::new(format!(
            "argument `{name}` must be a string"
        ))),
    }
}

fn parse_limit(value: Option<&Value>) -> Result<usize, ToolError> {
    let limit = match value {
        None | Some(Value::Null) => return Ok(DEFAULT_LIMIT),
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse::<u64>().ok(),
        Some(_) => None,
    };
    match limit {
        Some(0) | None => Err(ToolError::new(format!(
            "argument `limit` must be a positive integer (max {MAX_LIMIT})"
        ))),
        Some(n) => Ok(usize::try_from(n).unwrap_or(MAX_LIMIT).min(MAX_LIMIT)),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiki_api::AccessLevel;

    use super::*;
    use crate::ServerOptions;

    fn registry(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ToolRegistry {
        ToolRegistry::new(&ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            ..ServerOptions::default()
        })
        .unwrap()
    }

    fn run(value: Value, registry: &ToolRegistry) -> Result<Value, ToolError> {
        let Value::Object(args) = value else {
            panic!("не объект")
        };
        search(&args, registry).map(|text| serde_json::from_str(&text).unwrap())
    }

    fn names(result: &Value) -> Vec<&str> {
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["tool_name"].as_str().unwrap())
            .collect()
    }

    fn ranked(query: &str) -> Vec<&'static str> {
        rank(query, None)
            .into_iter()
            .map(|(op, _)| op.tool_name)
            .collect()
    }

    fn entry<'a>(result: &'a Value, name: &str) -> &'a Value {
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["tool_name"] == name)
            .unwrap_or_else(|| panic!("нет {name} в {result}"))
    }

    #[test]
    fn tokenize_splits_dedupes_and_adds_variants() {
        let tokens = tokenize("Attachments  grid-Grid ТАБЛИЦА таблицы Вложение");
        let first: Vec<&str> = tokens.iter().map(|v| v[0].as_str()).collect();
        assert_eq!(
            first,
            ["attachments", "grid", "таблица", "таблицы", "вложение"]
        );
        assert!(tokens[0].contains(&"attachment".to_owned()));
        assert!(tokens[2].contains(&"таблиц".to_owned()));
        assert!(tokens[2].contains(&"grid".to_owned()));
        assert!(tokens[3].contains(&"grid".to_owned()));
        assert!(tokens[4].contains(&"attachment".to_owned()));
        assert!(tokenize("  ").is_empty());
    }

    #[test]
    fn word_starts_weigh_more() {
        assert_eq!(field_score("get_grid", "grid", 3), 6);
        assert_eq!(field_score("/v1/grids/{idx}", "grid", 3), 6);
        assert_eq!(field_score("list_subpages", "page", 3), 3);
        assert_eq!(field_score("изменить динамическую таблицу", "таблиц", 2), 4);
        assert_eq!(field_score("get_page", "grid", 3), 0);
    }

    #[test]
    fn russian_stems() {
        assert_eq!(russian_stem("таблица"), "таблиц");
        assert_eq!(russian_stem("страницы"), "страниц");
        assert_eq!(russian_stem("вложение"), "вложен");
        assert_eq!(russian_stem("комментарий"), "комментар");
        // Короткие слова не обрезаются.
        assert_eq!(russian_stem("файл"), "файл");
        assert_eq!(russian_stem("прав"), "прав");
    }

    #[test]
    fn grids_in_russian_find_grid_operations() {
        for query in ["таблица", "таблицы", "Таблицу", "grid", "grids"] {
            let hits = ranked(query);
            assert!(!hits.is_empty(), "{query}");
            for name in [
                "get_grid",
                "create_grid",
                "update_grid",
                "delete_grid",
                "add_grid_rows",
                "update_grid_cells",
            ] {
                assert!(hits.contains(&name), "{query}: {name} нет в {hits:?}");
            }
            // Лучшие совпадения — операции таблиц.
            for name in hits.iter().take(5) {
                let op = find(name).unwrap();
                assert_eq!(op.toolset, "grids", "{query}: {name}");
            }
        }
        let hits = ranked("строки таблицы");
        assert!(
            hits.iter().take(3).any(|name| name.contains("rows")),
            "{hits:?}"
        );
        let hits = ranked("ячейки таблицы");
        assert_eq!(hits[0], "update_grid_cells", "{hits:?}");
    }

    #[test]
    fn attachments_in_russian_and_english() {
        for query in ["вложение", "вложения", "attachment", "attachments"] {
            let hits = ranked(query);
            let top: Vec<&str> = hits.iter().take(8).copied().collect();
            for name in [
                "list_attachments",
                "delete_attachment",
                "download_attachment",
            ] {
                assert!(top.contains(&name), "{query}: {name} нет в {top:?}");
            }
        }
        // Загрузка файла: сессии загрузки и загрузка части.
        let hits = ranked("загрузить файл");
        let top: Vec<&str> = hits.iter().take(8).copied().collect();
        for name in ["upload_file_part", "create_upload_session"] {
            assert!(top.contains(&name), "{name} нет в {top:?}");
        }
        let hits = ranked("скачать вложение");
        assert!(hits[0].starts_with("download_attachment"), "{hits:?}");
    }

    #[test]
    fn access_rights_are_found() {
        for query in ["доступ", "права доступа", "access"] {
            let hits = ranked(query);
            let top: Vec<&str> = hits.iter().take(6).copied().collect();
            for name in [
                "create_page_access",
                "update_page_access",
                "delete_page_access",
                "delete_all_page_accesses",
            ] {
                assert!(top.contains(&name), "{query}: {name} нет в {top:?}");
            }
        }
    }

    #[test]
    fn ranking_prefers_exact_names_and_paths() {
        let hits = rank("get_page", None);
        assert_eq!(hits[0].0.tool_name, "get_page");
        assert!(hits[0].1 >= EXACT_BONUS);
        assert_eq!(ranked("/v1/search")[0], "search");
        assert_eq!(ranked("поиск")[0], "search");
        assert_eq!(ranked("подстраницы")[0], "list_page_descendants");
        assert!(rank("zzzqqq", None).is_empty());
    }

    fn composite_names(result: &Value) -> Vec<&str> {
        result
            .get("composite_tools")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|r| r["tool_name"].as_str().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn composite_tools_are_found() {
        let read = registry(AccessLevel::Read, None);
        for query in [
            "загрузить файл",
            "прикрепить файл к странице",
            "upload file",
            "upload attachment",
        ] {
            let result = run(json!({ "query": query }), &read).unwrap();
            assert_eq!(
                composite_names(&result).first(),
                Some(&"upload_attachment"),
                "{query}: {result}"
            );
        }
        let result = run(json!({"query": "upload attachment"}), &read).unwrap();
        let upload = &result["composite_tools"][0];
        assert_eq!(upload["composite"], true);
        assert_eq!(upload["access"], "write");
        assert_eq!(upload["toolset"], "attachments");
        assert_eq!(upload["registered"], false);
        assert_eq!(
            upload["enable_with"],
            "`write` access for the server's profile (`wiki-mcp ui`)"
        );
        assert_eq!(upload["required_params"], json!(["idx", "file"]));
        assert!(
            upload["calls"]
                .as_array()
                .unwrap()
                .contains(&json!("upload_file_part"))
        );
        // Операции шагов остаются в обычной выдаче.
        assert!(names(&result).contains(&"upload_file_part"), "{result}");

        for query in [
            "дождаться копирования страницы",
            "статус операции",
            "wait operation",
            "wait_for_operation",
        ] {
            let result = run(json!({ "query": query }), &read).unwrap();
            assert_eq!(
                composite_names(&result).first(),
                Some(&"wait_for_operation"),
                "{query}: {result}"
            );
            assert_eq!(result["composite_tools"][0]["registered"], true);
        }

        // Посторонние запросы составные инструменты не показывают.
        for query in ["строки таблицы", "comment", "доступ к странице", "к", "to"]
        {
            let result = run(json!({ "query": query }), &read).unwrap();
            assert!(composite_names(&result).is_empty(), "{query}: {result}");
        }
        // Набор без запроса — все его составные инструменты.
        let result = run(json!({"toolset": "attachments"}), &read).unwrap();
        assert_eq!(
            composite_names(&result),
            ["upload_attachment", "create_upload_url"]
        );
        // Адрес загрузки на сервере на машине агента не зарегистрирован и не включается
        // флагом: подсказка ведёт к `upload_attachment`.
        let url = &result["composite_tools"][1];
        assert_eq!(url["registered"], false);
        assert!(url.get("enable_with").is_none(), "{url}");
        assert!(
            url["note"].as_str().unwrap().contains("upload_attachment"),
            "{url}"
        );

        let result = run(json!({"tool_name": "upload_attachment"}), &read).unwrap();
        assert_eq!(result["tool_name"], "upload_attachment");
        assert_eq!(result["input_schema"]["required"], json!(["idx", "file"]));
        assert!(
            result["description"]
                .as_str()
                .unwrap()
                .contains("create_upload_session")
        );

        // В HTTP-режиме загрузки файла нет: так и сказано.
        let remote = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Write,
            local_machine: false,
            ..ServerOptions::default()
        })
        .unwrap();
        let result = run(json!({"tool_name": "upload_attachment"}), &remote).unwrap();
        assert_eq!(result["registered"], false);
        assert!(
            result["note"].as_str().unwrap().contains("HTTP mode"),
            "{result}"
        );
    }

    #[test]
    fn results_show_registration_and_flags() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"query": "comment", "limit": 10}), &read).unwrap();
        let create = entry(&result, "create_comment");
        assert_eq!(create["method"], "POST");
        assert_eq!(create["path"], "/v1/pages/{idx}/comments");
        assert_eq!(create["access"], "write");
        assert_eq!(create["toolset"], "comments");
        assert_eq!(create["registered"], false);
        assert_eq!(
            create["enable_with"],
            "`write` access for the server's profile (`wiki-mcp ui`)"
        );
        assert!(
            create["required_params"]
                .as_array()
                .unwrap()
                .contains(&json!("idx"))
        );
        let delete = entry(&result, "delete_comment");
        assert_eq!(
            delete["enable_with"],
            "`destructive` access for the server's profile (`wiki-mcp ui`)"
        );
        let list = entry(&result, "list_comments");
        assert_eq!(list["registered"], true);
        assert!(list.get("enable_with").is_none());

        // Набор доступа выключен по умолчанию и выше уровня: оба флага.
        let result = run(json!({"query": "доступ"}), &read).unwrap();
        let access = entry(&result, "create_page_access");
        assert_eq!(access["registered"], false);
        assert_eq!(
            access["enable_with"],
            "`destructive` access for the server's profile (`wiki-mcp ui`) and --toolsets access"
        );

        // Набор выключен, уровень хватает: подсказка про `api_request`.
        let result = run(json!({"query": "пространства"}), &read).unwrap();
        let spaces = entry(&result, "list_spaces");
        assert_eq!(spaces["enable_with"], "--toolsets extra");
        assert!(spaces["note"].as_str().unwrap().contains("api_request"));

        let result = run(
            json!({"tool_name": "delete_grid"}),
            &registry(AccessLevel::Read, Some(&["pages"])),
        )
        .unwrap();
        assert_eq!(
            result["enable_with"],
            "`destructive` access for the server's profile (`wiki-mcp ui`) and --toolsets grids"
        );
    }

    #[test]
    fn file_tools_are_marked_in_http_mode() {
        let all = registry(AccessLevel::Destructive, Some(&["all"]));
        let result = run(json!({"tool_name": "upload_file_part"}), &all).unwrap();
        assert_eq!(result["registered"], true);

        let remote = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Destructive,
            toolsets: Some(vec!["all".into()]),
            local_machine: false,
            ..ServerOptions::default()
        })
        .unwrap();
        let result = run(json!({"tool_name": "upload_file_part"}), &remote).unwrap();
        assert_eq!(result["registered"], false);
        assert!(result["note"].as_str().unwrap().contains("HTTP mode"));
        assert!(result.get("enable_with").is_none());
    }

    #[test]
    fn dynamic_mode_suggests_enable_toolset() {
        let dynamic = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Write,
            dynamic_toolsets: true,
            ..ServerOptions::default()
        })
        .unwrap();
        let result = run(json!({"tool_name": "get_page"}), &dynamic).unwrap();
        assert_eq!(result["registered"], false);
        assert_eq!(
            result["enable_with"],
            r#"enable_toolset {"toolset": "pages"}"#
        );
        let note = result["note"].as_str().unwrap();
        assert!(note.contains("`enable_toolset`"), "{note}");
        assert!(note.contains("`api_request`"), "{note}");

        // Составной инструмент включается своим набором; одного пути для `api_request` нет.
        let result = run(json!({"tool_name": "upload_attachment"}), &dynamic).unwrap();
        assert_eq!(
            result["enable_with"],
            r#"enable_toolset {"toolset": "attachments"}"#
        );
        let note = result["note"].as_str().unwrap();
        assert!(!note.contains("api_request"), "{note}");

        // Выше уровня доступа — только флаг сервера, без `--toolsets`.
        let result = run(json!({"tool_name": "delete_page"}), &dynamic).unwrap();
        assert_eq!(
            result["enable_with"],
            "`destructive` access for the server's profile (`wiki-mcp ui`)"
        );
    }

    #[test]
    fn limits_and_toolset_filter() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"query": "page"}), &read).unwrap();
        assert_eq!(result["returned"], DEFAULT_LIMIT);
        assert!(result["total_matches"].as_u64().unwrap() > DEFAULT_LIMIT as u64);
        assert!(result["note"].is_string());

        let result = run(json!({"query": "page", "limit": 1000}), &read).unwrap();
        assert_eq!(
            result["returned"].as_u64(),
            Some(
                result["total_matches"]
                    .as_u64()
                    .unwrap()
                    .min(MAX_LIMIT as u64)
            )
        );
        let result = run(json!({"query": "page", "limit": "2"}), &read).unwrap();
        assert_eq!(result["returned"], 2);

        for bad in [json!(0), json!(-1), json!(1.5), json!("x"), json!(true)] {
            let err = run(json!({"query": "page", "limit": bad}), &read).unwrap_err();
            assert!(err.message().contains("`limit`"), "{err}");
        }

        // Только набор: все его операции по имени.
        let result = run(json!({"toolset": "grids", "limit": 100}), &read).unwrap();
        let listed = names(&result);
        assert!(!listed.is_empty());
        assert!(listed.windows(2).all(|w| w[0] <= w[1]));
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["toolset"] == "grids")
        );

        let result = run(json!({"query": "delete", "toolset": "pages"}), &read).unwrap();
        assert!(names(&result).contains(&"delete_page"));
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["toolset"] == "pages")
        );

        let err = run(json!({"toolset": "Страницы"}), &read).unwrap_err();
        assert!(
            err.message().contains("unknown toolset `Страницы`"),
            "{err}"
        );

        let result = run(json!({"query": "zzzqqq"}), &read).unwrap();
        assert_eq!(result["total_matches"], 0);
        assert!(result["note"].as_str().unwrap().contains("Nothing matches"));
    }

    #[test]
    fn tool_name_returns_full_schema() {
        let read = registry(AccessLevel::Read, None);
        let result = run(json!({"tool_name": "create_page"}), &read).unwrap();
        assert_eq!(result["tool_name"], "create_page");
        assert_eq!(result["method"], "POST");
        assert_eq!(result["registered"], false);
        assert_eq!(
            result["enable_with"],
            "`write` access for the server's profile (`wiki-mcp ui`)"
        );
        assert_eq!(result["input_schema"]["type"], "object");
        assert!(result["input_schema"]["properties"]["body"].is_object());

        let err = run(json!({"tool_name": "get_pag"}), &read).unwrap_err();
        assert!(
            err.message().contains("unknown operation `get_pag`"),
            "{err}"
        );
        assert!(err.message().contains("get_page"), "{err}");
    }

    #[test]
    fn argument_errors() {
        let read = registry(AccessLevel::Read, None);
        assert!(
            run(json!({}), &read)
                .unwrap_err()
                .message()
                .contains("pass `query`")
        );
        assert!(
            run(json!({"query": "x", "tool_name": "get_page"}), &read)
                .unwrap_err()
                .message()
                .contains("mutually exclusive")
        );
        assert!(
            run(json!({"query": 1}), &read)
                .unwrap_err()
                .message()
                .contains("must be a string")
        );
        assert!(
            run(json!({"q": "x"}), &read)
                .unwrap_err()
                .message()
                .contains("unknown argument(s) `q`")
        );
    }

    #[test]
    fn tool_is_read_only() {
        let tool = api_search_tool();
        let a = tool.annotations.as_ref().unwrap();
        assert_eq!(a.read_only_hint, Some(true));
        assert_eq!(tool.input_schema["additionalProperties"], false);
    }
}
