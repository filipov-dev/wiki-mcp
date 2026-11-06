//! Организация Wiki.
//!
//! Каждый запрос к Wiki API несёт ID организации в заголовке, который зависит от типа
//! организации: Yandex 360 для бизнеса — `X-Org-Id`, Yandex Cloud Organization —
//! `X-Cloud-Org-Id`. Тип и ID хранятся в профиле вместе (токен — отдельно, в системном
//! хранилище).

use std::fmt;

use serde::{Deserialize, Serialize};

/// Максимальная длина ID организации.
const MAX_ORG_ID_LEN: usize = 64;

/// Тип организации: определяет заголовок с её ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OrgKind {
    /// Yandex 360 для бизнеса: заголовок `X-Org-Id`.
    #[serde(rename = "yandex360")]
    Yandex360,
    /// Yandex Cloud Organization: заголовок `X-Cloud-Org-Id`.
    #[serde(rename = "cloud")]
    Cloud,
}

impl OrgKind {
    /// Заголовок запроса, в котором передаётся ID организации этого типа.
    pub const fn header_name(self) -> &'static str {
        match self {
            Self::Yandex360 => "X-Org-Id",
            Self::Cloud => "X-Cloud-Org-Id",
        }
    }

    /// Название типа для пользователя.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Yandex360 => "Yandex 360",
            Self::Cloud => "Yandex Cloud",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OrgError {
    #[error(
        "invalid organization ID: use 1-{MAX_ORG_ID_LEN} characters from A-Z, a-z, 0-9, `-`, `_`"
    )]
    InvalidId,
}

/// Организация: тип и ID. ID проверяется при создании и при чтении из файла, поэтому его
/// можно без дополнительных проверок подставлять в заголовок.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawOrganization", into = "RawOrganization")]
pub struct Organization {
    kind: OrgKind,
    id: String,
}

impl Organization {
    /// Организация с проверенным ID; пробелы по краям отбрасываются.
    pub fn new(kind: OrgKind, id: impl AsRef<str>) -> Result<Self, OrgError> {
        let id = id.as_ref().trim();
        let valid_chars = id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        if id.is_empty() || id.len() > MAX_ORG_ID_LEN || !valid_chars {
            return Err(OrgError::InvalidId);
        }
        Ok(Self {
            kind,
            id: id.to_owned(),
        })
    }

    pub fn kind(&self) -> OrgKind {
        self.kind
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Заголовок организации: имя и значение.
    pub fn header(&self) -> (&'static str, &str) {
        (self.kind.header_name(), &self.id)
    }
}

impl fmt::Display for Organization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} `{}` ({})",
            self.kind.label(),
            self.id,
            self.kind.header_name()
        )
    }
}

/// Представление в файле профилей: `type` и `id`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOrganization {
    #[serde(rename = "type")]
    kind: OrgKind,
    id: String,
}

impl TryFrom<RawOrganization> for Organization {
    type Error = OrgError;

    fn try_from(raw: RawOrganization) -> Result<Self, Self::Error> {
        Self::new(raw.kind, raw.id)
    }
}

impl From<Organization> for RawOrganization {
    fn from(org: Organization) -> Self {
        Self {
            kind: org.kind,
            id: org.id,
        }
    }
}

/// Организация профиля в виде, который принимает исполнитель запросов.
impl From<&Organization> for crate::executor::OrgId {
    fn from(org: &Organization) -> Self {
        match org.kind() {
            OrgKind::Yandex360 => Self::Yandex360(org.id().to_owned()),
            OrgKind::Cloud => Self::Cloud(org.id().to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers_by_kind() {
        let org = Organization::new(OrgKind::Yandex360, " 12345 ").unwrap();
        assert_eq!(org.header(), ("X-Org-Id", "12345"));
        let org = Organization::new(OrgKind::Cloud, "bpf-test_1").unwrap();
        assert_eq!(org.header(), ("X-Cloud-Org-Id", "bpf-test_1"));
        assert_eq!(
            org.to_string(),
            "Yandex Cloud `bpf-test_1` (X-Cloud-Org-Id)"
        );
    }

    #[test]
    fn rejects_bad_ids() {
        for bad in ["", "  ", "a b", "a/b", "a\nb", "ид", &"1".repeat(65)] {
            assert_eq!(
                Organization::new(OrgKind::Yandex360, bad),
                Err(OrgError::InvalidId),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn serde_round_trip_validates() {
        let org = Organization::new(OrgKind::Cloud, "abc").unwrap();
        let json = serde_json::to_value(&org).unwrap();
        assert_eq!(json, serde_json::json!({"type": "cloud", "id": "abc"}));
        assert_eq!(serde_json::from_value::<Organization>(json).unwrap(), org);

        let bad = serde_json::json!({"type": "yandex360", "id": "a\r\nX-Evil: 1"});
        assert!(serde_json::from_value::<Organization>(bad).is_err());
        let unknown = serde_json::json!({"type": "other", "id": "1"});
        assert!(serde_json::from_value::<Organization>(unknown).is_err());
    }

    #[test]
    fn converts_to_executor_org() {
        use crate::executor::OrgId;

        let org = Organization::new(OrgKind::Yandex360, "123").unwrap();
        let id = OrgId::from(&org);
        assert_eq!(id, OrgId::Yandex360("123".into()));
        assert_eq!(id.header_name(), org.kind().header_name());
        let org = Organization::new(OrgKind::Cloud, "bpf").unwrap();
        let id = OrgId::from(&org);
        assert_eq!(id, OrgId::Cloud("bpf".into()));
        assert_eq!(id.header_name(), org.kind().header_name());
    }
}
