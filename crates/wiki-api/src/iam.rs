//! IAM-токены Yandex Cloud для федеративных и сервисных аккаунтов.
//!
//! Федеративные и сервисные аккаунты организации Yandex Cloud работают с Wiki через
//! IAM-токен (`Authorization: Bearer`). IAM-токен живёт не больше 12 часов, поэтому он не
//! хранится в системном хранилище, а получается из источника ([`IamSource`]) и держится
//! только в памяти процесса ([`IamTokens`]). Источники:
//! - команда `yc iam create-token` ([`YcCommand`]) — федеративный аккаунт или любой
//!   настроенный профиль `yc`;
//! - авторизованный ключ сервисного аккаунта ([`KeyExchange`]): JWT, подписанный ключом,
//!   обменивается на IAM-токен в IAM API (модуль [`crate::service_account`]).
//!
//! Правила:
//! - `yc` запускается без shell, с аргументами списком, закрытым stdin и таймаутом
//!   ([`YcCommand`]); процесс убивается, если таймаут истёк;
//! - токен не пишется в файлы и хранилище, не попадает в логи и тексты ошибок; stderr `yc`
//!   и ответы IAM API с ошибкой могут содержать чувствительные данные, поэтому в ошибку идёт
//!   только короткая выжимка с замаскированными длинными значениями ([`summarize_stderr`]);
//! - срок: `yc iam create-token --format json` и IAM API отдают `expires_at`; токен
//!   обновляется не реже [`MAX_TOKEN_AGE`] и не позже чем за [`EXPIRY_MARGIN`] до истечения.
//!   Если срок неизвестен (старые версии `yc` печатают только токен), действует только
//!   [`MAX_TOKEN_AGE`];
//! - время берётся из [`Clock`]: тесты подставляют [`ManualClock`] и проверяют работу
//!   дольше 12 часов без ожидания.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

pub use crate::service_account::KeyExchange;
use crate::token::ApiToken;

/// Программа `yc` по умолчанию: ищется в `PATH`.
pub const DEFAULT_YC_PROGRAM: &str = "yc";

/// Максимальный возраст токена: Yandex Cloud рекомендует запрашивать IAM-токен заново
/// примерно раз в час, хотя живёт он до 12 часов.
pub const MAX_TOKEN_AGE: Duration = Duration::from_secs(60 * 60);

/// Запас до истечения срока: токен, которому осталось меньше, заменяется новым.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(10 * 60);

/// Минимальное время повторного использования токена. Если `yc` выдал токен, который вот-вот
/// истечёт (взят из его кеша), он используется хотя бы это время, чтобы не запускать `yc`
/// на каждый запрос; если Wiki его уже не примет, запрос повторится после 401.
pub const MIN_REUSE: Duration = Duration::from_secs(30);

/// Таймаут `yc` по умолчанию. Для федеративного аккаунта `yc` может открыть браузер для
/// входа через SSO, поэтому интерактивные команды задают таймаут больше.
pub const DEFAULT_YC_TIMEOUT: Duration = Duration::from_secs(60);

/// Предел stdout и stderr `yc`, байт: ответ с токеном — сотни байт.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Предел длины токена.
const MAX_TOKEN_LEN: usize = 8 * 1024;

/// Предел длины имени профиля `yc`.
const MAX_YC_PROFILE_LEN: usize = 64;

/// Предел выжимки stderr `yc` в тексте ошибки, символов.
const MAX_STDERR_SUMMARY: usize = 300;

/// Сколько прежних токенов помнить для вырезания из текстов ошибок.
const REMEMBERED_TOKENS: usize = 3;

