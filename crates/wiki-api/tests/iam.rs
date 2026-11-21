//! IAM-токены через подменный `yc` (скрипт во временном каталоге) и исполнитель на
//! `wiremock`: обновление по сроку, повтор после 401, работа дольше 12 часов на управляемых
//! часах, токен не попадает в ошибки. Настоящий `yc` и сеть не используются. Обмен ключа
//! сервисного аккаунта на токен — в `tests/service_account.rs`.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use wiki_api::HttpMethod;
use wiki_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, OrgId, RequestBody, RetryPolicy,
};
use wiki_api::iam::{IamError, IamTokens, ManualClock, YcCommand};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORG: &str = "bpf-test-org";
const ME: &str = "/v1/users/me";

/// Подменный `yc`: каждый вызов увеличивает счётчик в `count` и печатает токен
/// `iam-token-<номер>`; аргументы пишет в `args`. Файл `expires` — срок для JSON-ответа
/// (без него печатается только токен, как у старых версий `yc`), файл `same` — печатать
/// всегда `iam-token-1`, файл `fail` — завершиться ошибкой с «секретом» в stderr, файл
/// `sleep` — зависнуть.
struct FakeYc {
    dir: tempfile::TempDir,
}

const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
n=$(cat "$dir/count" 2>/dev/null || echo 0)
n=$((n + 1))
echo "$n" > "$dir/count"
echo "$@" > "$dir/args"
if [ -f "$dir/sleep" ]; then exec sleep 30; fi
if [ -f "$dir/fail" ]; then
  echo "ERROR: token t1.9euelZqLeakedSecretValue0123 is broken" >&2
  echo "see https://auth.example.test/?code=0123456789abcdef" >&2
  exit 3
fi
if [ -f "$dir/same" ]; then n=1; fi
if [ -f "$dir/expires" ]; then
  printf '{"iam_token": "iam-token-%s", "expires_at": "%s"}\n' "$n" "$(cat "$dir/expires")"
else
  echo "iam-token-$n"
fi
"#;

/// Пишет исполняемый скрипт через отдельный процесс `sh`: если тест держит
/// открытый на запись дескриптор, параллельный тест может унаследовать его
/// при fork, и запуск скрипта падает с «Text file busy» (ETXTBSY).
fn write_script(path: &std::path::Path, content: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(content.as_bytes()).unwrap();
    drop(stdin);
    assert!(child.wait().unwrap().success());
}

impl FakeYc {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("yc");
        write_script(&script, SCRIPT);
        Self { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("yc")
    }

    fn command(&self) -> YcCommand {
        YcCommand::new(Some(self.path()), Some("work".into())).unwrap()
    }

    fn calls(&self) -> u32 {
        std::fs::read_to_string(self.dir.path().join("count"))
            .map(|text| text.trim().parse().unwrap())
            .unwrap_or(0)
    }

    fn args(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("args")).unwrap()
    }

    fn flag(&self, name: &str, value: Option<&str>) {
        let file = self.dir.path().join(name);
        match value {
            Some(value) => std::fs::write(file, value).unwrap(),
            None => {
                let _ = std::fs::remove_file(file);
            }
        }
    }
}

/// Время RFC 3339 для файла `expires`.
fn rfc3339(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Обратный алгоритм Хиннанта: дни от 1970-01-01 в дату.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.123456Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Начало отсчёта управляемых часов: 2026-01-01T00:00:00Z.
fn start() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_767_225_600)
}

fn client(server: &MockServer, tokens: Arc<IamTokens>) -> ApiClient {
    let mut config = ClientConfig::new(None, Some(OrgId::Cloud(ORG.into()))).unwrap();
    config.base_url = server.uri().parse().unwrap();
    config.iam = Some(tokens);
    config.timeout = Duration::from_secs(5);
    config.retry = RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    };
    ApiClient::new(config).unwrap()
}

/// `GET /v1/users/me` отвечает 200 только на `Bearer <token>`.
async fn accept(server: &MockServer, token: &str) {
    Mock::given(method("GET"))
        .and(path(ME))
        .and(header("authorization", format!("Bearer {token}").as_str()))
        .and(header("x-cloud-org-id", ORG))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"username": token})))
        .with_priority(1)
        .mount(server)
        .await;
}

/// Всё остальное — 401 с эхом заголовка в теле (как будто сервер отразил токен).
async fn reject_others(server: &MockServer) {
    Mock::given(path(ME))
        .respond_with(|req: &wiremock::Request| {
            let auth = req
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            ResponseTemplate::new(401).set_body_json(json!({
                "error_code": "UNAUTHORIZED",
                "debug_message": format!("token expired: {auth}")
            }))
        })
        .with_priority(10)
        .mount(server)
        .await;
}

