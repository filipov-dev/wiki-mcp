//! `wiki-mcp call` — вызов инструментов сервера из консоли.
//!
//! Сервер собирается так же, как в `serve` через stdio (права профиля, одобрение на macOS,
//! флаги только сужают), и вызывается MCP-клиентом в этом же процессе ([`mcp_kit::call`]):
//! проверка аргументов, уровни доступа и вырезание секретов те же, что у агента. По
//! умолчанию доступны все наборы инструментов, без наборов по запросу.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::anyhow;
use clap::Args;
use mcp_kit::approval;
use mcp_kit::call::{self, CallArgs, Session, TerminalIo};
use wiki_api::auth::KeyringStore;

use crate::cli::serve::{
    DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_TIMEOUT_SECS, ServeArgs, Setup, init_logging,
};
use crate::clients::EXE_STEM;
use crate::profiles::ProfileRepo;

const CALL_HELP: &str = "\
Without TOOL, lists the tools the profile can use, with their access level. With TOOL and \
--schema, prints the input schema of the tool. Otherwise calls the tool and prints its result; \
the exit code is 1 when the tool returns an error.

Arguments: `name=value` takes the type from the tool's schema (numbers, booleans, JSON arrays \
and objects; a comma-separated list for an array of strings), `name:=JSON` is always JSON, and \
--args takes all arguments as one JSON object (`-` reads it from stdin). Examples:
  wiki-mcp call get_current_user
  wiki-mcp call get_page slug=users/name/page fields=content
  wiki-mcp call api_request method=GET path=/v1/users/me jq='.username'

Access: the same as `serve` without flags: the access of the profile (approved on this Mac on \
macOS); --allow-write, --allow-destructive and --allow-path only narrow it. Destructive tools \
ask for confirmation unless --yes is given. Every toolset is enabled unless --toolsets is given.";

#[derive(Debug, Clone, Args)]
#[command(after_help = CALL_HELP)]
pub struct CallCommand {
    /// Profile with the token and organization [default: default].
    #[arg(long)]
    pub profile: Option<String>,
    /// Limit the access to write (the profile must allow it).
    #[arg(long)]
    pub allow_write: bool,
    /// Limit the access to destructive (the profile must allow it).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Limit the extra directories of the profile to this one. Repeatable.
    #[arg(long, value_name = "DIR")]
    pub allow_path: Vec<PathBuf>,
    /// Comma-separated toolsets to enable [default: all].
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Additional custom tools file. Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
    /// Timeout of one API request, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Maximum size of an API response; larger responses are an error.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    pub max_response_bytes: usize,
    #[command(flatten)]
    pub call: CallArgs,
}

impl CallCommand {
    /// Аргументы `serve`, с которыми собирается сервер.
    pub fn serve_args(&self) -> ServeArgs {
        ServeArgs {
            allow_write: self.allow_write,
            allow_destructive: self.allow_destructive,
            toolsets: if self.toolsets.is_empty() {
                vec![wiki_mcp_core::ALL_TOOLSETS.to_owned()]
            } else {
                self.toolsets.clone()
            },
            tools_file: self.tools_file.clone(),
            allow_path: self.allow_path.clone(),
            timeout: self.timeout,
            max_response_bytes: self.max_response_bytes,
            ..ServeArgs::for_profile(self.profile.clone())
        }
    }
}

pub async fn run(args: CallCommand) -> anyhow::Result<ExitCode> {
    init_logging(None)?;
    let repo = ProfileRepo::open_default()?;
    let approvals = approval::platform_store(EXE_STEM);
    let setup = Setup::load(
        &args.serve_args(),
        &KeyringStore,
        &repo,
        approvals.as_deref(),
    )?;
    if let Some(warning) = &setup.warning {
        eprintln!("wiki-mcp: {warning}");
    }
    let outcome = call_with(setup, &args.call, &mut TerminalIo::default()).await?;
    Ok(ExitCode::from(outcome.exit_code()))
}

