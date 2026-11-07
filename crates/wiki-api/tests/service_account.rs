//! Обмен ключа сервисного аккаунта на IAM-токен: IAM API и Wiki API — на `wiremock`, часы
//! управляемые. Проверяется JWT в запросе обмена, срок из ответа, работа дольше 12 часов и
//! что ни ключ, ни JWT, ни токен не попадают в тексты ошибок. Ключ создаётся в тесте:
//! закрытых ключей в репозитории нет.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lc_rs::encoding::AsDer;
use aws_lc_rs::rsa::KeySize;
use aws_lc_rs::signature::{self, KeyPair, RsaKeyPair, UnparsedPublicKey};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use wiki_api::HttpMethod;
use wiki_api::executor::{
    ApiClient, ApiError, ApiRequest, ClientConfig, OrgId, ResponseBody, RetryPolicy,
};
use wiki_api::iam::{Clock, IamError, IamTokens, ManualClock};
use wiki_api::service_account::{IAM_TOKENS_URL, KeyExchange, ServiceAccountKey};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const ORG: &str = "bpf-test-org";
const ME: &str = "/v1/users/me";
const TOKENS: &str = "/iam/v1/tokens";
const KEY_ID: &str = "ajekeytest01";
const SA_ID: &str = "ajesatest01";

/// Начало отсчёта управляемых часов: 2026-01-01T00:00:00Z.
fn start() -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_767_225_600)
}

/// Новый ключ и его открытая часть (для проверки подписи JWT).
fn new_key() -> (ServiceAccountKey, Vec<u8>) {
    let pair = RsaKeyPair::generate(KeySize::Rsa2048).unwrap();
    let pkcs8 = pair.as_der().unwrap().as_ref().to_vec();
    let public = pair.public_key().as_ref().to_vec();
    let pem = format!(
        "PLEASE DO NOT REMOVE THIS LINE! Yandex.Cloud SA Key ID <{KEY_ID}>\n\
         -----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
        STANDARD.encode(&pkcs8)
    );
    let file = json!({
        "id": KEY_ID,
        "service_account_id": SA_ID,
        "key_algorithm": "RSA_2048",
        "private_key": pem,
    });
    let key = ServiceAccountKey::from_json(file.to_string().as_bytes()).unwrap();
    (key, public)
}

/// Время RFC 3339 с секундами (без долей).
fn rfc3339(time: SystemTime) -> String {
    let secs = time.duration_since(UNIX_EPOCH).unwrap().as_secs();
    let (days, rest) = (secs / 86_400, secs % 86_400);
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
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn decode(part: &str) -> Value {
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
}

/// Подменный IAM API: на каждый обмен выдаёт `sa-token-<номер>` со сроком 12 часов от
/// `iat` JWT и запоминает число обменов. JWT проверяется: подпись ключом, `kid`, `iss`, `aud`.
struct FakeIam {
    server: MockServer,
    calls: Arc<AtomicU32>,
}

impl FakeIam {
    async fn start(public_key: Vec<u8>) -> Self {
        let server = MockServer::start().await;
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        Mock::given(method("POST"))
            .and(path(TOKENS))
            .respond_with(move |req: &Request| {
                let body: Value = serde_json::from_slice(&req.body).unwrap();
                let jwt = body["jwt"].as_str().unwrap();
                let parts: Vec<&str> = jwt.split('.').collect();
                assert_eq!(parts.len(), 3);
                let header = decode(parts[0]);
                let claims = decode(parts[1]);
                assert_eq!(header["alg"], "PS256");
                assert_eq!(header["kid"], KEY_ID);
                assert_eq!(claims["iss"], SA_ID);
                assert_eq!(claims["aud"], IAM_TOKENS_URL);
                let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
                let signed = format!("{}.{}", parts[0], parts[1]);
                UnparsedPublicKey::new(&signature::RSA_PSS_2048_8192_SHA256, &public_key)
                    .verify(signed.as_bytes(), &signature)
                    .unwrap();
                let iat = claims["iat"].as_u64().unwrap();
                let expires = UNIX_EPOCH + Duration::from_secs(iat + 30 + 12 * 3600);
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                ResponseTemplate::new(200).set_body_json(json!({
                    "iamToken": format!("sa-token-{n}"),
                    "expiresAt": rfc3339(expires),
                }))
            })
            .mount(&server)
            .await;
        Self { server, calls }
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    fn exchange(&self, key: ServiceAccountKey) -> KeyExchange {
        KeyExchange::new(key)
            .unwrap()
            .with_endpoint(format!("{}{TOKENS}", self.server.uri()).parse().unwrap())
            .unwrap()
            .with_timeout(Duration::from_secs(5))
    }
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

async fn reject_others(server: &MockServer) {
    Mock::given(path(ME))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error_code": "UNAUTHORIZED",
            "debug_message": "token expired"
        })))
        .with_priority(10)
        .mount(server)
        .await;
}

async fn login_of(client: &ApiClient) -> Result<String, ApiError> {
    let response = client.execute(ApiRequest::new(HttpMethod::Get, ME)).await?;
    match response.body {
        ResponseBody::Json(value) => Ok(value["username"].as_str().unwrap().to_owned()),
        other => panic!("ожидался JSON, получено {other:?}"),
    }
}

