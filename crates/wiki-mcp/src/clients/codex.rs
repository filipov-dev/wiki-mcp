//! Codex: запись `[mcp_servers.<имя>]` в `$CODEX_HOME/config.toml`.
//!
//! Файл правится через `toml_edit`, чтобы сохранить комментарии, форматирование и чужие записи.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use super::{ServerSpec, is_own_command};
use crate::ui::Ui;

/// Переменная окружения Codex с каталогом его конфигурации.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Каталог конфигурации Codex: `$CODEX_HOME`, иначе `~/.codex`.
pub fn codex_home(env_value: Option<OsString>, home: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(dir) = env_value.filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    home.map(|home| home.join(".codex"))
        .context("cannot determine the home directory; set CODEX_HOME")
}

/// Каталог конфигурации Codex текущего пользователя.
pub fn default_codex_home() -> anyhow::Result<PathBuf> {
    codex_home(std::env::var_os(CODEX_HOME_ENV), dirs::home_dir())
}

/// Путь к `config.toml` в каталоге Codex.
pub fn config_path(codex_home: &Path) -> PathBuf {
    codex_home.join("config.toml")
}

/// Прежняя запись с тем же именем.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousEntry {
    /// Имена ключей прежней записи. Значения не возвращаются и не показываются: в чужой или
    /// старой записи в `env` мог лежать токен.
    pub keys: Vec<String>,
    /// Запись запускала `wiki-mcp`.
    pub own: bool,
}

/// Записывает `command` и `args` сервера в документ. Остальные записи не меняются; если
/// `[mcp_servers.<имя>]` уже есть, таблица очищается и заполняется заново на том же месте
/// (с теми же комментариями над заголовком).
pub fn apply_entry(
    doc: &mut DocumentMut,
    spec: &ServerSpec,
) -> anyhow::Result<Option<PreviousEntry>> {
    let servers = doc
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut table = Table::new();
            // Без отдельного пустого заголовка `[mcp_servers]`.
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_mut()
        .context("`mcp_servers` in the Codex config is not a table; edit it manually")?;

    let mut args = Array::new();
    for arg in &spec.args {
        args.push(arg.as_str());
    }

    let previous = match servers.get_mut(&spec.name) {
        Some(Item::Table(table)) => {
            let previous = PreviousEntry {
                keys: table.iter().map(|(key, _)| key.to_owned()).collect(),
                own: table
                    .get("command")
                    .and_then(Item::as_str)
                    .is_some_and(is_own_command),
            };
            table.clear();
            table.insert("command", value(spec.command.as_str()));
            table.insert("args", value(args));
            Some(previous)
        }
        Some(other) => {
            let previous = PreviousEntry {
                keys: vec![format!("<{}>", other.type_name())],
                own: false,
            };
            *other = Item::Table(new_entry(spec, args));
            Some(previous)
        }
        None => {
            servers.insert(&spec.name, Item::Table(new_entry(spec, args)));
            None
        }
    };
    Ok(previous)
}

fn new_entry(spec: &ServerSpec, args: Array) -> Table {
    let mut table = Table::new();
    table.insert("command", value(spec.command.as_str()));
    table.insert("args", value(args));
    table
}

/// Запись сервера в виде TOML для показа пользователю.
pub fn render_entry(spec: &ServerSpec) -> anyhow::Result<String> {
    let mut doc = DocumentMut::new();
    apply_entry(&mut doc, spec)?;
    Ok(doc.to_string())
}