/// Собирает сервер из `setup` и выполняет `call`. Отдельно от [`run`] для тестов.
pub async fn call_with(
    mut setup: Setup,
    args: &CallArgs,
    io: &mut dyn call::CallIo,
) -> anyhow::Result<call::CallOutcome> {
    // Наборы по запросу в консоли не нужны: доступны все включённые наборы сразу.
    setup.options.dynamic_toolsets = false;
    let session = Session::start(setup.server()?)
        .await
        .map_err(|err| anyhow!(err))?;
    let outcome = call::run(&session, args, io).await;
    session.close().await;
    outcome.map_err(|err| anyhow!(err))
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};

    use clap::Parser;
    use serde_json::json;
    use wiki_api::auth::{MemoryStore, TokenStore};
    use wiki_api::{ApiToken, OrgKind, Organization};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::profiles::ProfileSettings;

    #[derive(Debug, Parser)]
    struct Wrapper {
        #[command(flatten)]
        call: CallCommand,
    }

    fn parse(args: &[&str]) -> CallCommand {
        let mut argv = vec!["call"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).unwrap().call
    }

    #[derive(Default)]
    struct BufferIo {
        out: Vec<u8>,
        confirm: bool,
        asked: Vec<String>,
    }

    impl call::CallIo for BufferIo {
        fn out(&mut self) -> &mut dyn Write {
            &mut self.out
        }
        fn read_stdin(&mut self) -> io::Result<String> {
            Ok(String::new())
        }
        fn confirm(&mut self, question: &str) -> io::Result<bool> {
            self.asked.push(question.to_owned());
            Ok(self.confirm)
        }
    }

    /// Профиль во временном каталоге и токен в памяти: настоящий keychain не трогаем.
    fn load_setup(
        args: &CallCommand,
        settings: ProfileSettings,
        api: &MockServer,
    ) -> (tempfile::TempDir, Setup) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("config"));
        repo.upsert("ci", settings).unwrap();
        let store = MemoryStore::new();
        store.set("ci", &ApiToken::new("call-test-token")).unwrap();
        let mut setup = Setup::load(&args.serve_args(), &store, &repo, None).unwrap();
        setup.config.base_url = api.uri().parse().unwrap();
        // В тестах текущий бинарник — тестовый, подкоманды `jq-worker` у него нет.
        setup.options.jq = wiki_mcp_core::JqRunner::in_process();
        (dir, setup)
    }

    fn settings(write: bool, destructive: bool) -> ProfileSettings {
        ProfileSettings {
            allow_write: write,
            allow_destructive: destructive,
            org: Some(Organization::new(OrgKind::Yandex360, "12345").unwrap()),
            ..ProfileSettings::default()
        }
    }

    #[test]
    fn parses_flags_and_tool_arguments() {
        let args = parse(&[
            "--profile",
            "ci",
            "--allow-write",
            "get_page",
            "slug=team/page",
            "fields=content",
        ]);
        assert_eq!(args.call.tool.as_deref(), Some("get_page"));
        assert_eq!(args.call.params, ["slug=team/page", "fields=content"]);
        let serve = args.serve_args();
        assert_eq!(serve.toolsets, ["all"]);
        assert!(serve.allow_write && !serve.dynamic_toolsets && !serve.http);
        assert_eq!(serve.profile(), "ci");
        assert_eq!(
            parse(&["--toolsets", "grids"]).serve_args().toolsets,
            ["grids"]
        );
        assert_eq!(parse(&[]).serve_args().profile(), "default");
    }

    #[tokio::test]
    async fn lists_and_calls_tools_within_the_profile_access() {
        let api = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/users/me"))
            .and(header("authorization", "OAuth call-test-token"))
            .and(header("x-org-id", "12345"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"username": "robot"})))
            .expect(1)
            .mount(&api)
            .await;

        let args = parse(&["--profile", "ci"]);
        let (_dir, setup) = load_setup(&args, settings(false, false), &api);
        let mut io = BufferIo::default();
        let outcome = call_with(setup, &args.call, &mut io).await.unwrap();
        assert_eq!(outcome.exit_code(), 0);
        let list = String::from_utf8(io.out).unwrap();
        assert!(list.contains("get_current_user"), "{list}");
        // Все наборы, но только чтение: инструментов записи нет.
        assert!(list.contains("list_spaces"), "{list}");
        assert!(!list.contains("create_page"), "{list}");
        assert!(!list.contains("enable_toolset"), "{list}");

        let args = parse(&["--profile", "ci", "get_current_user"]);
        let (_dir, setup) = load_setup(&args, settings(false, false), &api);
        let mut io = BufferIo::default();
        call_with(setup, &args.call, &mut io).await.unwrap();
        let out = String::from_utf8(io.out).unwrap();
        assert!(out.contains("robot"), "{out}");
        assert!(!out.contains("call-test-token"), "{out}");
    }

    #[tokio::test]
    async fn destructive_tools_ask_first() {
        let api = MockServer::start().await;
        let args = parse(&["--profile", "ci", "delete_page", "idx=1"]);
        let (_dir, setup) = load_setup(&args, settings(true, true), &api);
        let mut io = BufferIo::default();
        let err = call_with(setup, &args.call, &mut io).await.unwrap_err();
        assert!(err.to_string().contains("delete_page"), "{err}");
        assert_eq!(io.asked.len(), 1);
        assert!(api.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn flags_beyond_the_profile_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        repo.upsert("ci", settings(false, false)).unwrap();
        let store = MemoryStore::new();
        store.set("ci", &ApiToken::new("call-test-token")).unwrap();
        let args = parse(&["--profile", "ci", "--allow-write"]);
        let err = Setup::load(&args.serve_args(), &store, &repo, None)
            .err()
            .unwrap();
        assert!(err.to_string().contains("allows only `read`"), "{err}");
    }
}
