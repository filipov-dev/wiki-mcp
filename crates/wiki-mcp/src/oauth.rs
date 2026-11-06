//! OAuth-приложение пользователя в Яндекс OAuth: `ClientID` и ссылки для получения токена.
//!
//! Своего OAuth-приложения в бинарнике нет: пользователь создаёт его сам, а `connect`
//! подсказывает настройки и сохраняет `ClientID` (он не секретный) в профиль. Токен
//! пользователь получает вручную по ссылке авторизации и вставляет скрытым вводом.

use anyhow::{Context, bail};
use wiki_api::ApiToken;

use crate::browser::{BrowserDecision, SkipReason};
use crate::login::validate_token;
use crate::ui::Ui;

/// Страница создания OAuth-приложения.
pub const CLIENT_NEW_URL: &str = "https://oauth.yandex.ru/client/new/";

/// Redirect URI для получения токена вручную: Яндекс OAuth показывает токен на своей странице.
pub const VERIFICATION_REDIRECT_URI: &str = "https://oauth.yandex.ru/verification_code";

/// Адрес авторизации Яндекс OAuth.
pub const AUTHORIZE_URL: &str = "https://oauth.yandex.ru/authorize";

/// Справочник Wiki API.
pub const WIKI_API_DOCS_URL: &str = "https://yandex.ru/support/wiki/ru/api-ref/";

/// Длина `ClientID` Яндекс OAuth: 32 шестнадцатеричных символа.
const CLIENT_ID_LEN: usize = 32;

/// Сколько раз переспрашивать `ClientID` неверного формата.
const CLIENT_ID_ATTEMPTS: usize = 3;

/// Проверяет `ClientID` и приводит его к нижнему регистру. Значение подставляется в ссылку
/// авторизации и открывается в браузере, поэтому допускаются только 32 hex-символа.
pub fn normalize_client_id(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.len() != CLIENT_ID_LEN || !value.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!(
            "invalid ClientID: expected {CLIENT_ID_LEN} hexadecimal characters, as shown on the \
             OAuth application page"
        );
    }
    Ok(value.to_ascii_lowercase())
}

/// Ссылка, по которой Яндекс OAuth выдаёт токен для приложения `client_id`.
///
/// `client_id` должен быть проверен [`normalize_client_id`].
pub fn authorize_url(client_id: &str) -> String {
    format!("{AUTHORIZE_URL}?response_type=token&client_id={client_id}")
}

/// Инструкция по созданию OAuth-приложения для профиля.
pub fn app_instructions(profile: &str, allow_write: bool) -> String {
    let permissions = if allow_write {
        "Yandex Wiki read (wiki:read) and write (wiki:write)"
    } else {
        "Yandex Wiki read (wiki:read); add wiki:write only if the agent will change data \
         (connect --allow-write)"
    };
    format!(
        "Profile `{profile}` has no OAuth application yet. Create your own one in Yandex OAuth:\n\
         \x20 {CLIENT_NEW_URL}\n\
         \x20 - Type: \"For API access or debugging\" (\"Для доступа к API или отладки\")\n\
         \x20 - Name: for example wiki-mcp-{profile}; contact email: yours\n\
         \x20 - Permissions: {permissions}\n\
         \x20 - Redirect URI, if the form asks for it (platform \"Web services\"): \
         {VERIFICATION_REDIRECT_URI}\n\
         \x20 Click \"Create app\" and copy the ClientID from the application page.\n\
         \x20 The ClientID is not secret and is saved in the profile; the Client secret is not \
         needed.\n\
         \x20 Use a separate application for every agent, so each token can be revoked on its \
         own.\n\
         \x20 Reference: {WIKI_API_DOCS_URL}"
    )
}

/// Открывает ссылку в браузере, если `browser` разрешает, и сообщает, что получилось.
/// Сама ссылка печатается вызывающим кодом всегда, чтобы её можно было скопировать.
pub fn open_link(ui: &mut dyn Ui, url: &str, browser: BrowserDecision) {
    match browser {
        BrowserDecision::Open => match ui.open_url(url) {
            Ok(()) => ui.say("Opened the link above in your browser."),
            Err(err) => ui.say(&format!(
                "Could not open a browser ({err}); open the link above yourself."
            )),
        },
        BrowserDecision::Skip(SkipReason::Ssh) => {
            ui.say("SSH session: open the link above in a browser on your computer.");
        }
        BrowserDecision::Skip(SkipReason::NoDisplay) => {
            ui.say("No graphical session: open the link above in a browser.");
        }
        BrowserDecision::Skip(SkipReason::Flag) => {}
    }
}