/// Регистрирует сервер в Codex. Возвращает `false`, если пользователь отказался.
///
/// Чужая запись с тем же именем (не `wiki-mcp`) заменяется только после явного ответа в
/// терминале: `yes` её не перезаписывает.
pub fn register(
    ui: &mut dyn Ui,
    codex_home: &Path,
    spec: &ServerSpec,
    yes: bool,
) -> anyhow::Result<bool> {
    let name = spec.name.as_str();
    let path = config_path(codex_home);
    let original = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    let mut doc: DocumentMut = original
        .parse()
        .with_context(|| format!("failed to parse {}", path.display()))?;
    let previous = apply_entry(&mut doc, spec)?;

    let mut message = format!(
        "Codex config {} will get:\n\n{}",
        path.display(),
        render_entry(spec)?.trim_end()
    );
    let foreign = previous.as_ref().is_some_and(|previous| !previous.own);
    if let Some(previous) = &previous {
        let whose = if previous.own {
            "wiki-mcp"
        } else {
            "another server, not wiki-mcp"
        };
        message.push_str(&format!(
            "\n\nIt replaces the existing `{name}` entry ({whose}; keys: {}).",
            previous.keys.join(", ")
        ));
    }
    ui.say(&message);
    if foreign {
        ui.say("To keep that entry, re-run connect with another --name.");
        if yes {
            bail!(
                "Codex config already has another MCP server named `{name}`; it was not \
                 changed. Choose another name with --name, or re-run without --yes to replace it"
            );
        }
        if !ui.confirm(&format!("Replace the other server `{name}`?"), false)? {
            return Ok(false);
        }
    } else if !yes && !ui.confirm("Write the Codex config?", false)? {
        return Ok(false);
    }

    let updated = doc.to_string();
    if updated == original {
        ui.say("Codex config already up to date.");
        return Ok(true);
    }
    if !codex_home.exists() {
        fs::create_dir_all(codex_home)
            .with_context(|| format!("failed to create {}", codex_home.display()))?;
    }
    // Запись по исходному пути (а не через rename), чтобы не сломать symlink на dotfiles.
    fs::write(&path, updated).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

/// Проверка, которую делаем до ввода токена: Codex поддерживает только пользовательскую область.
pub fn ensure_scope(scope: super::Scope) -> anyhow::Result<()> {
    if scope != super::Scope::User {
        bail!("Codex supports only `--scope user` (config in $CODEX_HOME/config.toml)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testing::ScriptedUi;

    fn spec() -> ServerSpec {
        ServerSpec {
            name: "wiki".into(),
            command: "/usr/local/bin/wiki-mcp".into(),
            args: vec!["serve".into(), "--profile".into(), "codex".into()],
        }
    }

    fn answers(values: &[&str]) -> ScriptedUi {
        ScriptedUi {
            answers: values.iter().map(|value| (*value).to_owned()).collect(),
            ..Default::default()
        }
    }

    /// Конфиг со старым сервером Wiki под другим именем и прежней записью `wiki`.
    const EXISTING: &str = r#"# Codex config
model = "gpt-5"  # inline comment

[mcp_servers.yandex_wiki_mcp]
url = "http://localhost:9903/mcp"
bearer_token_env_var = "LEGACY_TOKEN"

# our server
[mcp_servers.wiki]
command = "/opt/homebrew/bin/wiki-mcp"
args = ["serve", "--profile", "old"]
env = { SOME = "old-secret-value" }

[profiles.fast]
model = "gpt-5-mini"
"#;

    #[test]
    fn codex_home_resolution() {
        assert_eq!(
            codex_home(Some("/tmp/codex".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/tmp/codex")
        );
        assert_eq!(
            codex_home(Some("".into()), Some("/home/u".into())).unwrap(),
            PathBuf::from("/home/u").join(".codex")
        );
        assert!(codex_home(None, None).is_err());
    }

    #[test]
    fn codex_home_resolution_windows() {
        // В Windows `dirs::home_dir()` — `%USERPROFILE%`; `CODEX_HOME` берётся как есть.
        assert_eq!(
            codex_home(Some(r"D:\codex".into()), Some(r"C:\Users\u".into())).unwrap(),
            PathBuf::from(r"D:\codex")
        );
        assert_eq!(
            codex_home(None, Some(r"C:\Users\u".into())).unwrap(),
            PathBuf::from(r"C:\Users\u").join(".codex")
        );
    }

    #[test]
    fn windows_path_round_trips_through_toml() {
        let exe = r"C:\Users\u\AppData\Local\Programs\wiki-mcp\wiki-mcp.exe";
        let spec = ServerSpec {
            command: exe.into(),
            ..spec()
        };
        let mut doc = DocumentMut::new();
        apply_entry(&mut doc, &spec).unwrap();
        // Обратные косые черты в TOML экранируются или пишутся литеральной строкой:
        // после разбора путь тот же.
        let parsed: DocumentMut = doc.to_string().parse().unwrap();
        assert_eq!(parsed["mcp_servers"]["wiki"]["command"].as_str(), Some(exe));

        // Прежняя запись с Windows-путём (любая форма строки TOML) — своя.
        for existing in [
            "[mcp_servers.wiki]\ncommand = 'C:\\Tools\\wiki-mcp.exe'\n",
            "[mcp_servers.wiki]\ncommand = \"C:\\\\Tools\\\\WIKI-MCP.EXE\"\n",
        ] {
            let mut doc: DocumentMut = existing.parse().unwrap();
            let previous = apply_entry(&mut doc, &spec).unwrap().unwrap();
            assert!(previous.own, "{existing}");
        }
    }

    #[test]
    fn adds_entry_to_empty_config() {
        let mut doc = DocumentMut::new();
        assert_eq!(apply_entry(&mut doc, &spec()).unwrap(), None);
        assert_eq!(
            doc.to_string(),
            "[mcp_servers.wiki]\ncommand = \"/usr/local/bin/wiki-mcp\"\nargs = [\"serve\", \"--profile\", \"codex\"]\n"
        );
    }

    #[test]
    fn replaces_entry_and_preserves_everything_else() {
        let mut doc: DocumentMut = EXISTING.parse().unwrap();
        let previous = apply_entry(&mut doc, &spec()).unwrap().unwrap();
        assert_eq!(previous.keys, ["command", "args", "env"]);
        assert!(previous.own);

        assert_eq!(
            doc.to_string(),
            r#"# Codex config
model = "gpt-5"  # inline comment

[mcp_servers.yandex_wiki_mcp]
url = "http://localhost:9903/mcp"
bearer_token_env_var = "LEGACY_TOKEN"

# our server
[mcp_servers.wiki]
command = "/usr/local/bin/wiki-mcp"
args = ["serve", "--profile", "codex"]

[profiles.fast]
model = "gpt-5-mini"
"#
        );
    }

    #[test]
    fn register_writes_file_in_temp_codex_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex-home");
        let mut ui = answers(&["y"]);
        assert!(register(&mut ui, &home, &spec(), false).unwrap());
        let text = fs::read_to_string(config_path(&home)).unwrap();
        assert!(text.contains("[mcp_servers.wiki]"));
        assert!(
            ui.text()
                .contains("args = [\"serve\", \"--profile\", \"codex\"]")
        );

        // Повторный запуск ничего не меняет.
        assert!(register(&mut ui, &home, &spec(), true).unwrap());
        assert_eq!(fs::read_to_string(config_path(&home)).unwrap(), text);
        assert!(ui.text().contains("already up to date"));
    }

    #[test]
    fn register_keeps_file_when_declined() {
        for ui in [answers(&["n"]), answers(&[""])] {
            let mut ui = ui;
            let dir = tempfile::tempdir().unwrap();
            fs::write(config_path(dir.path()), EXISTING).unwrap();
            assert!(!register(&mut ui, dir.path(), &spec(), false).unwrap());
            assert_eq!(
                fs::read_to_string(config_path(dir.path())).unwrap(),
                EXISTING
            );
            let text = ui.text();
            assert!(
                text.contains(
                    "replaces the existing `wiki` entry (wiki-mcp; keys: command, args, env)"
                ),
                "{text}"
            );
            // Значения прежней записи (в том числе `env`) не показываются.
            assert!(!text.contains("old-secret-value"), "{text}");
        }
    }

    #[test]
    fn foreign_entry_needs_explicit_answer() {
        let foreign = ServerSpec {
            name: "yandex_wiki_mcp".into(),
            ..spec()
        };
        let dir = tempfile::tempdir().unwrap();
        fs::write(config_path(dir.path()), EXISTING).unwrap();

        // `--yes` чужую запись не перезаписывает.
        let mut ui = ScriptedUi::default();
        let err = register(&mut ui, dir.path(), &foreign, true).unwrap_err();
        assert!(err.to_string().contains("--name"), "{err}");
        assert_eq!(
            fs::read_to_string(config_path(dir.path())).unwrap(),
            EXISTING
        );
        assert!(ui.text().contains("another server, not wiki-mcp"));

        let mut ui = answers(&["n"]);
        assert!(!register(&mut ui, dir.path(), &foreign, false).unwrap());
        assert_eq!(
            fs::read_to_string(config_path(dir.path())).unwrap(),
            EXISTING
        );

        let mut ui = answers(&["y"]);
        assert!(register(&mut ui, dir.path(), &foreign, false).unwrap());
        let text = fs::read_to_string(config_path(dir.path())).unwrap();
        assert!(!text.contains("localhost:9903"), "{text}");
        assert!(text.contains("[mcp_servers.wiki]"), "{text}");
    }

    #[test]
    fn rejects_non_table_mcp_servers() {
        let mut doc: DocumentMut = "mcp_servers = 1\n".parse().unwrap();
        assert!(apply_entry(&mut doc, &spec()).is_err());
    }
}
