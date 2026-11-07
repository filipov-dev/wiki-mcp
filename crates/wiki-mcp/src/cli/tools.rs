//! `wiki-mcp tools` — пользовательские инструменты из `tools.toml`: загрузка для `serve`,
//! список и проверка файлов.
//!
//! Файл пользователя `tools.toml` лежит в каталоге конфигурации рядом с `profiles.toml`
//! (`~/.config/wiki-mcp/tools.toml`, с учётом `XDG_CONFIG_HOME` и `%APPDATA%`) и читается
//! в stdio-режиме всегда, если он есть. Другие файлы (например, из репозитория проекта) —
//! только по явному `--tools-file`: файл из клонированного репозитория — недоверенный ввод и
//! не должен молча добавлять инструменты. HTTP-режим читает только `--tools-file`: он
//! собирается из флагов, без каталога конфигурации.
//!
//! Проверку определений и имён выполняет ядро (`wiki_mcp_core::custom_tools`).

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use clap::{Args, Subcommand};
use wiki_mcp_core::custom_tools::check_names;
use wiki_mcp_core::{CustomTool, CustomToolsFile, access_name};

use crate::profiles::ProfileRepo;

/// Имя файла пользовательских инструментов в каталоге конфигурации.
pub const TOOLS_FILE: &str = "tools.toml";

#[derive(Debug, Subcommand)]
pub enum ToolsCommand {
    /// List the custom tools defined in the user file and in --tools-file files.
    List(FilesArgs),
    /// Validate custom tool files: operations, fixed arguments, `jq` filters and names.
    Check(FilesArgs),
}

#[derive(Debug, Args)]
#[command(after_help = "\
The user file tools.toml next to profiles.toml (~/.config/wiki-mcp/tools.toml) is always \
read when it exists. Pass the same --tools-file options as to `serve` to include more files.")]
pub struct FilesArgs {
    /// Additional custom tools file, as passed to `serve --tools-file`. Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
}

pub fn run(command: ToolsCommand) -> anyhow::Result<()> {
    let repo = ProfileRepo::open_default()?;
    let default = user_tools_file(&repo);
    match command {
        ToolsCommand::List(args) => {
            let tools = load(Some(&default), &args.tools_file)?;
            print!("{}", render(&tools, &default));
        }
        ToolsCommand::Check(args) => {
            let tools = load(Some(&default), &args.tools_file)?;
            print!("{}", check_report(&tools, &default, &args.tools_file));
        }
    }
    Ok(())
}

/// Файл пользователя: `tools.toml` в каталоге файла профилей.
pub fn user_tools_file(repo: &ProfileRepo) -> PathBuf {
    repo.path()
        .parent()
        .map_or_else(|| PathBuf::from(TOOLS_FILE), |dir| dir.join(TOOLS_FILE))
}

/// Загружает инструменты из файла пользователя `default` (если он есть) и из `extra`
/// (обязательны). Один и тот же файл, переданный дважды, читается один раз.
pub fn load(default: Option<&Path>, extra: &[PathBuf]) -> anyhow::Result<Vec<CustomTool>> {
    let default = default.filter(|path| path.exists());
    let mut tools = Vec::new();
    let mut seen = Vec::new();
    for path in default
        .into_iter()
        .chain(extra.iter().map(PathBuf::as_path))
    {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        tools.extend(load_file(path)?);
    }
    check_names(&tools).map_err(|err| anyhow!(err))?;
    Ok(tools)
}

fn load_file(path: &Path) -> anyhow::Result<Vec<CustomTool>> {
    let source = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the custom tools file {source}"))?;
    let file: CustomToolsFile =
        toml::from_str(&text).map_err(|err| anyhow!("{source}: invalid tools file: {err}"))?;
    file.tools
        .into_iter()
        .map(|def| CustomTool::compile(def, &source).map_err(|err| anyhow!(err)))
        .collect()
}

/// Таблица инструментов для `tools list`.
fn render(tools: &[CustomTool], default: &Path) -> String {
    if tools.is_empty() {
        return format!(
            "No custom tools defined (user file: {}).\n",
            default.display()
        );
    }
    let width = tools
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(0)
        .max("TOOL".len());
    let mut out = format!("{:<width$}  {:<11}  OPERATION\n", "TOOL", "ACCESS");
    for tool in tools {
        let mut details = Vec::new();
        if !tool.fixed_args().is_empty() {
            let names: Vec<&str> = tool.fixed_args().keys().map(String::as_str).collect();
            details.push(format!("fixed: {}", names.join(", ")));
        }
        if tool.jq().is_some() {
            details.push("jq".to_owned());
        }
        details.push(tool.source.clone());
        out.push_str(&format!(
            "{:<width$}  {:<11}  {}  ({})\n",
            tool.name,
            access_name(tool.access()),
            tool.operation.tool_name,
            details.join("; ")
        ));
    }
    out.push_str(
        "\nTools above the server's access level (--allow-write, --allow-destructive) are not \
         registered.\n",
    );
    out
}

