use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lore::cli::{AdminCommand, Client};
use lore::config::{Config, LogFormat};
use lore::http::{AppState, router};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "lore",
    version,
    about = "Ask questions of your traces, sessions and code"
)]
struct Cli {
    /// Base URL of the lore server (client commands)
    #[arg(
        long,
        global = true,
        env = "LORE_URL",
        default_value = "http://localhost:8080"
    )]
    url: String,
    /// Tenant API key (client commands)
    #[arg(long, global = true, env = "LORE_KEY", hide_env_values = true)]
    key: Option<String>,
    /// Admin token (admin commands)
    #[arg(long, global = true, env = "LORE_ADMIN_TOKEN", hide_env_values = true)]
    admin_token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the HTTP server (applies migrations on start)
    Serve,
    /// Apply database migrations and exit
    Migrate,
    /// Manage tenants and API keys (needs the admin token)
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Show the tenant and scopes of the API key
    Me,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            let config = Config::from_env()?;
            init_tracing(&config)?;
            serve(config).await
        }
        Command::Migrate => {
            let config = Config::from_env()?;
            init_tracing(&config)?;
            let pool = lore::db::connect(&config.database_url).await?;
            lore::db::migrate(&pool).await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Command::Admin { command } => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_admin(&client, command).await
        }
        Command::Me => {
            let client = Client::new(&cli.url, cli.key, cli.admin_token)?;
            lore::cli::run_me(&client).await
        }
    }
}

fn init_tracing(config: &Config) -> Result<()> {
    let filter = EnvFilter::try_new(&config.log)
        .with_context(|| format!("invalid LORE_LOG filter {:?}", config.log))?;
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match config.log_format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
    Ok(())
}

async fn serve(config: Config) -> Result<()> {
    let pool = lore::db::connect(&config.database_url).await?;
    lore::db::migrate(&pool).await?;

    // Load the embedder before binding, so the first-start model download finishes before the
    // server answers anything. Loading is blocking and CPU/network heavy.
    let started = std::time::Instant::now();
    let embedder = {
        let config = config.clone();
        tokio::task::spawn_blocking(move || lore::embed::from_config(&config))
            .await
            .context("embedder load task failed")??
    };
    tracing::info!(
        embedder = embedder.name(),
        load_ms = started.elapsed().as_millis() as u64,
        "embedder ready"
    );

    let listener = tokio::net::TcpListener::bind(&config.listen_addr)
        .await
        .with_context(|| format!("binding {}", config.listen_addr))?;
    tracing::info!(addr = %config.listen_addr, version = %config.version, "lore listening");

    let state = AppState {
        pool,
        config: Arc::new(config),
        embedder,
        source_locks: Default::default(),
    };
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serving")?;
    tracing::info!("shut down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "failed to listen for ctrl-c");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}
