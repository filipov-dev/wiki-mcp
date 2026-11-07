# Разработка

## Структура

| Крейт | Назначение |
| --- | --- |
| `crates/wiki-api` | Спецификация Wiki API (`spec/`), генератор таблицы операций (`build.rs`, `build/`), исполнитель HTTP-запросов, хранилище токенов. |
| `crates/wiki-mcp-core` | MCP-инструменты: реестр, уровни доступа, наборы, составные инструменты (`upload_attachment`, `wait_for_operation`), `api_request`, `api_search`, вырезание учётных данных, политика файлов. Не зависит от транспорта. |
| `crates/wiki-mcp` | Бинарник: CLI (`serve`, `connect`, `auth`), профили и транспорты stdio и HTTP (`src/cli/serve/http_server.rs`). |

## Сборка и проверки

Нужен Rust 1.88 или новее; `rust-toolchain.toml` выбирает стабильный канал с `rustfmt` и `clippy`. Для криптографии TLS нужен C-компилятор; системные библиотеки D-Bus и OpenSSL не нужны.

```bash
cargo build --release
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Тесты не обращаются к реальному Wiki API (HTTP подменяется `wiremock`) и не используют системное хранилище токенов.

Готовые сборки: macOS (universal), Linux x86_64 и aarch64 (статические, musl) и Windows x86_64 (`x86_64-pc-windows-msvc`).

### Windows

Сборка — нативная, целевая платформа `x86_64-pc-windows-msvc`: нужны Visual Studio Build Tools с компонентом «Разработка классических приложений на C++» и `rustup`. Релизный бинарник собирается со статическим CRT, чтобы на машине пользователя не требовался Visual C++ Redistributable:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --release --locked --target x86_64-pc-windows-msvc -p wiki-mcp
cargo clippy --all-targets --locked --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc
```

Ветки кода для Windows (хранилище токенов в Credential Manager, запуск `claude.cmd`, сравнение файлов без inode в исполнителе, завершение HTTP-режима без SIGTERM) компилируются или выполняются только там, поэтому проверяйте изменения в них этими командами. С macOS и Linux `cargo check --target x86_64-pc-windows-msvc` без Windows SDK не проходит: C-код `aws-lc-sys` требует заголовков Windows.

Запустить CLI из исходников:

```bash
cargo run -p wiki-mcp -- --help
cargo run -p wiki-mcp -- serve --help
```

## Спецификация и генерация инструментов

Инструменты генерируются из `crates/wiki-api/spec/wiki.openapi.json`. Её собирает скрипт `build_openapi.py` (Python 3.10+, без сторонних пакетов и сети) из двух файлов:

- `wiki.upstream.json` — снимок официальной спецификации `https://api.wiki.yandex.net/v1/openapi.json`, руками не правится;
- `overlay.json` — дополнение: короткие имена операций, уровни доступа, наборы, описания из справочника и исправления расхождений с реальным API.

```bash
python3 -I crates/wiki-api/spec/build_openapi.py          # пересобрать
python3 -I crates/wiki-api/spec/build_openapi.py --check  # проверить, что результат актуален
python3 -I crates/wiki-api/spec/spec_drift.py              # не отстал ли снимок от API (нужна сеть)
```

`spec_drift.py` скачивает официальную спецификацию и оглавление документации `llms.txt` и печатает новые, удалённые и изменённые операции (отдельно — новые без записи в дополнении) и страницы справочника, на которые не ссылается ни одна операция; без расхождений отчёт пустой, код выхода `0`, с расхождениями — `3`.

Как обновить снимок, что записано в дополнении и какие расхождения исправлены — в [описании спецификации](../crates/wiki-api/spec/README.md).

При сборке `build.rs` превращает спецификацию в таблицу операций: имя инструмента, метод, путь, уровень доступа (`x-access-level`), набор (тег спецификации) и JSON Schema аргументов. Чтобы обновить инструменты:

1. Обновите снимок или дополнение и пересоберите спецификацию скриптом. Если в снимке появилась операция без записи в дополнении, скрипт падает и называет её.
2. Соберите проект.
3. Проверьте снапшоты операций (`crates/wiki-api/tests/snapshots/`) и уровни доступа новых операций; снапшоты обновляются осознанно: `cargo insta review` или `INSTA_UPDATE=always cargo test -p wiki-api`.
4. Обновите [MCP-инструменты](tools.md), если изменились наборы, операции или число инструментов.

Сгенерированный код не правится вручную: единственный источник операций — спецификация.