/// Ошибки получения IAM-токена. Тексты не содержат токен и stderr `yc` целиком.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IamError {
    #[error(
        "the Yandex Cloud CLI `{program}` was not found; install it \
         (https://yandex.cloud/docs/cli/quickstart), or set the path to yc with --yc-path"
    )]
    NotFound { program: String },
    #[error("could not run `{program}`: {details}")]
    Spawn { program: String, details: String },
    #[error("`{command}` did not finish in {seconds} s and was stopped")]
    Timeout { command: String, seconds: u64 },
    #[error(
        "`{command}` failed ({status}){details}. Check the yc setup: run `{command}` yourself \
         (for a federated account it may ask you to sign in in the browser)"
    )]
    Failed {
        command: String,
        status: String,
        details: String,
    },
    #[error("unexpected output of `{command}`: {reason}")]
    BadOutput { command: String, reason: String },
    #[error("invalid yc settings: {0}")]
    InvalidConfig(String),
    /// Ключ сервисного аккаунта не разобран; текст не содержит ключа.
    #[error("invalid service account key: {0}")]
    InvalidKey(String),
    /// IAM API не обменял JWT на токен.
    #[error(
        "Yandex Cloud IAM did not issue a token for {key} (HTTP {status}){details}. Check \
         that the key and the service account still exist"
    )]
    ExchangeRejected {
        key: String,
        status: u16,
        details: String,
    },
    /// IAM API недоступен (сеть, TLS, таймаут) или ответил не по форме.
    #[error("could not get an IAM token for {key} from Yandex Cloud IAM: {details}")]
    ExchangeFailed { key: String, details: String },
}

/// Источник текущего времени.
pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> SystemTime;
}

/// Системные часы.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Управляемые часы для тестов: время меняется только вызовами [`ManualClock::advance`]
/// и [`ManualClock::set`].
#[derive(Debug)]
pub struct ManualClock(Mutex<SystemTime>);

impl ManualClock {
    pub fn new(start: SystemTime) -> Self {
        Self(Mutex::new(start))
    }

    pub fn set(&self, time: SystemTime) {
        *self.lock() = time;
    }

    pub fn advance(&self, by: Duration) {
        let mut now = self.lock();
        *now += by;
    }

    fn lock(&self) -> MutexGuard<'_, SystemTime> {
        // Отравление невозможно: под блокировкой нет кода, который паникует.
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *self.lock()
    }
}

/// Команда `yc iam create-token [--profile <имя>] --format json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YcCommand {
    program: PathBuf,
    profile: Option<String>,
    timeout: Duration,
}

impl YcCommand {
    /// `path` — абсолютный путь к `yc` (без него `yc` ищется в `PATH`); `profile` — имя
    /// профиля `yc` (без него — активный профиль `yc`).
    pub fn new(path: Option<PathBuf>, profile: Option<String>) -> Result<Self, IamError> {
        let program = match path {
            Some(path) => {
                // Относительный путь зависел бы от рабочего каталога сервера, который задаёт
                // MCP-клиент, поэтому принимается только абсолютный.
                if !path.is_absolute() {
                    return Err(IamError::InvalidConfig(format!(
                        "the path to yc must be absolute: `{}`",
                        path.display()
                    )));
                }
                path
            }
            None => PathBuf::from(DEFAULT_YC_PROGRAM),
        };
        if let Some(profile) = &profile {
            validate_yc_profile(profile)?;
        }
        Ok(Self {
            program,
            profile,
            timeout: DEFAULT_YC_TIMEOUT,
        })
    }

    /// Та же команда с другим таймаутом.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn profile(&self) -> Option<&str> {
        self.profile.as_deref()
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Аргументы `yc`.
    pub fn args(&self) -> Vec<String> {
        let mut args = vec!["iam".to_owned(), "create-token".to_owned()];
        if let Some(profile) = &self.profile {
            args.push("--profile".to_owned());
            args.push(profile.clone());
        }
        args.push("--format".to_owned());
        args.push("json".to_owned());
        args
    }

    /// Команда для пользователя, без `--format json`: так её удобно запустить вручную.
    pub fn display(&self) -> String {
        let mut text = format!("{} iam create-token", self.program.display());
        if let Some(profile) = &self.profile {
            text.push_str(" --profile ");
            text.push_str(profile);
        }
        text
    }

