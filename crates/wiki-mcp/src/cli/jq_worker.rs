//! Скрытая подкоманда `wiki-mcp jq-worker`: выполняет один фильтр `jq` для `serve`.
//!
//! Сервер запускает её дочерним процессом с пустым окружением (см.
//! `wiki_mcp_core::jq`): бесконечная рекурсия или зацикливание фильтра завершают только
//! этот процесс. Запрос — JSON в stdin, ответ — JSON в stdout. Токен, хранилище, профили и
//! сеть здесь не используются.

use std::path::Path;

use anyhow::Context;
use wiki_mcp_core::JqRunner;

/// Имя подкоманды; `serve` передаёт его в `JqRunner::subprocess`.
pub const SUBCOMMAND: &str = "jq-worker";

pub fn run() -> anyhow::Result<()> {
    wiki_mcp_core::jq::run_worker(std::io::stdin().lock(), std::io::stdout().lock())
        .context("jq worker failed")
}

/// Фильтры `jq` выполняются в дочернем процессе этого же бинарника: зависший или
/// переполнивший стек фильтр не роняет сервер. Без пути к бинарнику — в потоке сервера.
pub fn runner(executable: Option<&Path>) -> JqRunner {
    match executable {
        Some(path) => JqRunner::subprocess(path, [SUBCOMMAND]),
        None => {
            tracing::warn!(
                "не удалось определить путь к бинарнику: фильтры jq выполняются в процессе сервера"
            );
            JqRunner::in_process()
        }
    }
}

/// Исполнитель фильтров для `serve`: текущий бинарник.
pub fn current_runner() -> JqRunner {
    runner(std::env::current_exe().ok().as_deref())
}
