mod apikeys;
mod availability;
mod bests;
mod config;
mod db;
mod digest;
mod envelope;
mod garmin;
mod importer;
mod llm;
mod metrics;
mod oauth;
mod performance;
mod planned;
mod report;
mod rules;
mod route_profile;
mod route_thumb;
mod segments;
mod settings;
mod strava;
mod sync;
mod web;
mod workout;
mod zwift;
mod zwo;

use clap::{Parser, Subcommand};
use config::Config;

#[derive(Parser)]
#[command(name = "cyclai")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch new Strava activities/streams, run deterministic calcs, generate per-activity bilans.
    /// Triggered daily by a systemd timer.
    Sync {
        /// Walk the whole Strava history (not just activities since the last
        /// sync) and import whatever isn't stored yet — e.g. past runs.
        #[arg(long)]
        full: bool,
    },
    /// Run the web dashboard (UI only, no API) — keep it on a private network.
    Serve {
        /// Address to listen on.
        #[arg(long, env = "BIND_ADDR", default_value = "0.0.0.0:3000")]
        bind: String,
    },
    /// Run the JSON API only (`/api/v1`), guarded by RS256 bearer tokens — the
    /// process that can be exposed. Keys are managed with `api-key`.
    ServeApi {
        /// Address to listen on.
        #[arg(long, env = "BIND_ADDR", default_value = "0.0.0.0:3001")]
        bind: String,
    },
    /// Manage the public keys allowed to call the API.
    #[command(subcommand)]
    ApiKey(ApiKeyCommand),
    /// Manage the OAuth access of MCP connectors (Claude web/mobile).
    #[command(subcommand)]
    Oauth(OauthCommand),
    /// One-time (or re-run as needed) import of starred Strava segments'
    /// coordinates into reference_segments. Starring itself is manual.
    ImportSegments,
}

#[derive(Subcommand)]
enum ApiKeyCommand {
    /// Register a client's RSA public key (PEM file, or `-` for stdin).
    Add {
        /// Key name; clients put it in the JWT `kid` header.
        name: String,
        pem_file: String,
        /// `read` (GET only) or `write` (also POST/DELETE, e.g. planning).
        #[arg(long, default_value = "read")]
        scope: String,
    },
    /// List registered keys.
    List,
    /// Revoke a key (effective on its next request).
    Revoke { name: String },
    /// Mint a short-lived token from a private key — to test the API by hand.
    Token {
        /// Key name registered with `add` (JWT `kid`).
        name: String,
        /// RSA private key (PEM file).
        private_key_file: String,
        /// Validity in seconds (max 300).
        #[arg(long, default_value_t = 60)]
        ttl: i64,
    },
}

#[derive(Subcommand)]
enum OauthCommand {
    /// Set the password asked on the consent page (read from stdin, one line).
    SetPassword,
    /// List the registered connectors.
    List,
    /// Revoke a connector by client id, or `all` (its tokens stop working at once).
    Revoke { client_id: String },
    /// Days of inactivity after which a connector must be authorized again.
    SetRefreshDays { days: i64 },
}

async fn oauth_command(pool: &sqlx::SqlitePool, cmd: OauthCommand) -> anyhow::Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    match cmd {
        OauthCommand::SetPassword => {
            let interactive = std::io::stdin().is_terminal();
            let mut read = |prompt: &str| -> anyhow::Result<String> {
                if interactive {
                    eprint!("{prompt}");
                    std::io::stderr().flush()?;
                }
                let mut line = String::new();
                std::io::stdin().lock().read_line(&mut line)?;
                Ok(line.trim_end_matches(['\r', '\n']).to_string())
            };
            let password = read("New password (visible as you type, min 12 characters): ")?;
            if interactive && read("Again: ")? != password {
                anyhow::bail!("the two entries differ");
            }
            oauth::set_password(pool, &password).await?;
            println!("password set");
        }
        OauthCommand::List => {
            for c in oauth::list(pool).await? {
                println!(
                    "{}	{}	created {}	last used {}	{}",
                    c.id,
                    c.name.as_deref().unwrap_or("-"),
                    c.created_at,
                    c.last_used_at.as_deref().unwrap_or("never"),
                    if c.revoked { "revoked" } else { "active" },
                );
            }
        }
        OauthCommand::Revoke { client_id } => {
            let n = oauth::revoke(pool, &client_id).await?;
            if n == 0 {
                anyhow::bail!("no active connector '{client_id}'");
            }
            println!("{n} connector(s) revoked");
        }
        OauthCommand::SetRefreshDays { days } => {
            if !(1..=3650).contains(&days) {
                anyhow::bail!("days must be between 1 and 3650");
            }
            sqlx::query("UPDATE oauth_settings SET refresh_days = ? WHERE id = 1").bind(days).execute(pool).await?;
            println!("connectors must re-authorize after {days} days of inactivity (applies to tokens issued from now on)");
        }
    }
    Ok(())
}

async fn api_key_command(pool: &sqlx::SqlitePool, cmd: ApiKeyCommand) -> anyhow::Result<()> {
    match cmd {
        ApiKeyCommand::Add { name, pem_file, scope } => {
            let pem = if pem_file == "-" {
                std::io::read_to_string(std::io::stdin())?
            } else {
                std::fs::read_to_string(&pem_file)?
            };
            apikeys::add(pool, &name, &pem, apikeys::Scope::parse(&scope)?).await?;
            println!("key '{name}' added ({scope})");
        }
        ApiKeyCommand::List => {
            for (name, scope, created, last_used, revoked) in apikeys::list(pool).await? {
                println!(
                    "{name}	{scope}	created {created}	last used {}	{}",
                    last_used.as_deref().unwrap_or("never"),
                    revoked.map_or("active".to_string(), |r| format!("revoked {r}")),
                );
            }
        }
        ApiKeyCommand::Revoke { name } => {
            if apikeys::revoke(pool, &name).await? {
                println!("key '{name}' revoked");
            } else {
                anyhow::bail!("no active key named '{name}'");
            }
        }
        ApiKeyCommand::Token { name, private_key_file, ttl } => {
            let pem = std::fs::read_to_string(&private_key_file)?;
            println!("{}", apikeys::mint_token(&pem, &name, ttl)?);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    let config = Config::from_env()?;
    let pool = db::connect(&config.database_url).await?;
    db::seed_athlete_config(&pool, &config).await?;
    db::seed_settings(&pool, &config).await?;
    db::seed_sync_state(&pool, &config).await?;
    zwift::seed(&pool).await?;

    match cli.command {
        Command::Sync { full } => {
            if full {
                sync::run_full(&pool).await?;
            } else {
                sync::run(&pool).await?;
            }
        }
        Command::Serve { bind } => {
            web::serve(pool, &bind).await?;
        }
        Command::ServeApi { bind } => {
            web::serve_api(pool, &bind).await?;
        }
        Command::ApiKey(cmd) => {
            api_key_command(&pool, cmd).await?;
        }
        Command::Oauth(cmd) => {
            oauth_command(&pool, cmd).await?;
        }
        Command::ImportSegments => {
            segments::import_starred(&pool).await?;
        }
    }

    Ok(())
}
