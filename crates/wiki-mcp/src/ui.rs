//! Взаимодействие с пользователем в интерактивных командах (`auth`, `connect`).
//!
//! Вынесено в трейт, чтобы сценарии можно было тестировать без терминала.

use std::io::{self, BufRead, Read, Write};

use wiki_api::ApiToken;

/// Предел размера токена из stdin: OAuth-токен — несколько десятков символов, IAM-токен — сотни.
const MAX_STDIN_TOKEN_BYTES: u64 = 16 * 1024;

pub trait Ui {
    /// Выводит строку пользователю.
    fn say(&mut self, line: &str);
    /// Скрытый ввод токена: значение не отображается в терминале.
    fn read_token(&mut self, prompt: &str) -> io::Result<ApiToken>;
    /// Токен из stdin (`--token-stdin`): весь ввод до EOF.
    fn read_token_stdin(&mut self) -> io::Result<ApiToken>;
    /// Обычный (видимый) ввод строки, например ID организации.
    fn ask(&mut self, prompt: &str) -> io::Result<String>;
    /// Вопрос «да/нет»; пустой ответ — `default`. EOF на stdin считается отказом.
    fn confirm(&mut self, question: &str, default: bool) -> io::Result<bool>;
    /// Открывает ссылку в браузере по умолчанию, не дожидаясь его.
    fn open_url(&mut self, url: &str) -> io::Result<()>;
}

/// Разбирает ответ на вопрос «да/нет». `None` — ответ не распознан.
pub fn parse_yes_no(answer: &str, default: bool) -> Option<bool> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "" => Some(default),
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

/// Терминал: вывод в stdout, токен через `rpassword` (читает напрямую из TTY без эха).
#[derive(Debug, Default)]
pub struct TerminalUi;

impl Ui for TerminalUi {
    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn read_token(&mut self, prompt: &str) -> io::Result<ApiToken> {
        let value = rpassword::prompt_password(prompt)?;
        Ok(ApiToken::new(value.trim()))
    }

    fn read_token_stdin(&mut self) -> io::Result<ApiToken> {
        let mut value = String::new();
        io::stdin()
            .lock()
            .take(MAX_STDIN_TOKEN_BYTES + 1)
            .read_to_string(&mut value)?;
        if value.len() as u64 > MAX_STDIN_TOKEN_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "stdin is too large for a token",
            ));
        }
        Ok(ApiToken::new(value.trim()))
    }

    fn ask(&mut self, prompt: &str) -> io::Result<String> {
        print!("{prompt}");
        io::stdout().flush()?;
        let mut answer = String::new();
        if io::stdin().lock().read_line(&mut answer)? == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "no answer on stdin",
            ));
        }
        Ok(answer.trim().to_owned())
    }

    fn confirm(&mut self, question: &str, default: bool) -> io::Result<bool> {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        loop {
            print!("{question} {hint} ");
            io::stdout().flush()?;
            let mut answer = String::new();
            // EOF (например, stdin не терминал) считается отказом, а не ответом по умолчанию.
            if io::stdin().lock().read_line(&mut answer)? == 0 {
                println!();
                return Ok(false);
            }
            match parse_yes_no(&answer, default) {
                Some(value) => return Ok(value),
                None => println!("Please answer y or n."),
            }
        }
    }

    fn open_url(&mut self, url: &str) -> io::Result<()> {
        crate::browser::open_in_browser(url)
    }
}

#[cfg(test)]
pub mod testing {
    use std::collections::VecDeque;

    use super::*;

    /// Сценарий ввода для тестов: заранее заданные токены и ответы, весь вывод сохраняется.
    #[derive(Debug, Default)]
    pub struct ScriptedUi {
        /// Токены для скрытого ввода.
        pub tokens: VecDeque<String>,
        /// Содержимое stdin для `--token-stdin`.
        pub stdin_token: Option<String>,
        /// Ответы на видимые вопросы и на вопросы «да/нет» (`y`, `n`, пусто), по порядку.
        pub answers: VecDeque<String>,
        pub output: Vec<String>,
        pub questions: Vec<String>,
        /// Ссылки, которые сценарий «открыл» в браузере (настоящий браузер не запускается).
        pub opened: Vec<String>,
        /// Открытие браузера завершается ошибкой.
        pub open_fails: bool,
    }

    impl ScriptedUi {
        pub fn text(&self) -> String {
            self.output.join("\n")
        }
    }

    impl Ui for ScriptedUi {
        fn say(&mut self, line: &str) {
            self.output.push(line.to_owned());
        }

        fn read_token(&mut self, _prompt: &str) -> io::Result<ApiToken> {
            self.tokens
                .pop_front()
                .map(|value| ApiToken::new(value.trim()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no token scripted"))
        }

        fn read_token_stdin(&mut self) -> io::Result<ApiToken> {
            self.stdin_token
                .take()
                .map(|value| ApiToken::new(value.trim()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no stdin scripted"))
        }

        fn ask(&mut self, prompt: &str) -> io::Result<String> {
            self.questions.push(prompt.to_owned());
            self.answers
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "no answer scripted"))
        }

        fn confirm(&mut self, question: &str, default: bool) -> io::Result<bool> {
            let answer = self.ask(question)?;
            parse_yes_no(&answer, default).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unexpected yes/no answer")
            })
        }

        fn open_url(&mut self, url: &str) -> io::Result<()> {
            if self.open_fails {
                return Err(io::Error::new(io::ErrorKind::NotFound, "no browser"));
            }
            self.opened.push(url.to_owned());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yes_no_answers() {
        assert_eq!(parse_yes_no("", true), Some(true));
        assert_eq!(parse_yes_no(" ", false), Some(false));
        assert_eq!(parse_yes_no("Y", false), Some(true));
        assert_eq!(parse_yes_no("yes", false), Some(true));
        assert_eq!(parse_yes_no("No", true), Some(false));
        assert_eq!(parse_yes_no("maybe", true), None);
    }
}