/// Итог `tools check`: какие файлы проверены и сколько в них инструментов.
fn check_report(tools: &[CustomTool], default: &Path, extra: &[PathBuf]) -> String {
    let mut out = String::new();
    let mut files: Vec<String> = Vec::new();
    if default.exists() {
        files.push(default.display().to_string());
    } else {
        out.push_str(&format!(
            "User file {} does not exist.\n",
            default.display()
        ));
    }
    for path in extra {
        let file = path.display().to_string();
        if !files.contains(&file) {
            files.push(file);
        }
    }
    for file in &files {
        let count = tools.iter().filter(|tool| &tool.source == file).count();
        out.push_str(&format!("{file}: OK, {count} tool(s)\n"));
    }
    if !tools.is_empty() {
        out.push('\n');
        out.push_str(&render(tools, default));
    }
    out
}

#[cfg(test)]
mod tests {
    use wiki_api::AccessLevel;

    use super::*;

    const SAMPLE: &str = r#"
[[tools]]
name = "team_pages"
description = "Pages of the team section"
operation = "list_page_descendants"
jq = ".results[] | {slug, title}"

[tools.args]
slug = "team/project"
page_size = 50

[[tools]]
name = "comment_on_home"
description = "Comment on the home page"
operation = "create_comment"
args = { idx = 42 }
"#;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn user_file_is_next_to_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        assert_eq!(user_tools_file(&repo), dir.path().join("tools.toml"));
    }

    #[test]
    fn loads_and_renders() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "tools.toml", SAMPLE);
        let tools = load(Some(&path), &[]).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].access(), AccessLevel::Read);
        assert_eq!(tools[1].access(), AccessLevel::Write);
        let table = render(&tools, &path);
        assert!(table.contains("team_pages"), "{table}");
        assert!(table.contains("list_page_descendants"), "{table}");
        assert!(table.contains("fixed: page_size, slug; jq"), "{table}");
        assert!(table.contains("write"), "{table}");
        assert!(render(&[], &path).contains("No custom tools"));

        let report = check_report(&tools, &path, &[]);
        assert!(report.contains("OK, 2 tool(s)"), "{report}");

        let missing = dir.path().join("none.toml");
        assert!(load(Some(&missing), &[]).unwrap().is_empty());
        assert!(check_report(&[], &missing, &[]).contains("does not exist"));
    }

    #[test]
    fn reports_errors_with_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let bad_toml = write(dir.path(), "a.toml", "[[tools]\nname = 1");
        let err = load(None, &[bad_toml]).unwrap_err().to_string();
        assert!(err.contains("a.toml: invalid tools file"), "{err}");

        let unknown = write(
            dir.path(),
            "b.toml",
            "[[tools]]\nname = \"x\"\ndescription = \"d\"\noperation = \"get_page\"\n\
             headers = { Authorization = \"x\" }\n",
        );
        let err = load(None, &[unknown]).unwrap_err().to_string();
        assert!(err.contains("unknown field `headers`"), "{err}");

        let bad_op = write(
            dir.path(),
            "c.toml",
            "[[tools]]\nname = \"x\"\ndescription = \"d\"\noperation = \"get_everything\"\n",
        );
        let err = load(None, &[bad_op]).unwrap_err().to_string();
        assert!(err.contains("c.toml: tool `x`: unknown operation"), "{err}");

        let missing = dir.path().join("missing.toml");
        let err = load(None, &[missing]).unwrap_err().to_string();
        assert!(err.contains("cannot read"), "{err}");

        // Файл пользователя, переданный ещё и через `--tools-file`, читается один раз.
        let user = write(dir.path(), "tools.toml", SAMPLE);
        assert_eq!(
            load(Some(&user), std::slice::from_ref(&user))
                .unwrap()
                .len(),
            2
        );
        let project = write(dir.path(), "project.toml", SAMPLE);
        let err = load(Some(&user), &[project]).unwrap_err().to_string();
        assert!(err.contains("defined more than once"), "{err}");

        let builtin = write(
            dir.path(),
            "d.toml",
            "[[tools]]\nname = \"get_page\"\ndescription = \"d\"\noperation = \"get_page\"\n",
        );
        let err = load(None, &[builtin]).unwrap_err().to_string();
        assert!(err.contains("same name as a built-in tool"), "{err}");
    }
}
