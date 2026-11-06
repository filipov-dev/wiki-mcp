//! Бинарник `wiki-mcp`: CLI и транспорт MCP-сервера Yandex Wiki.
//!
//! `main` только разбирает аргументы и вызывает модуль подкоманды из [`cli`].

mod browser;
mod cli;
mod clients;
mod login;
mod oauth;
mod profiles;
mod ui;
mod verify;

use clap::{Parser, Subcommand};

/// MCP server and CLI for the Yandex Wiki Public API.
#[derive(Debug, Parser)]
#[command(name = "wiki-mcp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the MCP server over stdio, or over HTTP with --http.
    Serve(cli::serve::ServeArgs),
    /// Manage stored tokens and Wiki organizations of profiles.
    #[command(subcommand)]
    Auth(cli::auth::AuthCommand),
    /// Register the server in an MCP client (Claude Code or Codex) for a profile.
    ///
    /// Sets up your Yandex OAuth application, the organization and the token of the profile,
    /// then adds `wiki-mcp serve --profile <profile>` to the client config after
    /// confirmation. The token never goes to the client config.
    Connect(cli::connect::ConnectArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Спецификация встроена в бинарник; ссылка на неё не даёт линковщику её выбросить.
    let _ = wiki_mcp_core::wiki_api::OPENAPI_JSON.len();
    match Cli::parse().command {
        Command::Serve(args) => cli::serve::run(args).await,
        Command::Auth(command) => cli::auth::run(command).await,
        Command::Connect(args) => cli::connect::run(args).await,
    }
}