    /// Запускает `yc` и разбирает ответ. Без shell, stdin закрыт, по таймауту процесс
    /// убивается.
    pub async fn run(&self) -> Result<IssuedToken, IamError> {
        let program = self.program.display().to_string();
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(self.args())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => IamError::NotFound {
                program: program.clone(),
            },
            _ => IamError::Spawn {
                program: program.clone(),
                details: err.to_string(),
            },
        })?;
        tracing::debug!(program = %program, "получение IAM-токена через yc");
        // При таймауте future с процессом сбрасывается, а kill_on_drop убивает процесс.
        let output = tokio::time::timeout(self.timeout, child.wait_with_output())
            .await
            .map_err(|_| IamError::Timeout {
                command: self.display(),
                seconds: self.timeout.as_secs(),
            })?
            .map_err(|err| IamError::Spawn {
                program: program.clone(),
                details: err.to_string(),
            })?;
        if !output.status.success() {
            let status = match output.status.code() {
                Some(code) => format!("exit code {code}"),
                None => "terminated by a signal".to_owned(),
            };
            let summary = summarize_stderr(&output.stderr);
            let details = if summary.is_empty() {
                String::new()
            } else {
                format!(": {summary}")
            };
            return Err(IamError::Failed {
                command: self.display(),
                status,
                details,
            });
        }
        parse_output(&output.stdout).map_err(|reason| IamError::BadOutput {
            command: self.display(),
            reason,
        })
    }
}

/// Имя профиля `yc`: передаётся отдельным аргументом, но не должно выглядеть как флаг.
fn validate_yc_profile(profile: &str) -> Result<(), IamError> {
    let valid_chars = profile
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if profile.is_empty()
        || profile.len() > MAX_YC_PROFILE_LEN
        || !valid_chars
        || profile.starts_with('-')
    {
        return Err(IamError::InvalidConfig(format!(
            "invalid yc profile name `{profile}`: use 1-{MAX_YC_PROFILE_LEN} characters from \
             A-Z, a-z, 0-9, `.`, `_`, `-`, not starting with `-`"
        )));
    }
    Ok(())
}

/// Токен от `yc` и срок его действия, если `yc` его сообщил.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedToken {
    pub token: ApiToken,
    pub expires_at: Option<SystemTime>,
}

/// Разбирает stdout `yc iam create-token`: JSON `{"iam_token", "expires_at"}` или одна
/// строка с токеном. Текст причины не содержит вывода `yc`.
fn parse_output(stdout: &[u8]) -> Result<IssuedToken, String> {
    if stdout.len() > MAX_OUTPUT_BYTES {
        return Err("the output is too large".to_owned());
    }
    let text = std::str::from_utf8(stdout)
        .map_err(|_| "the output is not UTF-8".to_owned())?
        .trim();
    if text.is_empty() {
        return Err("the output is empty".to_owned());
    }
    if text.starts_with('{') {
        let value: Value =
            serde_json::from_str(text).map_err(|_| "the JSON output is malformed".to_owned())?;
        return token_from_json(&value);
    }
    if text.lines().count() != 1 {
        return Err("expected a single line with the token".to_owned());
    }
    issued(text, None)
}

/// Токен и срок из JSON: `iam_token`/`iamToken` и `expires_at`/`expiresAt` (ответ `yc` и
/// IAM API). Текст причины не содержит значений.
pub(crate) fn token_from_json(value: &Value) -> Result<IssuedToken, String> {
    let field = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| value.get(*name).and_then(Value::as_str))
    };
    let token = field(&["iam_token", "iamToken"])
        .ok_or_else(|| "the JSON output has no `iam_token`".to_owned())?;
    let expires_at = match field(&["expires_at", "expiresAt"]) {
        Some(text) => {
            Some(parse_rfc3339(text).ok_or_else(|| "`expires_at` is not a valid time".to_owned())?)
        }
        None => None,
    };
    issued(token.trim(), expires_at)
}

