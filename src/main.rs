use std::fs;
use std::path::{Path, PathBuf};
use stoolap::Database;
use tokio::sync::mpsc;

pub mod config;
pub mod db;
pub mod mcp;
pub mod metrics;
mod web;

use crate::config::{
    Config, command_line_args, config_env_value, find_config_path, load_config, parse_config_arg,
    resolve_config_candidates,
};
use crate::db::{db_writer_task, initialize_database};
use crate::metrics::{MetricSource, SysinfoMetricSource, collector_task_with_config};
use crate::web::app;

use std::time::Duration;

use tokio_util::sync::CancellationToken;

pub const COLLECTION_INTERVAL: Duration = Duration::from_secs(60);
pub const RETENTION_CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
pub const DEFAULT_RETENTION_DAYS: u64 = 7;
pub const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
pub const CHANNEL_CAPACITY: usize = 16;
pub const WEB_LISTEN_ADDRESS: &str = "localhost:6972";
pub const RECENT_WINDOW_SECONDS: i64 = 24 * 60 * 60;
pub const UPLOT_CSS: &str = include_str!("assets/uplot-1.6.32/uPlot.min.css");
pub const UPLOT_JAVASCRIPT: &str = include_str!("assets/uplot-1.6.32/uPlot.iife.min.js");

fn default_database_url() -> stoolap::Result<String> {
    let data_directory = dirs::data_local_dir()
        .ok_or_else(|| stoolap::Error::internal("could not determine the local data directory"))?;
    database_url_in(&data_directory)
}

fn database_url_in(data_directory: &Path) -> stoolap::Result<String> {
    let database_path: PathBuf = data_directory.join("terakzor").join("terakzor.db");
    let database_directory = database_path.parent().expect("database path has a parent");
    fs::create_dir_all(database_directory).map_err(|error| {
        stoolap::Error::internal(format!(
            "could not create database directory {}: {error}",
            database_directory.display()
        ))
    })?;

    Ok(format!("file://{}", database_path.display()))
}

#[tokio::main]
async fn main() -> stoolap::Result<()> {
    let args = command_line_args(std::env::args_os())?;
    let cli_config = parse_config_arg(&args).map_err(stoolap::Error::internal)?;

    // Resolve which config file to use
    let env_config = config_env_value(std::env::var_os("TERAKZOR_CONFIG"))?;
    let candidates = resolve_config_candidates();
    let config_path = find_config_path(cli_config, env_config.as_deref(), &candidates)
        .map_err(stoolap::Error::internal)?;

    // Print diagnostic so the user always knows what was loaded
    match &config_path {
        Some(p) => eprintln!("config: using {}", p.display()),
        None => eprintln!("config: no file found, using defaults"),
    }

    let config = match config_path {
        Some(path) => load_config(&path)?,
        None => Config::default(),
    };

    let source = SysinfoMetricSource::new().await?;
    let database_url = default_database_url()?;
    let database = initialize_database(&database_url)?;
    start_agent(database, config, source).await
}

async fn start_agent(
    database: Database,
    config: Config,
    source: impl MetricSource,
) -> stoolap::Result<()> {
    let listener = tokio::net::TcpListener::bind(config.listen_address())
        .await
        .map_err(|error| stoolap::Error::internal(format!("web server bind failed: {error}")))?;
    let shutdown_token = CancellationToken::new();

    let mcp_token = config.mcp_token.clone();
    let mut pipeline = tokio::spawn(run_pipeline_with_config(
        source,
        database.clone(),
        config,
        shutdown_token.clone(),
    ));
    let mut server = tokio::spawn({
        let database = database.clone();
        let shutdown_token = shutdown_token.clone();
        async move {
            axum::serve(listener, app(database, mcp_token))
                .with_graceful_shutdown(async move { shutdown_token.cancelled().await })
                .await
                .map_err(|error| stoolap::Error::internal(format!("web server failed: {error}")))
        }
    });

    supervise(
        &shutdown_token,
        &mut pipeline,
        &mut server,
        wait_for_termination_signal(),
    )
    .await?;
    database.close()?;
    Ok(())
}

async fn run_pipeline_with_config<S: MetricSource>(
    source: S,
    database: Database,
    config: Config,
    shutdown_token: CancellationToken,
) -> stoolap::Result<()> {
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let retention = config.retention();
    let writer = tokio::spawn(db_writer_task(
        database,
        receiver,
        shutdown_token.clone(),
        retention,
    ));
    let collector = tokio::spawn(collector_task_with_config(
        source,
        sender,
        config,
        shutdown_token,
    ));

    let collected = collector
        .await
        .map_err(|error| stoolap::Error::internal(format!("collector task failed: {error}")))?;
    let written = writer.await.map_err(|error| {
        stoolap::Error::internal(format!("database writer task failed: {error}"))
    })?;

    collected?;
    written
}

async fn supervise(
    shutdown_token: &CancellationToken,
    pipeline: &mut tokio::task::JoinHandle<stoolap::Result<()>>,
    server: &mut tokio::task::JoinHandle<stoolap::Result<()>>,
    termination_signal: impl Future<Output = ()>,
) -> stoolap::Result<()> {
    let mut pipeline_result = None;
    tokio::select! {
        result = &mut *pipeline => pipeline_result = Some(result),
        _ = termination_signal => {},
    }

    shutdown_token.cancel();

    let collected = match pipeline_result {
        Some(join) => join
            .map_err(|error| stoolap::Error::internal(format!("pipeline task failed: {error}")))?,
        None => pipeline
            .await
            .map_err(|error| stoolap::Error::internal(format!("pipeline task failed: {error}")))?,
    };
    let served = server
        .await
        .map_err(|error| stoolap::Error::internal(format!("web server task failed: {error}")))?;

    collected?;
    served
}

#[cfg(unix)]
async fn wait_for_termination_signal() {
    match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(not(unix))]
async fn wait_for_termination_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
