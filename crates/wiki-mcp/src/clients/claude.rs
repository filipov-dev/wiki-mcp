//! Claude Code: регистрация через `claude mcp add`.
//!
//! Конфигурацию Claude Code (`~/.claude.json`, `.mcp.json`) напрямую не правим — только через
//! его CLI, чтобы не зависеть от внутреннего формата файлов и не задеть чужие записи.

use std::io;
use std::process::Command;

use anyhow::{Context, bail};

use super::{Scope, ServerSpec, is_own_command, shell_join};
use crate::ui::Ui;

/// Имя исполняемого файла Claude Code CLI.
pub const CLAUDE_PROGRAM: &str = "claude";

/// Результат внешней команды.
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Запуск внешних команд; в тестах подменяется записью вызовов.
pub trait CommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput>;
}

/// Запуск через `std::process::Command`.
#[derive(Debug, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
        let output = match Command::new(program).args(args).output() {
            // На Windows `Command` ищет в PATH только `<имя>.exe`, а установка Claude Code
            // через npm кладёт обёртку `claude.cmd`. Rust экранирует аргументы для `.cmd`
            // сам и отказывается запускать, если безопасно экранировать нельзя.
            Err(err) if cfg!(windows) && err.kind() == io::ErrorKind::NotFound => {
                Command::new(format!("{program}.cmd")).args(args).output()?
            }
            result => result?,
        };
        Ok(CommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Аргументы `claude mcp add --scope <scope> <name> -- <exe> serve --profile <p> ...`.
pub fn add_args(scope: Scope, spec: &ServerSpec) -> Vec<String> {
    let mut args = strings(&[
        "mcp",
        "add",
        "--scope",
        scope.as_str(),
        spec.name.as_str(),
        "--",
    ]);
    args.push(spec.command.clone());
    args.extend(spec.args.iter().cloned());
    args
}

/// Аргументы `claude mcp remove --scope <scope> <name>`.
pub fn remove_args(scope: Scope, name: &str) -> Vec<String> {
    strings(&["mcp", "remove", "--scope", scope.as_str(), name])
}

/// Аргументы `claude mcp get <name>`.
pub fn get_args(name: &str) -> Vec<String> {
    strings(&["mcp", "get", name])
}

fn display(args: &[String]) -> String {
    shell_join(std::iter::once(CLAUDE_PROGRAM).chain(args.iter().map(String::as_str)))
}

/// Проверяет, что Claude Code CLI доступен, до того как пользователь введёт токен.
pub fn ensure_available(runner: &mut dyn CommandRunner) -> anyhow::Result<()> {
    match runner.run(CLAUDE_PROGRAM, &["--version".to_owned()]) {
        Ok(output) if output.success => Ok(()),
        Ok(output) => bail!(
            "`{CLAUDE_PROGRAM} --version` failed: {}",
            output.stderr.trim()
        ),
        Err(err) if err.kind() == io::ErrorKind::NotFound => bail!(
            "Claude Code CLI `{CLAUDE_PROGRAM}` was not found in PATH; install Claude Code first"
        ),
        Err(err) => Err(err).context("failed to run Claude Code CLI"),
    }
}

/// Сообщение Claude Code о том, что сервер с таким именем уже есть.
fn already_exists(output: &CommandOutput) -> bool {
    let text = format!("{}\n{}", output.stdout, output.stderr).to_ascii_lowercase();
    text.contains("already exists")
}

/// Запускает ли существующая запись `wiki-mcp`: по строке `Command:` в выводе
/// `claude mcp get`. Если вывод не разобрать, запись считается чужой.
fn existing_is_own(output: &CommandOutput) -> bool {
    output.success
        && output.stdout.lines().any(|line| {
            line.trim()
                .strip_prefix("Command:")
                .is_some_and(|command| is_own_command(command.trim()))
        })
}

fn run_checked(runner: &mut dyn CommandRunner, args: &[String]) -> anyhow::Result<CommandOutput> {
    runner
        .run(CLAUDE_PROGRAM, args)
        .with_context(|| format!("failed to run `{}`", display(args)))
}

/// Регистрирует сервер в Claude Code. Возвращает `false`, если пользователь отказался.
///
/// Запись с тем же именем, которая запускает `wiki-mcp`, заменяется после подтверждения
/// (или сразу с `yes`). Чужая запись (другой сервер с тем же именем) заменяется только после
/// явного ответа в терминале: `yes` её не перезаписывает.
pub fn register(
    runner: &mut dyn CommandRunner,
    ui: &mut dyn Ui,
    scope: Scope,
    spec: &ServerSpec,
    yes: bool,
) -> anyhow::Result<bool> {
    let name = spec.name.as_str();
    let add = add_args(scope, spec);
    ui.say(&format!(
        "Claude Code ({} scope) will get MCP server `{name}`:\n  {}",
        scope.as_str(),
        display(&add)
    ));
    if !yes && !ui.confirm("Register the server in Claude Code?", false)? {
        return Ok(false);
    }

    let output = run_checked(runner, &add)?;
    if output.success {
        return Ok(true);
    }
    if !already_exists(&output) {
        bail!("`{}` failed: {}", display(&add), output.stderr.trim());
    }

    let remove = remove_args(scope, name);
    let existing = run_checked(runner, &get_args(name))?;
    if existing_is_own(&existing) {
        ui.say(&format!(
            "Claude Code already has MCP server `{name}` (wiki-mcp) in {} scope. \
             It will be replaced:\n  {}\n  {}",
            scope.as_str(),
            display(&remove),
            display(&add)
        ));
        if !yes && !ui.confirm("Replace the existing entry?", false)? {
            return Ok(false);
        }
    } else {
        ui.say(&format!(
            "Claude Code already has an MCP server named `{name}` that does not run \
             wiki-mcp. Replacing it removes that server from Claude Code:\n  {}\n  {}\n\
             To keep it, re-run connect with another --name.",
            display(&remove),
            display(&add)
        ));
        if yes {
            bail!(
                "Claude Code already has another MCP server named `{name}`; it was not \
                 changed. Choose another name with --name, or re-run without --yes to replace it"
            );
        }
        if !ui.confirm(&format!("Replace the other server `{name}`?"), false)? {
            return Ok(false);
        }
    }
    let output = run_checked(runner, &remove)?;
    if !output.success {
        bail!("`{}` failed: {}", display(&remove), output.stderr.trim());
    }
    let output = run_checked(runner, &add)?;
    if !output.success {
        bail!("`{}` failed: {}", display(&add), output.stderr.trim());
    }
    Ok(true)
}

#[cfg(test)]
pub mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// Записывает вызовы и возвращает заранее заданные результаты; `claude` не запускается.
    #[derive(Debug, Default)]
    pub struct FakeRunner {
        pub calls: Vec<Vec<String>>,
        pub results: VecDeque<CommandOutput>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&mut self, program: &str, args: &[String]) -> io::Result<CommandOutput> {
            assert_eq!(program, CLAUDE_PROGRAM);
            self.calls.push(args.to_vec());
            Ok(self.results.pop_front().unwrap_or(CommandOutput {
                success: true,
                ..Default::default()
            }))
        }
    }

    /// Ответ `claude mcp add`, когда сервер с таким именем уже есть.
    pub fn exists() -> CommandOutput {
        CommandOutput {
            success: false,
            stderr: "MCP server wiki already exists in user config".into(),
            ..Default::default()
        }
    }

    /// Ответ `claude mcp get` для записи с командой `command`.
    pub fn get_output(command: &str) -> CommandOutput {
        CommandOutput {
            success: true,
            stdout: format!(
                "wiki:\n  Scope: User config (available in all your projects)\n  \
                 Type: stdio\n  Command: {command}\n  Args: serve --profile claude-code\n"
            ),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{FakeRunner, exists, get_output};
    use super::*;
    use crate::clients::TEST_EXE;
    use crate::ui::testing::ScriptedUi;

    fn spec() -> ServerSpec {
        ServerSpec {
            name: "wiki".into(),
            command: TEST_EXE.into(),
            args: vec!["serve".into(), "--profile".into(), "claude-code".into()],
        }
    }

    fn answers(values: &[&str]) -> ScriptedUi {
        ScriptedUi {
            answers: values.iter().map(|value| (*value).to_owned()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn builds_argv() {
        assert_eq!(
            add_args(Scope::User, &spec()),
            [
                "mcp",
                "add",
                "--scope",
                "user",
                "wiki",
                "--",
                TEST_EXE,
                "serve",
                "--profile",
                "claude-code"
            ]
        );
        assert_eq!(
            remove_args(Scope::Project, "wiki"),
            ["mcp", "remove", "--scope", "project", "wiki"]
        );
        assert_eq!(get_args("wiki"), ["mcp", "get", "wiki"]);
    }

    #[test]
    fn register_with_confirmation() {
        let mut runner = FakeRunner::default();
        let mut ui = answers(&["y"]);
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
        assert_eq!(runner.calls, [add_args(Scope::User, &spec())]);
        assert!(ui.text().contains("claude mcp add --scope user wiki --"));
    }

    #[test]
    fn declined_or_unanswered_registration_runs_nothing() {
        for ui in [answers(&["n"]), answers(&[""])] {
            let mut ui = ui;
            let mut runner = FakeRunner::default();
            assert!(!register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
            assert!(runner.calls.is_empty());
        }
        // Нет ответа (EOF) — ошибка ввода, конфигурация не меняется.
        let mut runner = FakeRunner::default();
        let mut ui = ScriptedUi::default();
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), false).is_err());
        assert!(runner.calls.is_empty());
    }

    #[test]
    fn replaces_own_entry() {
        let mut runner = FakeRunner {
            results: [exists(), get_output("/opt/homebrew/bin/wiki-mcp")].into(),
            ..Default::default()
        };
        let mut ui = ScriptedUi::default();
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), true).unwrap());
        assert_eq!(
            runner.calls,
            [
                add_args(Scope::User, &spec()),
                get_args("wiki"),
                remove_args(Scope::User, "wiki"),
                add_args(Scope::User, &spec())
            ]
        );
        assert!(ui.questions.is_empty());
    }

    #[test]
    fn foreign_entry_is_not_replaced_with_yes() {
        let mut runner = FakeRunner {
            results: [exists(), get_output("node")].into(),
            ..Default::default()
        };
        let mut ui = ScriptedUi::default();
        let err = register(&mut runner, &mut ui, Scope::User, &spec(), true).unwrap_err();
        assert!(err.to_string().contains("--name"), "{err}");
        assert_eq!(
            runner.calls,
            [add_args(Scope::User, &spec()), get_args("wiki")]
        );

        // Интерактивно — только после явного «да»; отказ ничего не удаляет.
        let mut runner = FakeRunner {
            results: [exists(), get_output("node")].into(),
            ..Default::default()
        };
        let mut ui = answers(&["y", "n"]);
        assert!(!register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
        assert_eq!(runner.calls.len(), 2);
        assert!(ui.text().contains("does not run wiki-mcp"), "{}", ui.text());

        let mut runner = FakeRunner {
            results: [exists(), get_output("node")].into(),
            ..Default::default()
        };
        let mut ui = answers(&["y", "y"]);
        assert!(register(&mut runner, &mut ui, Scope::User, &spec(), false).unwrap());
        assert_eq!(runner.calls.len(), 4);
    }

    #[test]
    fn other_failures_are_errors() {
        let mut runner = FakeRunner {
            results: [CommandOutput {
                success: false,
                stderr: "boom".into(),
                ..Default::default()
            }]
            .into(),
            ..Default::default()
        };
        let mut ui = ScriptedUi::default();
        let err = register(&mut runner, &mut ui, Scope::User, &spec(), true).unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert_eq!(runner.calls.len(), 1);
    }
}