#[tokio::test]
async fn key_is_exchanged_for_a_token_with_expiry() {
    let (key, public) = new_key();
    let iam = FakeIam::start(public).await;
    let issued = iam.exchange(key).run(start()).await.unwrap();
    assert_eq!(issued.token.expose(), "sa-token-1");
    assert_eq!(
        issued.expires_at,
        Some(start() + Duration::from_secs(12 * 3600))
    );
    assert_eq!(iam.calls(), 1);
}

/// Сервер работает от сервисного аккаунта 30 часов без ручного обновления: Wiki принимает
/// каждый токен 12 часов с выдачи, токен обновляется раз в час.
#[tokio::test]
async fn service_account_works_longer_than_twelve_hours() {
    let (key, public) = new_key();
    let iam = FakeIam::start(public).await;
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = Arc::new(IamTokens::with_clock(iam.exchange(key), clock.clone()));
    let wiki = MockServer::start().await;
    let client = client(&wiki, tokens);

    let mut issued_at: Vec<SystemTime> = Vec::new();
    for step in 0..=90u64 {
        let now = start() + Duration::from_secs(step * 20 * 60);
        clock.set(now);
        wiki.reset().await;
        reject_others(&wiki).await;
        for (index, at) in issued_at.iter().enumerate() {
            if now.duration_since(*at).unwrap() < Duration::from_secs(12 * 3600) {
                accept(&wiki, &format!("sa-token-{}", index + 1)).await;
            }
        }
        accept(&wiki, &format!("sa-token-{}", issued_at.len() + 1)).await;

        let login = login_of(&client).await.unwrap();
        while issued_at.len() < iam.calls() as usize {
            issued_at.push(now);
        }
        assert_eq!(login, format!("sa-token-{}", step / 3 + 1), "шаг {step}");
    }
    assert_eq!(iam.calls(), 31);
}

#[tokio::test]
async fn unauthorized_gets_a_new_token_from_the_key() {
    let (key, public) = new_key();
    let iam = FakeIam::start(public).await;
    let clock = Arc::new(ManualClock::new(start()));
    let tokens = Arc::new(IamTokens::with_clock(iam.exchange(key), clock.clone()));
    let wiki = MockServer::start().await;
    // Первый токен отозван раньше срока: Wiki принимает только второй.
    accept(&wiki, "sa-token-2").await;
    reject_others(&wiki).await;
    let client = client(&wiki, tokens);

    assert_eq!(login_of(&client).await.unwrap(), "sa-token-2");
    assert_eq!(iam.calls(), 2);
    assert_eq!(clock.now(), start());
}

#[tokio::test]
async fn rejected_key_is_reported_without_secrets() {
    let (key, _) = new_key();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKENS))
        .respond_with(|req: &Request| {
            // Сервер отражает JWT в сообщении: в ошибку он попасть не должен.
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            ResponseTemplate::new(400).set_body_json(json!({
                "code": 3,
                "message": format!("Invalid JWT {}", body["jwt"].as_str().unwrap()),
            }))
        })
        .mount(&server)
        .await;
    let exchange = KeyExchange::new(key)
        .unwrap()
        .with_endpoint(format!("{}{TOKENS}", server.uri()).parse().unwrap())
        .unwrap();
    let tokens = Arc::new(IamTokens::new(exchange));
    let wiki = MockServer::start().await;
    let client = client(&wiki, tokens.clone());

    let err = login_of(&client).await.unwrap_err();
    assert!(matches!(err, ApiError::TokenUnavailable(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("HTTP 400"), "{text}");
    assert!(text.contains("Invalid JWT ***"), "{text}");
    assert!(text.contains(KEY_ID) && text.contains(SA_ID), "{text}");
    assert!(!text.contains("eyJ"), "{text}");
    // Запрос в Wiki не отправлялся.
    assert!(wiki.received_requests().await.unwrap().is_empty());

    let err = tokens.token().await.unwrap_err();
    assert!(matches!(
        err,
        IamError::ExchangeRejected { status: 400, .. }
    ));
}

#[tokio::test]
async fn bad_exchange_responses_are_errors() {
    let (key, _) = new_key();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKENS))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"iamToken": "with space secret"})),
        )
        .mount(&server)
        .await;
    let exchange = KeyExchange::new(key.clone())
        .unwrap()
        .with_endpoint(format!("{}{TOKENS}", server.uri()).parse().unwrap())
        .unwrap();
    let err = exchange.run(start()).await.unwrap_err();
    assert!(matches!(err, IamError::ExchangeFailed { .. }), "{err:?}");
    assert!(!err.to_string().contains("secret"), "{err}");

    // Недоступный IAM API — ошибка соединения без адреса.
    // Порт свободного сокета после его закрытия: на нём никто не слушает.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let uri = format!("http://127.0.0.1:{port}{TOKENS}");
    let exchange = KeyExchange::new(key)
        .unwrap()
        .with_endpoint(uri.parse().unwrap())
        .unwrap()
        .with_timeout(Duration::from_secs(2));
    let err = exchange.run(start()).await.unwrap_err();
    assert!(matches!(err, IamError::ExchangeFailed { .. }), "{err:?}");
}