async fn login_of(client: &ApiClient) -> Result<String, ApiError> {
    let response = client.execute(ApiRequest::new(HttpMethod::Get, ME)).await?;
    match response.body {
        wiki_api::executor::ResponseBody::Json(value) => {
            Ok(value["username"].as_str().unwrap().to_owned())
        }
        other => panic!("ожидался JSON, получено {other:?}"),
    }
}

#[tokio::test]
async fn token_is_cached_and_refreshed_hourly() {
    let yc = FakeYc::new();
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = Arc::new(IamTokens::with_clock(yc.command(), clock.clone()));
    let server = MockServer::start().await;
    for n in 1..=2 {
        accept(&server, &format!("iam-token-{n}")).await;
    }
    let client = client(&server, tokens.clone());

    assert_eq!(login_of(&client).await.unwrap(), "iam-token-1");
    assert_eq!(
        yc.args().trim(),
        "iam create-token --profile work --format json"
    );
    clock.advance(Duration::from_secs(59 * 60));
    assert_eq!(login_of(&client).await.unwrap(), "iam-token-1");
    assert_eq!(yc.calls(), 1, "токен в пределах часа берётся из памяти");

    clock.advance(Duration::from_secs(2 * 60));
    assert_eq!(login_of(&client).await.unwrap(), "iam-token-2");
    assert_eq!(yc.calls(), 2);
}

#[tokio::test]
async fn refresh_follows_expires_at_from_yc() {
    let yc = FakeYc::new();
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = IamTokens::with_clock(yc.command(), clock.clone());

    // Токен из кеша `yc`, которому осталось 25 минут: замена за 10 минут до истечения.
    yc.flag(
        "expires",
        Some(&rfc3339(start() + Duration::from_secs(25 * 60))),
    );
    assert_eq!(tokens.token().await.unwrap().expose(), "iam-token-1");
    clock.advance(Duration::from_secs(14 * 60));
    assert_eq!(tokens.token().await.unwrap().expose(), "iam-token-1");
    assert_eq!(yc.calls(), 1);
    clock.advance(Duration::from_secs(2 * 60));
    yc.flag(
        "expires",
        Some(&rfc3339(clock_now(&clock) + Duration::from_secs(12 * 3600))),
    );
    assert_eq!(tokens.token().await.unwrap().expose(), "iam-token-2");
    assert_eq!(yc.calls(), 2);

    // Срок 12 часов — всё равно не реже раза в час.
    clock.advance(Duration::from_secs(61 * 60));
    assert_eq!(tokens.token().await.unwrap().expose(), "iam-token-3");
    assert_eq!(yc.calls(), 3);
}

fn clock_now(clock: &ManualClock) -> SystemTime {
    use wiki_api::iam::Clock;
    clock.now()
}

/// Сервер работает дольше 12 часов: каждый токен Wiki принимает 12 часов с выдачи, часы
/// управляемые, ожидания нет.
#[tokio::test]
async fn works_longer_than_twelve_hours() {
    let yc = FakeYc::new();
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = Arc::new(IamTokens::with_clock(yc.command(), clock.clone()));
    let server = MockServer::start().await;
    reject_others(&server).await;
    let client = client(&server, tokens);

    // Когда выдан каждый токен: `issued_at[n - 1]` — для `iam-token-<n>`.
    let mut issued_at: Vec<SystemTime> = Vec::new();
    // 30 часов, запрос каждые 20 минут.
    for step in 0..=90u64 {
        let now = start() + Duration::from_secs(step * 20 * 60);
        clock.set(now);
        yc.flag(
            "expires",
            Some(&rfc3339(now + Duration::from_secs(12 * 3600))),
        );
        // Wiki принимает только токены младше 12 часов и тот, что `yc` выдаст сейчас.
        server.reset().await;
        reject_others(&server).await;
        for (index, at) in issued_at.iter().enumerate() {
            if now.duration_since(*at).unwrap() < Duration::from_secs(12 * 3600) {
                accept(&server, &format!("iam-token-{}", index + 1)).await;
            }
        }
        accept(&server, &format!("iam-token-{}", issued_at.len() + 1)).await;

        let login = login_of(&client).await.unwrap();
        while issued_at.len() < yc.calls() as usize {
            issued_at.push(now);
        }
        let expected = (step / 3) + 1;
        assert_eq!(login, format!("iam-token-{expected}"), "шаг {step}");
    }
    // Обновление раз в час: 31 токен за 30 часов, без повторов после 401.
    assert_eq!(yc.calls(), 31);
}

