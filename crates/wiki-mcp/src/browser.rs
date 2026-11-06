//! Открытие страниц Яндекс OAuth в браузере по умолчанию.
//!
//! Решение, открывать ли браузер, — чистая функция над флагом и снимком окружения
//! ([`decide`]), чтобы её можно было проверить тестами, не запуская браузер.

use std::ffi::OsString;
use std::io;
use std::process::{Command, Stdio};

/// Снимок окружения, от которого зависит решение открыть браузер.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserEnv {
    /// `std::env::consts::OS`: `macos`, `linux`, `windows`, …
    pub os: &'static str,
    /// Сессия по SSH (`SSH_CONNECTION` или `SSH_TTY`): браузер открылся бы на удалённой машине.
    pub ssh: bool,
    /// Есть графическая сессия (`DISPLAY` или `WAYLAND_DISPLAY`); учитывается только вне
    /// macOS и Windows.
    pub display: bool,
}

impl BrowserEnv {
    /// Окружение текущего процесса.
    pub fn current() -> Self {
        Self::from_vars(std::env::consts::OS, |name| std::env::var_os(name))
    }

    /// Окружение по функции чтения переменных; пустое значение считается незаданным.
    pub fn from_vars(os: &'static str, var: impl Fn(&str) -> Option<OsString>) -> Self {
        let set = |name: &str| var(name).is_some_and(|value| !value.is_empty());
        Self {
            os,
            ssh: set("SSH_CONNECTION") || set("SSH_TTY"),
            display: set("DISPLAY") || set("WAYLAND_DISPLAY"),
        }
    }
}

/// Открывать ли браузер.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserDecision {
    /// Открыть ссылку в браузере по умолчанию.
    Open,
    /// Только напечатать ссылку.
    Skip(SkipReason),
}

/// Почему браузер не открывается.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Флаг `--no-browser`.
    Flag,
    /// Сессия по SSH.
    Ssh,
    /// Нет графической сессии.
    NoDisplay,
}

/// Решает, открывать ли браузер: не открываем по `--no-browser`, по SSH и без графической
/// сессии (вне macOS и Windows).
pub fn decide(no_browser: bool, env: &BrowserEnv) -> BrowserDecision {
    if no_browser {
        BrowserDecision::Skip(SkipReason::Flag)
    } else if env.ssh {
        BrowserDecision::Skip(SkipReason::Ssh)
    } else if !matches!(env.os, "macos" | "windows") && !env.display {
        BrowserDecision::Skip(SkipReason::NoDisplay)
    } else {
        BrowserDecision::Open
    }
}

/// Команда, которая открывает `url` в браузере по умолчанию: программа и её аргументы.
///
/// Без оболочки: URL передаётся отдельным аргументом. На Windows вместо `cmd /C start`
/// используется `rundll32 url.dll,FileProtocolHandler`: в ссылке авторизации есть `&`,
/// который `cmd` принял бы за разделитель команд.
pub fn open_command(os: &str, url: &str) -> (&'static str, Vec<String>) {
    match os {
        "macos" => ("open", vec![url.to_owned()]),
        "windows" => (
            "rundll32",
            vec!["url.dll,FileProtocolHandler".into(), url.to_owned()],
        ),
        _ => ("xdg-open", vec![url.to_owned()]),
    }
}

/// Запускает браузер и не ждёт его: stdin, stdout и stderr отключены, чтобы браузер не
/// писал в терминал и не мешал скрытому вводу токена.
pub fn open_in_browser(url: &str) -> io::Result<()> {
    let (program, args) = open_command(std::env::consts::OS, url);
    let mut child = Command::new(program)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Дожидаемся завершения в фоне, чтобы не оставлять зомби-процесс, пока ждём ввод токена.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(os: &'static str, vars: &[(&str, &str)]) -> BrowserEnv {
        BrowserEnv::from_vars(os, |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        })
    }

    #[test]
    fn opens_on_desktop() {
        assert_eq!(decide(false, &env("macos", &[])), BrowserDecision::Open);
        assert_eq!(decide(false, &env("windows", &[])), BrowserDecision::Open);
        for display in ["DISPLAY", "WAYLAND_DISPLAY"] {
            assert_eq!(
                decide(false, &env("linux", &[(display, ":0")])),
                BrowserDecision::Open
            );
        }
    }

    #[test]
    fn flag_wins() {
        assert_eq!(
            decide(true, &env("macos", &[])),
            BrowserDecision::Skip(SkipReason::Flag)
        );
    }

    #[test]
    fn skips_over_ssh() {
        for ssh in ["SSH_CONNECTION", "SSH_TTY"] {
            for os in ["macos", "linux", "windows"] {
                assert_eq!(
                    decide(false, &env(os, &[(ssh, "x"), ("DISPLAY", ":0")])),
                    BrowserDecision::Skip(SkipReason::Ssh),
                    "{os} {ssh}"
                );
            }
        }
    }

    #[test]
    fn skips_without_display_on_linux() {
        assert_eq!(
            decide(false, &env("linux", &[])),
            BrowserDecision::Skip(SkipReason::NoDisplay)
        );
        // Пустое значение — как незаданное.
        assert_eq!(
            decide(false, &env("freebsd", &[("DISPLAY", "")])),
            BrowserDecision::Skip(SkipReason::NoDisplay)
        );
    }

    #[test]
    fn open_commands() {
        let url = "https://oauth.yandex.ru/authorize?response_type=token&client_id=x";
        assert_eq!(open_command("macos", url), ("open", vec![url.to_owned()]));
        assert_eq!(
            open_command("linux", url),
            ("xdg-open", vec![url.to_owned()])
        );
        assert_eq!(
            open_command("windows", url),
            (
                "rundll32",
                vec!["url.dll,FileProtocolHandler".into(), url.to_owned()]
            )
        );
    }
}