/// Создание OAuth-приложения: инструкция, страница создания в браузере, ввод `ClientID`.
pub fn ask_client_id(
    ui: &mut dyn Ui,
    profile: &str,
    allow_write: bool,
    browser: BrowserDecision,
) -> anyhow::Result<String> {
    ui.say(&app_instructions(profile, allow_write));
    open_link(ui, CLIENT_NEW_URL, browser);
    let mut attempt = 1;
    loop {
        let answer = ui
            .ask("ClientID of the OAuth application: ")
            .context("failed to read the ClientID")?;
        match normalize_client_id(&answer) {
            Ok(client_id) => return Ok(client_id),
            Err(err) if attempt < CLIENT_ID_ATTEMPTS => {
                ui.say(&format!("{err:#}; try again."));
                attempt += 1;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Новый токен: ссылка авторизации OAuth-приложения в браузере и скрытый ввод.
pub fn prompt_token(
    ui: &mut dyn Ui,
    profile: &str,
    client_id: &str,
    browser: BrowserDecision,
) -> anyhow::Result<ApiToken> {
    let url = authorize_url(client_id);
    ui.say(&format!(
        "Get a token for profile `{profile}` from your OAuth application: sign in with the \
         Yandex account the agent will work as, allow access and copy the token:\n  {url}"
    ));
    open_link(ui, &url, browser);
    let token = ui
        .read_token(&format!(
            "Paste the token for profile `{profile}` (input is hidden): "
        ))
        .context("failed to read the token")?;
    validate_token(&token)?;
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testing::ScriptedUi;

    const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn client_id_format() {
        assert_eq!(normalize_client_id(CLIENT_ID).unwrap(), CLIENT_ID);
        assert_eq!(
            normalize_client_id(&format!(" {} ", CLIENT_ID.to_uppercase())).unwrap(),
            CLIENT_ID
        );
        for bad in [
            "",
            "0123456789abcdef",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdef0",
            "0123456789abcdef&123456789abcdef",
        ] {
            assert!(normalize_client_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn authorize_link() {
        assert_eq!(
            authorize_url(CLIENT_ID),
            format!("https://oauth.yandex.ru/authorize?response_type=token&client_id={CLIENT_ID}")
        );
    }

    #[test]
    fn instructions_name_permissions_and_redirect() {
        let read = app_instructions("codex", false);
        assert!(read.contains(CLIENT_NEW_URL));
        assert!(read.contains(VERIFICATION_REDIRECT_URI));
        assert!(read.contains("wiki:read"));
        assert!(read.contains("wiki-mcp-codex"));
        assert!(read.contains("add wiki:write only if"));
        let write = app_instructions("codex", true);
        assert!(write.contains("read (wiki:read) and write (wiki:write)"));
    }

    #[test]
    fn asks_client_id_again_on_bad_format() {
        let mut ui = ScriptedUi {
            answers: ["nope".to_owned(), CLIENT_ID.to_uppercase()].into(),
            ..Default::default()
        };
        let client_id = ask_client_id(&mut ui, "codex", false, BrowserDecision::Open).unwrap();
        assert_eq!(client_id, CLIENT_ID);
        assert_eq!(ui.opened, [CLIENT_NEW_URL]);
        assert!(ui.text().contains("try again"), "{}", ui.text());

        let mut ui = ScriptedUi {
            answers: ["a".to_owned(), "b".to_owned(), "c".to_owned()].into(),
            ..Default::default()
        };
        let err = ask_client_id(
            &mut ui,
            "codex",
            false,
            BrowserDecision::Skip(SkipReason::Flag),
        )
        .unwrap_err();
        assert!(err.to_string().contains("invalid ClientID"), "{err}");
        assert!(ui.opened.is_empty());
    }
}