/// Проверяет форму токена: одна строка печатных символов ASCII без пробелов.
fn issued(token: &str, expires_at: Option<SystemTime>) -> Result<IssuedToken, String> {
    if token.is_empty()
        || token.len() > MAX_TOKEN_LEN
        || !token.chars().all(|c| c.is_ascii_graphic())
    {
        return Err("the token is empty or has unexpected characters".to_owned());
    }
    Ok(IssuedToken {
        token: ApiToken::new(token),
        expires_at,
    })
}

/// Короткая выжимка stderr `yc` для текста ошибки: первые строки, длинные значения со
/// служебными символами (токены, ключи, ссылки с параметрами) заменены на `***`.
pub fn summarize_stderr(stderr: &[u8]) -> String {
    let head = &stderr[..stderr.len().min(4096)];
    let text = String::from_utf8_lossy(head);
    let lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(3)
        .map(|line| {
            line.split_whitespace()
                .map(mask_word)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let joined = lines.join(" | ");
    if joined.chars().count() <= MAX_STDERR_SUMMARY {
        return joined;
    }
    let mut short: String = joined.chars().take(MAX_STDERR_SUMMARY).collect();
    short.push('…');
    short
}

/// Слово от 16 символов с цифрами или знаками `._-/=+:%~` похоже на токен, ключ или ссылку:
/// заменяется на `***`. Обычные слова сообщений об ошибках остаются.
fn mask_word(word: &str) -> &str {
    let suspicious = word.chars().count() >= 16
        && word.chars().any(|c| {
            c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '/' | '=' | '+' | ':' | '%' | '~')
        });
    if suspicious { "***" } else { word }
}

/// Время RFC 3339 (`2026-01-02T03:04:05.123Z`, `...+03:00`) в `SystemTime`. Доли секунды
/// отбрасываются: срок нужен с точностью до минут.
fn parse_rfc3339(text: &str) -> Option<SystemTime> {
    let b = text.trim().as_bytes();
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = b.get(range)?;
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    if !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &b[19..];
    if let Some(fraction) = rest.strip_prefix(b".") {
        let digits = fraction.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        rest = &fraction[digits..];
    }
    let offset = match rest {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let digits = [*h1, *h2, *m1, *m2];
            if !digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let value = |hi: u8, lo: u8| i64::from(hi - b'0') * 10 + i64::from(lo - b'0');
            let minutes = value(*h1, *h2) * 60 + value(*m1, *m2);
            if *sign == b'+' {
                minutes * 60
            } else {
                -minutes * 60
            }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(secs)
        .ok()
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
}

/// Число дней от 1970-01-01 до даты григорианского календаря (алгоритм Г. Хиннанта).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Токен в памяти и время, когда его пора заменить.
struct Cached {
    token: ApiToken,
    refresh_at: SystemTime,
}

#[derive(Default)]
struct State {
    current: Option<Cached>,
    /// Прежние токены: нужны, чтобы вырезать их из текстов ошибок (например, ответа 401).
    previous: Vec<ApiToken>,
}

/// Откуда берутся IAM-токены.
#[derive(Debug, Clone)]
pub enum IamSource {
    /// `yc iam create-token`.
    Yc(YcCommand),
    /// Обмен JWT, подписанного ключом сервисного аккаунта, в IAM API.
    ServiceAccountKey(KeyExchange),
}

impl IamSource {
    /// Источник для пользователя: команда `yc` или ключ сервисного аккаунта. Без секретов.
    pub fn display(&self) -> String {
        match self {
            Self::Yc(command) => format!("`{}`", command.display()),
            Self::ServiceAccountKey(exchange) => exchange.display(),
        }
    }

    /// Новый токен; `now` — текущее время (для JWT ключа сервисного аккаунта).
    pub async fn issue(&self, now: SystemTime) -> Result<IssuedToken, IamError> {
        match self {
            Self::Yc(command) => command.run().await,
            Self::ServiceAccountKey(exchange) => exchange.run(now).await,
        }
    }
}

impl From<YcCommand> for IamSource {
    fn from(command: YcCommand) -> Self {
        Self::Yc(command)
    }
}

impl From<KeyExchange> for IamSource {
    fn from(exchange: KeyExchange) -> Self {
        Self::ServiceAccountKey(exchange)
    }
}

/// IAM-токены профиля: получаются из [`IamSource`] и держатся только в памяти процесса.
///
/// Запросы к API берут токен через [`IamTokens::token`]: если срок подошёл, токен
/// обновляется до запроса. После ответа 401 исполнитель вызывает
/// [`IamTokens::refresh_after_unauthorized`]. Одновременно идёт не больше одного получения
/// токена: остальные запросы ждут и получают тот же новый токен.
pub struct IamTokens {
    source: IamSource,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
    refresh: tokio::sync::Mutex<()>,
}

impl fmt::Debug for IamTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Токены в Debug не выводятся, только признак, что токен есть.
        let cached = self.lock().current.is_some();
        f.debug_struct("IamTokens")
            .field("source", &self.source)
            .field("cached", &cached)
            .finish_non_exhaustive()
    }
}