#[tokio::test]
async fn unauthorized_is_retried_once_with_a_new_token() {
    let yc = FakeYc::new();
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = Arc::new(IamTokens::with_clock(yc.command(), clock.clone()));
    let server = MockServer::start().await;
    // Первый токен отклоняется (например, отозван раньше срока), второй принимается.
    accept(&server, "iam-token-2").await;
    reject_others(&server).await;
    let client = client(&server, tokens.clone());

    assert_eq!(login_of(&client).await.unwrap(), "iam-token-2");
    assert_eq!(yc.calls(), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);

    // Новый токен не принимается: повтор только один, ошибка без значения токена.
    server.reset().await;
    reject_others(&server).await;
    let err = login_of(&client).await.unwrap_err();
    assert_eq!(err.status(), Some(401));
    let text = err.to_string();
    assert!(!text.contains("iam-token"), "{text}");
    assert!(text.contains("Bearer ***"), "{text}");
    assert_eq!(yc.calls(), 3);
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn same_token_from_yc_is_not_retried() {
    let yc = FakeYc::new();
    yc.flag("same", Some(""));
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = Arc::new(IamTokens::with_clock(yc.command(), clock));
    let server = MockServer::start().await;
    reject_others(&server).await;
    let client = client(&server, tokens);

    let err = login_of(&client).await.unwrap_err();
    assert_eq!(err.status(), Some(401));
    // yc спрошен дважды (после 401), но выдал тот же токен — запрос не повторялся.
    assert_eq!(yc.calls(), 2);
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn post_is_retried_after_401_but_not_after_a_redirect() {
    let yc = FakeYc::new();
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = Arc::new(IamTokens::with_clock(yc.command(), clock));
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .and(header("authorization", "Bearer iam-token-2"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({"id": 1})))
        .with_priority(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/pages"))
        .respond_with(ResponseTemplate::new(401))
        .with_priority(10)
        .mount(&server)
        .await;
    // Запрос, который выполнился и ответил редиректом, а 401 пришёл на адрес редиректа.
    Mock::given(method("POST"))
        .and(path("/v1/redirecting"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "/v1/target"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/target"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let client = client(&server, tokens);

    let mut request = ApiRequest::new(HttpMethod::Post, "/v1/pages");
    request.body = RequestBody::Json(json!({"slug": "test/page", "title": "x"}));
    let response = client.execute(request).await.unwrap();
    assert_eq!(response.status, 201);
    assert_eq!(yc.calls(), 2);

    let mut request = ApiRequest::new(HttpMethod::Post, "/v1/redirecting");
    request.body = RequestBody::Json(json!({}));
    let err = client.execute(request).await.unwrap_err();
    assert_eq!(err.status(), Some(401));
    assert_eq!(
        yc.calls(),
        2,
        "после редиректа POST не повторяется и yc не спрашивается"
    );
}

#[tokio::test]
async fn yc_failures_do_not_leak_secrets() {
    let yc = FakeYc::new();
    yc.flag("fail", Some(""));
    let tokens = Arc::new(IamTokens::new(yc.command()));
    let server = MockServer::start().await;
    let client = client(&server, tokens.clone());

    let err = login_of(&client).await.unwrap_err();
    assert!(matches!(err, ApiError::TokenUnavailable(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("exit code 3"), "{text}");
    assert!(text.contains("iam create-token --profile work"), "{text}");
    assert!(!text.contains("LeakedSecret"), "{text}");
    assert!(!text.contains("0123456789abcdef"), "{text}");
    assert!(text.contains("ERROR: token *** is broken"), "{text}");
    // Запрос в API не отправлялся.
    assert!(server.received_requests().await.unwrap().is_empty());

    let err = tokens.token().await.unwrap_err();
    assert!(matches!(err, IamError::Failed { .. }), "{err:?}");
}

#[tokio::test]
async fn yc_timeout_stops_the_process() {
    let yc = FakeYc::new();
    yc.flag("sleep", Some(""));
    let command = yc.command().with_timeout(Duration::from_millis(300));
    let started = std::time::Instant::now();
    let err = command.run().await.unwrap_err();
    assert!(matches!(err, IamError::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn bad_output_is_reported_without_it() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("yc");
    write_script(
        &script,
        "#!/bin/sh\necho 'secret-looking output with spaces'\n",
    );
    let command = YcCommand::new(Some(script), None).unwrap();
    let err = command.run().await.unwrap_err();
    assert!(matches!(err, IamError::BadOutput { .. }), "{err:?}");
    assert!(!err.to_string().contains("secret-looking"), "{err}");
}

#[tokio::test]
async fn concurrent_requests_share_one_yc_call() {
    let yc = FakeYc::new();
    let tokens = Arc::new(IamTokens::new(yc.command()));
    let server = MockServer::start().await;
    accept(&server, "iam-token-1").await;
    let client = client(&server, tokens);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let client = client.clone();
        tasks.push(tokio::spawn(async move { login_of(&client).await }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), "iam-token-1");
    }
    assert_eq!(yc.calls(), 1);
}

#[test]
fn fake_yc_path_is_absolute() {
    let yc = FakeYc::new();
    assert!(Path::new(&yc.path()).is_absolute());
}
