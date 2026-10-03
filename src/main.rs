#![warn(clippy::all, clippy::nursery, clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::cast_precision_loss,
    clippy::significant_drop_tightening
)]

mod database;
mod migrations;
mod settings;
pub mod telegram;

use crate::database::create_pool;
use crate::migrations::Migrations;
use crate::settings::Settings;
use crate::telegram::Telegram;
use deadpool_postgres::Pool;
use markovchain_client_lib::MarkovChainClient;
use rand::distr::Uniform;
use std::env;
use std::io::stdout;
use std::path::Path;
use std::sync::Arc;
use tokio::join;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[allow(unused)]
pub struct AppState {
    settings: Settings,
    db_pool: Pool,
    markov_chain: MarkovChainClient,
    uniform: Uniform<u8>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[rustfmt::skip]
    let env_filter = EnvFilter::builder().parse_lossy(
        env::var("RUST_LOG")
          .as_deref()
          .unwrap_or("info"),
    );

    let file_appender = tracing_appender::rolling::hourly("logs", "rolling.log");
    let (non_blocking_file, _file_guard) = tracing_appender::non_blocking(file_appender);
    let (non_blocking_stdout, _stdout_guard) = tracing_appender::non_blocking(stdout());
    let console = tracing_subscriber::fmt::layer().with_writer(non_blocking_stdout);

    let file = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(non_blocking_file);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console)
        .with(file)
        .init();

    info!("Welcome to {}", env!("CARGO_PKG_NAME"));

    let settings = Settings::parse().expect("Failed to load settings");

    let db_pool = create_pool(&settings.database)
        .await
        .expect("Failed to create pool");
    let client = db_pool.get().await.expect("Failed to get client from pool");

    Migrations::new("version_info".into(), Path::new("migrations"))
        .expect("Failed to initialize migrations")
        .up(&client)
        .await
        .expect("Failed to apply migrations");

    let cancellation_token = CancellationToken::new();

    let app_state = Arc::new(AppState {
        markov_chain: MarkovChainClient::new(settings.server.url.clone()),
        uniform: Uniform::try_from(5..=50)?,
        settings,
        db_pool,
    });

    let telegram = Arc::new(Telegram::new(app_state));

    let ctrl_c_handle = async || {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to listen for CTRL+C");

        cancellation_token.cancel();
        info!("Ctrl-C received, shutting down");
    };

    join!(
        ctrl_c_handle(),
        telegram.start(cancellation_token.child_token())
    );

    Ok(())
}