impl IamTokens {
    pub fn new(source: impl Into<IamSource>) -> Self {
        Self::with_clock(source, Arc::new(SystemClock))
    }

    pub fn with_clock(source: impl Into<IamSource>, clock: Arc<dyn Clock>) -> Self {
        Self {
            source: source.into(),
            clock,
            state: Mutex::new(State::default()),
            refresh: tokio::sync::Mutex::new(()),
        }
    }

    pub fn source(&self) -> &IamSource {
        &self.source
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Под блокировкой только короткие операции без паники.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Токен, если он есть и его срок ещё не подошёл.
    fn fresh(&self) -> Option<ApiToken> {
        let now = self.clock.now();
        let state = self.lock();
        state
            .current
            .as_ref()
            .filter(|cached| now < cached.refresh_at)
            .map(|cached| cached.token.clone())
    }

    /// Текущий токен; если его нет или срок подошёл — новый из источника.
    pub async fn token(&self) -> Result<ApiToken, IamError> {
        if let Some(token) = self.fresh() {
            return Ok(token);
        }
        let _guard = self.refresh.lock().await;
        // Пока ждали, токен мог обновить другой запрос.
        if let Some(token) = self.fresh() {
            return Ok(token);
        }
        let issued = self.source.issue(self.clock.now()).await?;
        Ok(self.store(issued))
    }

    /// Новый токен после ответа 401 на запрос с токеном `rejected`.
    ///
    /// `None` — повторять запрос бессмысленно: источник выдал тот же токен (например, `yc` из
    /// своего кеша), значит, дело не в сроке токена.
    pub async fn refresh_after_unauthorized(
        &self,
        rejected: &ApiToken,
    ) -> Result<Option<ApiToken>, IamError> {
        let _guard = self.refresh.lock().await;
        // Другой запрос уже получил новый токен, пока этот ждал ответа.
        if let Some(token) = self.fresh()
            && token != *rejected
        {
            return Ok(Some(token));
        }
        let issued = self.source.issue(self.clock.now()).await?;
        if issued.token == *rejected {
            self.store(issued);
            return Ok(None);
        }
        Ok(Some(self.store(issued)))
    }

    /// Все токены, которые процесс получал недавно: для вырезания из текстов.
    pub fn known_tokens(&self) -> Vec<ApiToken> {
        let state = self.lock();
        state
            .current
            .iter()
            .map(|cached| cached.token.clone())
            .chain(state.previous.iter().cloned())
            .collect()
    }

    /// Запоминает новый токен и время его замены.
    fn store(&self, issued: IssuedToken) -> ApiToken {
        let now = self.clock.now();
        let refresh_at = refresh_time(now, issued.expires_at);
        let token = issued.token;
        let mut state = self.lock();
        if let Some(old) = state.current.take()
            && old.token != token
        {
            state.previous.insert(0, old.token);
            state.previous.truncate(REMEMBERED_TOKENS);
        }
        state.current = Some(Cached {
            token: token.clone(),
            refresh_at,
        });
        token
    }
}

/// Когда заменить токен, полученный в `now`: не позже [`MAX_TOKEN_AGE`] и за
/// [`EXPIRY_MARGIN`] до истечения срока, но не раньше чем через [`MIN_REUSE`].
fn refresh_time(now: SystemTime, expires_at: Option<SystemTime>) -> SystemTime {
    let mut at = now + MAX_TOKEN_AGE;
    if let Some(expires_at) = expires_at {
        let before_expiry = expires_at.checked_sub(EXPIRY_MARGIN).unwrap_or(UNIX_EPOCH);
        at = at.min(before_expiry);
    }
    at.max(now + MIN_REUSE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn parses_rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(at(0)));
        // 2024-02-29T12:30:45Z (високосный год) = 1709209845.
        assert_eq!(
            parse_rfc3339("2024-02-29T12:30:45Z"),
            Some(at(1_709_209_845))
        );
        assert_eq!(
            parse_rfc3339("2024-02-29T12:30:45.123456789Z"),
            Some(at(1_709_209_845))
        );
        assert_eq!(
            parse_rfc3339("2024-02-29T15:30:45+03:00"),
            Some(at(1_709_209_845))
        );
        assert_eq!(
            parse_rfc3339("2024-02-29T10:30:45-02:00"),
            Some(at(1_709_209_845))
        );
        for bad in [
            "",
            "2024-02-29",
            "2024-02-29T12:30:45",
            "2024-13-01T00:00:00Z",
            "2024-02-29T25:00:00Z",
            "2024-02-29T12:30:45.Z",
            "2024-02-29T12:30:45+0300",
            "x024-02-29T12:30:45Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "{bad}");
        }
    }

