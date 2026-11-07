//! `serve --http` настоящим бинарником с логами уровня `trace`: токен и ID организации из
//! заголовков запросов не попадают ни в stderr, ни в stdout.
//!
//! Отдельный процесс, а не подписчик `tracing` внутри тестов: при параллельных тестах кеш
//! interest у `tracing` отключает часть событий, и проверка была бы неполной. К Wiki API
//! тест не обращается: вызывается только инструмент с ошибкой в аргументах.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use serde_json::{Value, json};

const TOKEN: &str = "log-secret-token-91ab";
const ORG: &str = "log-secret-org-77";
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Узнаваемые идентификатор и секрет адреса загрузки.
const UPLOAD_ID: &str = "0123456789abcdef0123456789abcdef";
const UPLOAD_SECRET: &str = "5ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7e75ec7";

/// Останавливает сервер, даже если тест упал.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[tokio::test]
async fn token_and_org_never_appear_in_logs() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wiki-mcp"))
        .args([
            "serve",
            "--http",
            "--listen",
            "127.0.0.1:0",
            "--log-level",
            "trace",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let child = Child(child);

    // stderr читается целиком в фоне; адрес эндпоинта — из строки запуска.
    let logs = Arc::new(Mutex::new(String::new()));
    let (endpoint_tx, endpoint_rx) = mpsc::channel();
    let reader_logs = Arc::clone(&logs);
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if let Some(url) = line.strip_prefix("wiki-mcp: MCP endpoint ") {
                let _ = endpoint_tx.send(url.to_owned());
            }
            let mut logs = reader_logs.lock().unwrap();
            logs.push_str(&line);
            logs.push('\n');
        }
    });
    let endpoint = endpoint_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("server did not print its endpoint");

    let http = reqwest::Client::new();
    let post = |body: Value| {
        http.post(&endpoint)
            .header("accept", "application/json, text/event-stream")
            .header("content-type", "application/json")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .body(body.to_string())
    };
    let init = rpc(
        1,
        "initialize",
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1.0"}
        }),
    );
    let accepted = [
        init.clone(),
        rpc(2, "tools/list", json!({})),
        // Без обязательного `slug`: ошибка до запроса к API.
        rpc(
            3,
            "tools/call",
            json!({"name": "get_page", "arguments": {}}),
        ),
    ];
    for body in accepted {
        let response = post(body)
            .header("authorization", format!("OAuth {TOKEN}"))
            .header("x-cloud-org-id", ORG)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let text = response.text().await.unwrap();
        assert!(!text.contains(TOKEN) && !text.contains(ORG), "{text}");
    }
    // Отклонённые запросы тоже не пишут значения заголовков.
    let rejected = [
        (
            post(init.clone())
                .header("authorization", format!("Token {TOKEN}"))
                .header("x-org-id", ORG),
            401,
        ),
        (
            post(init.clone())
                .header("authorization", format!("OAuth {TOKEN}"))
                .header("x-org-id", ORG)
                .header("x-cloud-org-id", ORG),
            400,
        ),
        (
            post(init.clone())
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("x-org-id", ORG)
                .header("host", "evil.example"),
            403,
        ),
    ];
    for (request, status) in rejected {
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), status);
        let text = response.text().await.unwrap();
        assert!(!text.contains(TOKEN) && !text.contains(ORG), "{text}");
    }
    // Адрес загрузки файла: путь с секретом в логи не попадает. Адрес неизвестен (создать
    // настоящий нельзя без Wiki API), поэтому ответ — 404 после всех проверок заголовков.
    let upload = endpoint.replace("/mcp", &format!("/uploads/{UPLOAD_ID}/{UPLOAD_SECRET}"));
    let response = http
        .put(&upload)
        .header("authorization", format!("OAuth {TOKEN}"))
        .header("x-cloud-org-id", ORG)
        .body("file content")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 404);
    let text = response.text().await.unwrap();
    assert!(
        !text.contains(UPLOAD_SECRET) && !text.contains(TOKEN),
        "{text}"
    );

    drop(child);
    reader.join().unwrap();
    let mut out = String::new();
    stdout.read_to_string(&mut out).unwrap();
    assert!(out.is_empty(), "stdout: {out}");
    let logs = logs.lock().unwrap().clone();
    // Логи действительно подробные: видны вызов инструмента и запросы MCP.
    assert!(logs.contains("get_page"), "{logs}");
    assert!(logs.contains("TRACE") || logs.contains("DEBUG"), "{logs}");
    assert!(!logs.contains(TOKEN), "{logs}");
    assert!(!logs.contains(ORG), "{logs}");
    assert!(!logs.contains(UPLOAD_SECRET), "{logs}");
    assert!(!logs.contains(UPLOAD_ID), "{logs}");
}
