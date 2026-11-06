//! Токен Wiki API.

use std::fmt;

/// Токен доступа к Wiki API.
///
/// `Debug` и `Display` не раскрывают значение, чтобы токен не попадал в логи и ошибки.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiToken(String);

impl ApiToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Значение токена. Использовать только для заголовка `Authorization` и записи в хранилище.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiToken(***)")
    }
}

impl fmt::Display for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_hide_value() {
        let token = ApiToken::new("super-secret");
        assert_eq!(format!("{token:?}"), "ApiToken(***)");
        assert_eq!(format!("{token}"), "***");
        assert_eq!(token.expose(), "super-secret");
    }
}
