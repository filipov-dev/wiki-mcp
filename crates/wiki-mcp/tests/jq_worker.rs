//! Фильтры `jq` в отдельном процессе: настоящий бинарник как исполнитель.

use std::time::Duration;

use serde_json::json;
use wiki_mcp_core::JqRunner;
use wiki_mcp_core::jq::JqFilter;

fn runner() -> JqRunner {
    JqRunner::subprocess(env!("CARGO_BIN_EXE_wiki-mcp"), ["jq-worker"])
}

fn filter(code: &str) -> JqFilter {
    JqFilter::parse(code).unwrap()
}

#[tokio::test]
async fn evaluates_in_subprocess() {
    let out = runner()
        .run(
            &filter(".[] | {slug}"),
            json!([{"slug": "team/a", "title": "A"}, {"slug": "team/b"}]),
            1024,
        )
        .await
        .unwrap();
    assert_eq!(out.text, r#"[{"slug":"team/a"},{"slug":"team/b"}]"#);
    assert!(!out.truncated);

    let out = runner()
        .run(&filter("range(100000)"), json!(null), 100)
        .await
        .unwrap();
    assert!(out.truncated && out.text.len() <= 100);

    let err = runner()
        .run(&filter("error(\"boom\")"), json!(null), 100)
        .await
        .unwrap_err();
    assert!(err.contains("boom"), "{err}");
}

#[tokio::test]
async fn unbounded_recursion_does_not_crash_the_server() {
    let err = runner()
        .run(&filter("def f: 1 + f; f"), json!(null), 1024)
        .await
        .unwrap_err();
    assert!(err.contains("crashed"), "{err}");
}

#[tokio::test]
async fn endless_filter_is_killed() {
    let err = runner()
        .with_timeout(Duration::from_millis(500))
        .run(&filter("last(range(1e18))"), json!(null), 1024)
        .await
        .unwrap_err();
    assert!(err.contains("did not finish"), "{err}");
}