    #[test]
    fn parses_yc_output() {
        let issued =
            parse_output(br#"{"iam_token": "fake-iam-1", "expires_at": "2024-02-29T12:30:45.5Z"}"#)
                .unwrap();
        assert_eq!(issued.token.expose(), "fake-iam-1");
        assert_eq!(issued.expires_at, Some(at(1_709_209_845)));

        let issued =
            parse_output(b"{\"iamToken\": \"t1.x\", \"expiresAt\": \"1970-01-01T00:01:00Z\"}")
                .unwrap();
        assert_eq!(issued.expires_at, Some(at(60)));

        let issued = parse_output(b"t1.plain-token\n").unwrap();
        assert_eq!(issued.token.expose(), "t1.plain-token");
        assert_eq!(issued.expires_at, None);

        for (bad, reason) in [
            (&b""[..], "empty"),
            (b"two\nlines", "single line"),
            (b"with space", "unexpected characters"),
            (b"{\"token\": \"x\"}", "no `iam_token`"),
            (
                b"{\"iam_token\": \"x\", \"expires_at\": \"soon\"}",
                "valid time",
            ),
            (b"{broken", "malformed"),
            (b"\xff\xfe", "UTF-8"),
        ] {
            let err = parse_output(bad).unwrap_err();
            assert!(err.contains(reason), "{err}");
        }
        // Причина не содержит вывода yc.
        let err = parse_output(b"{\"iam_token\": \"secret value\"}").unwrap_err();
        assert!(!err.contains("secret"), "{err}");
    }

    #[test]
    fn stderr_summary_masks_secrets() {
        let stderr = b"ERROR: failed to get token t1.9euelZqSecretSecretSecret for user\n\
            see https://example.test/auth?code=abcdef0123456789 now\n\
            \n\
            line three PermissionDenied\nline four is dropped\n";
        let summary = summarize_stderr(stderr);
        assert!(!summary.contains("t1.9eue"), "{summary}");
        assert!(!summary.contains("abcdef0123"), "{summary}");
        assert!(
            summary.contains("ERROR: failed to get token *** for user"),
            "{summary}"
        );
        assert!(summary.contains("PermissionDenied"), "{summary}");
        assert!(!summary.contains("line four"), "{summary}");

        let long = "word ".repeat(200);
        let summary = summarize_stderr(long.as_bytes());
        assert!(
            summary.chars().count() <= MAX_STDERR_SUMMARY + 1,
            "{summary}"
        );
        assert!(summary.ends_with('…'));
        assert_eq!(summarize_stderr(b""), "");
    }

    #[test]
    fn refresh_time_rules() {
        let now = at(1_000_000);
        // Срок неизвестен — через час.
        assert_eq!(refresh_time(now, None), now + MAX_TOKEN_AGE);
        // Срок 12 часов — всё равно через час.
        assert_eq!(
            refresh_time(now, Some(now + Duration::from_secs(12 * 3600))),
            now + MAX_TOKEN_AGE
        );
        // Осталось 30 минут — за 10 минут до истечения.
        assert_eq!(
            refresh_time(now, Some(now + Duration::from_secs(30 * 60))),
            now + Duration::from_secs(20 * 60)
        );
        // Уже истекает — не раньше MIN_REUSE.
        assert_eq!(refresh_time(now, Some(now)), now + MIN_REUSE);
        assert_eq!(refresh_time(now, Some(at(0))), now + MIN_REUSE);
    }

    #[test]
    fn command_arguments_and_validation() {
        let command = YcCommand::new(None, None).unwrap();
        assert_eq!(command.program(), Path::new("yc"));
        assert_eq!(command.args(), ["iam", "create-token", "--format", "json"]);
        assert_eq!(command.display(), "yc iam create-token");
        assert_eq!(command.timeout(), DEFAULT_YC_TIMEOUT);

        let command = YcCommand::new(None, Some("work".into()))
            .unwrap()
            .with_timeout(Duration::from_secs(5));
        assert_eq!(
            command.args(),
            [
                "iam",
                "create-token",
                "--profile",
                "work",
                "--format",
                "json"
            ]
        );
        assert_eq!(command.display(), "yc iam create-token --profile work");
        assert_eq!(command.timeout(), Duration::from_secs(5));

        let err = YcCommand::new(Some("bin/yc".into()), None).unwrap_err();
        assert!(err.to_string().contains("absolute"), "{err}");
        for bad in ["", "-x", "--format", "a b", "a/b", &"p".repeat(65)] {
            let err = YcCommand::new(None, Some(bad.to_owned())).unwrap_err();
            assert!(err.to_string().contains("yc profile"), "{bad}: {err}");
        }
    }

    #[test]
    fn manual_clock_moves_only_on_demand() {
        let clock = ManualClock::new(at(10));
        assert_eq!(clock.now(), at(10));
        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.now(), at(15));
        clock.set(at(1));
        assert_eq!(clock.now(), at(1));
    }

    #[tokio::test]
    async fn missing_program_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let command = YcCommand::new(Some(dir.path().join("no-such-yc")), None).unwrap();
        let err = command.run().await.unwrap_err();
        assert!(matches!(err, IamError::NotFound { .. }), "{err:?}");
        assert!(err.to_string().contains("--yc-path"), "{err}");
    }

    #[test]
    fn debug_hides_tokens() {
        let tokens = IamTokens::new(YcCommand::new(None, None).unwrap());
        tokens.store(IssuedToken {
            token: ApiToken::new("t1.debug-secret"),
            expires_at: None,
        });
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("debug-secret"), "{debug}");
        assert!(debug.contains("cached: true"), "{debug}");
        assert_eq!(tokens.known_tokens().len(), 1);
    }
}
