use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{Json, Router, extract::State, response::Html, routing::get};
use serde::{Deserialize, Serialize};
use stoolap::Database;
use sysinfo::{Disks, MINIMUM_CPU_UPDATE_INTERVAL, System};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const COLLECTION_INTERVAL: Duration = Duration::from_secs(60);
const RETENTION_CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const DEFAULT_RETENTION_DAYS: u64 = 7;
const SECONDS_PER_DAY: u64 = 24 * 60 * 60;
const CHANNEL_CAPACITY: usize = 16;
const DATABASE_URL: &str = "file://terakzor.db";

const WEB_LISTEN_ADDRESS: &str = "127.0.0.1:3000";
const RECENT_WINDOW_SECONDS: i64 = 24 * 60 * 60;
const SUPPORTED_METRICS: [&str; 5] = [
    "cpu_percent",
    "ram_used_bytes",
    "disk_used_bytes",
    "uptime_seconds",
    "load_average_1m",
];

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_collection_interval_seconds")]
    collection_interval_seconds: u64,
    #[serde(default = "default_retention_days")]
    retention_days: u64,
    #[serde(default = "default_listen_address")]
    listen_address: String,
    #[serde(default)]
    metrics: MetricsConfig,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MetricsConfig {
    #[serde(default = "enabled_by_default")]
    cpu_percent: bool,
    #[serde(default = "enabled_by_default")]
    ram_used_bytes: bool,
    #[serde(default = "enabled_by_default")]
    disk_used_bytes: bool,
    #[serde(default = "enabled_by_default")]
    uptime_seconds: bool,
    #[serde(default = "enabled_by_default")]
    load_average_1m: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            collection_interval_seconds: default_collection_interval_seconds(),
            retention_days: default_retention_days(),
            listen_address: default_listen_address(),
            metrics: MetricsConfig::default(),
        }
    }
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            cpu_percent: true,
            ram_used_bytes: true,
            disk_used_bytes: true,
            uptime_seconds: true,
            load_average_1m: true,
        }
    }
}

impl Config {
    fn from_toml(contents: &str) -> Result<Self, String> {
        let config: Config = toml::from_str(contents).map_err(|error| error.to_string())?;

        if config.collection_interval_seconds == 0 {
            return Err("collection_interval_seconds must be greater than zero".to_owned());
        }

        if config.retention_days == 0 {
            return Err("retention_days must be greater than zero".to_owned());
        }

        if config.retention_days.checked_mul(SECONDS_PER_DAY).is_none() {
            return Err("retention_days is too large".to_owned());
        }

        if config.listen_address.is_empty() {
            return Err("listen_address must not be empty".to_owned());
        }

        Ok(config)
    }

    fn collection_interval(&self) -> Duration {
        Duration::from_secs(self.collection_interval_seconds)
    }

    fn retention(&self) -> Duration {
        Duration::from_secs(self.retention_days * SECONDS_PER_DAY)
    }

    fn listen_address(&self) -> &str {
        &self.listen_address
    }
}

fn default_collection_interval_seconds() -> u64 {
    COLLECTION_INTERVAL.as_secs()
}

fn default_retention_days() -> u64 {
    DEFAULT_RETENTION_DAYS
}

fn default_listen_address() -> String {
    WEB_LISTEN_ADDRESS.to_owned()
}

fn enabled_by_default() -> bool {
    true
}

fn resolve_config_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // ./terakzor.toml (current working directory)
    candidates.push(std::path::PathBuf::from("terakzor.toml"));

    // ~/.config/terakzor/terakzor.toml  (or OS equivalent)
    if let Some(config_dir) = dirs::config_dir() {
        candidates.push(config_dir.join("terakzor").join("terakzor.toml"));
    }

    // /etc/terakzor/terakzor.toml (non-Windows only)
    #[cfg(not(target_os = "windows"))]
    candidates.push(std::path::PathBuf::from("/etc/terakzor/terakzor.toml"));

    candidates
}

fn config_env_value(value: Option<std::ffi::OsString>) -> stoolap::Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(value) => value.into_string().map(Some).map_err(|_| {
            stoolap::Error::internal("TERAKZOR_CONFIG must contain a valid UTF-8 path")
        }),
    }
}

fn command_line_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> stoolap::Result<Vec<String>> {
    args.into_iter()
        .map(|arg| {
            arg.into_string().map_err(|_| {
                stoolap::Error::internal("command-line argument must contain valid UTF-8")
            })
        })
        .collect()
}

fn find_config_path(
    cli_arg: Option<&str>,
    env_var: Option<&str>,
    candidates: &[std::path::PathBuf],
) -> Result<Option<std::path::PathBuf>, String> {
    // 1. --config flag (explicit; missing = fatal)
    if let Some(raw) = cli_arg {
        let path = std::path::PathBuf::from(raw);
        if path.is_file() {
            return Ok(Some(path));
        }
        return Err(format!(
            "--config path does not exist or is not a file: {}",
            path.display()
        ));
    }

    // 2. $TERAKZOR_CONFIG env var (explicit; missing = fatal)
    if let Some(raw) = env_var {
        let path = std::path::PathBuf::from(raw);
        if path.is_file() {
            return Ok(Some(path));
        }
        return Err(format!(
            "TERAKZOR_CONFIG path does not exist or is not a file: {}",
            path.display()
        ));
    }

    // 3-5. Candidate paths (implicit; missing = silent skip)
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(Some(candidate.clone()));
        }
    }

    Ok(None)
}

fn parse_config_arg(args: &[String]) -> Result<Option<&str>, String> {
    match args.iter().position(|arg| arg == "--config") {
        Some(index) => args
            .get(index + 1)
            .map(String::as_str)
            .map(Some)
            .ok_or_else(|| "--config requires a path".to_owned()),
        None => Ok(None),
    }
}

fn load_config(path: &Path) -> stoolap::Result<Config> {
    match fs::read_to_string(path) {
        Ok(contents) => Config::from_toml(&contents).map_err(|error| {
            stoolap::Error::internal(format!("invalid config at {}: {error}", path.display()))
        }),
        Err(error) => Err(stoolap::Error::internal(format!(
            "could not read config at {}: {error}",
            path.display()
        ))),
    }
}

#[derive(Serialize)]
struct MetricsResponse {
    samples: Vec<MetricSample>,
}

#[derive(Serialize)]
struct MetricSample {
    timestamp: i64,
    #[serde(flatten)]
    values: BTreeMap<String, f64>,
}

struct Metric {
    timestamp: i64,
    metric_name: String,
    value: f64,
}

trait MetricSource: Send + 'static {
    fn collect(&mut self) -> Vec<Metric>;
}

struct SysinfoMetricSource {
    system: System,
    disks: Disks,
}

impl SysinfoMetricSource {
    async fn new() -> stoolap::Result<Self> {
        tokio::task::spawn_blocking(|| {
            let mut system = System::new();
            system.refresh_cpu_usage();
            std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
            system.refresh_cpu_usage();
            Self {
                system,
                disks: Disks::new_with_refreshed_list(),
            }
        })
        .await
        .map_err(|error| stoolap::Error::internal(format!("metric source setup failed: {error}")))
    }
}

impl MetricSource for SysinfoMetricSource {
    fn collect(&mut self) -> Vec<Metric> {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        self.disks.refresh(true);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_secs() as i64;

        vec![
            Metric {
                timestamp,
                metric_name: "cpu_percent".to_owned(),
                value: f64::from(self.system.global_cpu_usage()),
            },
            Metric {
                timestamp,
                metric_name: "ram_used_bytes".to_owned(),
                value: self.system.used_memory() as f64,
            },
            Metric {
                timestamp,
                metric_name: "disk_used_bytes".to_owned(),
                value: aggregate_disk_used_bytes(&disk_usage_entries(&self.disks)) as f64,
            },
            Metric {
                timestamp,
                metric_name: "uptime_seconds".to_owned(),
                value: System::uptime() as f64,
            },
            Metric {
                timestamp,
                metric_name: "load_average_1m".to_owned(),
                value: System::load_average().one,
            },
        ]
    }
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
    let database = initialize_database(DATABASE_URL)?;
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
            axum::serve(listener, app(database))
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

fn app(database: Database) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/api/metrics", get(metrics_handler))
        .with_state(Arc::new(database))
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn metrics_handler(
    State(database): State<Arc<Database>>,
) -> Result<Json<MetricsResponse>, (axum::http::StatusCode, String)> {
    tokio::task::spawn_blocking(move || recent_metrics(&database))
        .await
        .map_err(|error| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("metrics query task failed: {error}"),
            )
        })?
        .map(Json)
        .map_err(|error| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                error.to_string(),
            )
        })
}

fn recent_metrics(database: &Database) -> stoolap::Result<MetricsResponse> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_secs() as i64;
    recent_metrics_until(database, now)
}

fn recent_metrics_until(database: &Database, now: i64) -> stoolap::Result<MetricsResponse> {
    let cutoff = now - RECENT_WINDOW_SECONDS;
    let mut samples = BTreeMap::<i64, MetricSample>::new();

    for metric_name in SUPPORTED_METRICS {
        let rows = database.query(
            "SELECT timestamp, value FROM metrics WHERE metric_name = $1 AND timestamp >= $2 ORDER BY timestamp",
            (metric_name, cutoff),
        )?;

        for row in rows {
            let row = row?;
            let timestamp = row.get::<i64>(0)?;
            let value = row.get::<f64>(1)?;
            samples
                .entry(timestamp)
                .or_insert_with(|| MetricSample {
                    timestamp,
                    values: BTreeMap::new(),
                })
                .values
                .insert(metric_name.to_owned(), value);
        }
    }

    Ok(MetricsResponse {
        samples: samples.into_values().collect(),
    })
}

#[cfg(test)]
async fn collector_task<S: MetricSource>(
    source: S,
    sender: mpsc::Sender<Metric>,
) -> stoolap::Result<()> {
    collector_task_with_config(source, sender, Config::default(), CancellationToken::new()).await
}

async fn collector_task_with_config<S: MetricSource>(
    source: S,
    sender: mpsc::Sender<Metric>,
    config: Config,
    shutdown_token: CancellationToken,
) -> stoolap::Result<()> {
    collect_scheduled_cycles_with_config(source, sender, None, config, shutdown_token).await
}

#[cfg(test)]
async fn collect_scheduled_cycles<S: MetricSource>(
    source: S,
    sender: mpsc::Sender<Metric>,
    cycles: Option<usize>,
) -> stoolap::Result<()> {
    collect_scheduled_cycles_with_config(
        source,
        sender,
        cycles,
        Config::default(),
        CancellationToken::new(),
    )
    .await
}

async fn collect_scheduled_cycles_with_config<S: MetricSource>(
    source: S,
    sender: mpsc::Sender<Metric>,
    cycles: Option<usize>,
    config: Config,
    shutdown_token: CancellationToken,
) -> stoolap::Result<()> {
    let mut interval = tokio::time::interval(config.collection_interval());
    let mut source = source;
    let mut completed = 0;

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => return Ok(()),
            _ = interval.tick() => {
                source = collect_once_with_config(
                    source,
                    sender.clone(),
                    &config.metrics,
                    &shutdown_token,
                )
                .await?;
                completed += 1;

                if cycles.is_some_and(|limit| completed >= limit) {
                    return Ok(());
                }
            }
        }
    }
}

async fn collect_once_with_config<S: MetricSource>(
    source: S,
    sender: mpsc::Sender<Metric>,
    metrics_config: &MetricsConfig,
    shutdown_token: &CancellationToken,
) -> stoolap::Result<S> {
    let (source, metrics) = tokio::task::spawn_blocking(move || {
        let mut source = source;
        let metrics = source.collect();
        (source, metrics)
    })
    .await
    .map_err(|error| stoolap::Error::internal(format!("metric collection task failed: {error}")))?;

    for metric in metrics
        .into_iter()
        .filter(|metric| metric_is_enabled(metric, metrics_config))
    {
        if sender.send(metric).await.is_err() {
            if shutdown_token.is_cancelled() {
                break;
            }
            return Err(stoolap::Error::internal("database writer stopped"));
        }
    }

    Ok(source)
}

fn is_virtual_filesystem(file_system: &str) -> bool {
    matches!(
        file_system,
        "tmpfs"
            | "devtmpfs"
            | "devfs"
            | "proc"
            | "procfs"
            | "sysfs"
            | "ramfs"
            | "squashfs"
            | "overlay"
            | "cgroup"
            | "cgroup2"
            | "nsfs"
    )
}

fn aggregate_disk_used_bytes(entries: &[(String, String, u64, u64)]) -> u64 {
    let mut seen_devices = std::collections::HashSet::new();
    let mut used = 0;

    for (device, file_system, total_space, available_space) in entries {
        if is_virtual_filesystem(file_system) || !seen_devices.insert(device.clone()) {
            continue;
        }
        used += total_space.saturating_sub(*available_space);
    }

    used
}

fn disk_usage_entries(disks: &Disks) -> Vec<(String, String, u64, u64)> {
    disks
        .list()
        .iter()
        .map(|disk| {
            (
                disk.name().to_string_lossy().into_owned(),
                disk.file_system().to_string_lossy().into_owned(),
                disk.total_space(),
                disk.available_space(),
            )
        })
        .collect()
}

fn metric_is_enabled(metric: &Metric, config: &MetricsConfig) -> bool {
    match metric.metric_name.as_str() {
        "cpu_percent" => config.cpu_percent,
        "ram_used_bytes" => config.ram_used_bytes,
        "disk_used_bytes" => config.disk_used_bytes,
        "uptime_seconds" => config.uptime_seconds,
        "load_average_1m" => config.load_average_1m,
        _ => false,
    }
}

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_secs() as i64
}

fn delete_expired_metrics(database: &Database, retention: Duration) -> stoolap::Result<i64> {
    let cutoff = unix_seconds_now() - retention.as_secs() as i64;
    database.execute("DELETE FROM metrics WHERE timestamp < $1", (cutoff,))
}

async fn db_writer_task(
    database: Database,
    mut receiver: mpsc::Receiver<Metric>,
    shutdown_token: CancellationToken,
    retention: Duration,
) -> stoolap::Result<()> {
    let mut shutting_down = false;
    let mut cleanup = tokio::time::interval(RETENTION_CLEANUP_INTERVAL);

    loop {
        if shutting_down {
            match receiver.recv().await {
                Some(first) => persist_batch(&database, &mut receiver, vec![first]).await?,
                None => break,
            }
        } else {
            tokio::select! {
                _ = shutdown_token.cancelled() => shutting_down = true,
                _ = cleanup.tick() => {
                    delete_expired_metrics(&database, retention)?;
                }
                first = receiver.recv() => match first {
                    Some(first) => persist_batch(&database, &mut receiver, vec![first]).await?,
                    None => break,
                },
            }
        }
    }

    Ok(())
}

async fn persist_batch(
    database: &Database,
    receiver: &mut mpsc::Receiver<Metric>,
    mut batch: Vec<Metric>,
) -> stoolap::Result<()> {
    while let Ok(metric) = receiver.try_recv() {
        batch.push(metric);
    }

    let database = database.clone();
    tokio::task::spawn_blocking(move || -> stoolap::Result<()> {
        for metric in &batch {
            database.execute(
                "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                (metric.timestamp, metric.metric_name.clone(), metric.value),
            )?;
        }
        Ok(())
    })
    .await
    .map_err(|error| stoolap::Error::internal(format!("database write task failed: {error}")))??;

    Ok(())
}

fn initialize_database(database_url: &str) -> stoolap::Result<Database> {
    let database = Database::open(database_url)?;
    database.execute(
        "CREATE TABLE IF NOT EXISTS metrics (timestamp INTEGER, metric_name TEXT, value REAL)",
        (),
    )?;
    database.execute(
        "CREATE INDEX IF NOT EXISTS idx_metrics_name_time ON metrics(metric_name, timestamp)",
        (),
    )?;
    Ok(database)
}

#[cfg(test)]
async fn run_pipeline_once<S: MetricSource>(database_url: &str, source: S) -> stoolap::Result<()> {
    run_pipeline_once_with_config(database_url, source, Config::default()).await
}

#[cfg(test)]
const LONG_RETENTION: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

#[cfg(test)]
async fn run_pipeline_once_with_config<S: MetricSource>(
    database_url: &str,
    source: S,
    config: Config,
) -> stoolap::Result<()> {
    let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
    let database = initialize_database(database_url)?;
    let writer = tokio::spawn(db_writer_task(
        database,
        receiver,
        CancellationToken::new(),
        LONG_RETENTION,
    ));

    collect_once_with_config(source, sender, &config.metrics, &CancellationToken::new()).await?;

    writer.await.map_err(|error| {
        stoolap::Error::internal(format!("database writer task failed: {error}"))
    })?
}

#[cfg(test)]
mod tests {
    use super::{
        CHANNEL_CAPACITY, COLLECTION_INTERVAL, Config, LONG_RETENTION, Metric, MetricSource,
        SysinfoMetricSource, WEB_LISTEN_ADDRESS, app, collect_scheduled_cycles,
        collect_scheduled_cycles_with_config, collector_task, db_writer_task, initialize_database,
        run_pipeline_once, run_pipeline_once_with_config,
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use tower::ServiceExt;

    static API_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct FakeMetricSource {
        next_timestamp: i64,
    }

    impl FakeMetricSource {
        fn new() -> Self {
            Self {
                next_timestamp: 1_700_000_001,
            }
        }
    }

    impl MetricSource for FakeMetricSource {
        fn collect(&mut self) -> Vec<Metric> {
            let timestamp = self.next_timestamp;
            self.next_timestamp += 1;

            vec![
                Metric {
                    timestamp,
                    metric_name: "cpu_percent".to_owned(),
                    value: 25.0,
                },
                Metric {
                    timestamp,
                    metric_name: "ram_used_bytes".to_owned(),
                    value: 2_048.0,
                },
            ]
        }
    }

    struct FullFakeMetricSource {
        timestamp: i64,
    }

    impl FullFakeMetricSource {
        fn new() -> Self {
            Self {
                timestamp: 1_700_000_001,
            }
        }
    }

    impl MetricSource for FullFakeMetricSource {
        fn collect(&mut self) -> Vec<Metric> {
            let timestamp = self.timestamp;
            self.timestamp += 1;

            vec![
                Metric {
                    timestamp,
                    metric_name: "cpu_percent".to_owned(),
                    value: 25.0,
                },
                Metric {
                    timestamp,
                    metric_name: "ram_used_bytes".to_owned(),
                    value: 2_048.0,
                },
                Metric {
                    timestamp,
                    metric_name: "disk_used_bytes".to_owned(),
                    value: 8_192.0,
                },
                Metric {
                    timestamp,
                    metric_name: "uptime_seconds".to_owned(),
                    value: 3_600.0,
                },
                Metric {
                    timestamp,
                    metric_name: "load_average_1m".to_owned(),
                    value: 1.25,
                },
            ]
        }
    }

    fn api_database() -> stoolap::Database {
        let database = initialize_database("memory://").unwrap();
        database.execute("DELETE FROM metrics", ()).unwrap();
        database
    }

    #[tokio::test]
    async fn api_metrics_groups_all_stored_metrics_by_ordered_timestamp() {
        let _lock = API_TEST_LOCK.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let (first, second) = (now, now + 1);
        let database = api_database();
        database
            .execute(
                "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                (second, "ram_used_bytes", 4_096.0_f64),
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                (first, "ram_used_bytes", 2_048.0_f64),
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                (first, "cpu_percent", 25.0_f64),
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                (second, "cpu_percent", 50.0_f64),
            )
            .unwrap();
        database
            .execute(
                "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                (first, "disk_used_bytes", 8_192.0_f64),
            )
            .unwrap();

        let response = app(database)
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let status = response.status();
        let content_type = response.headers().get(header::CONTENT_TYPE).cloned();

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(content_type.unwrap(), "application/json");
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            json,
            serde_json::json!({
                "samples": [
                    {
                        "timestamp": first,
                        "cpu_percent": 25.0,
                        "disk_used_bytes": 8_192.0,
                        "ram_used_bytes": 2_048.0
                    },
                    {
                        "timestamp": second,
                        "cpu_percent": 50.0,
                        "ram_used_bytes": 4_096.0
                    }
                ]
            })
        );
    }

    #[test]
    fn config_parses_interval_and_metric_toggles() {
        let config = Config::from_toml(
            r#"
collection_interval_seconds = 15

[metrics]
cpu_percent = false
ram_used_bytes = true
disk_used_bytes = true
uptime_seconds = false
load_average_1m = true
"#,
        )
        .unwrap();

        assert_eq!(
            config.collection_interval(),
            std::time::Duration::from_secs(15)
        );
        assert!(!config.metrics.cpu_percent);
        assert!(config.metrics.ram_used_bytes);
        assert!(config.metrics.disk_used_bytes);
        assert!(!config.metrics.uptime_seconds);
        assert!(config.metrics.load_average_1m);
    }

    #[test]
    fn config_defaults_to_the_existing_interval_and_all_metrics_enabled() {
        let config = Config::default();

        assert_eq!(config.collection_interval(), COLLECTION_INTERVAL);
        assert!(config.metrics.cpu_percent);
        assert!(config.metrics.ram_used_bytes);
        assert!(config.metrics.disk_used_bytes);
        assert!(config.metrics.uptime_seconds);
        assert!(config.metrics.load_average_1m);
    }

    #[tokio::test]
    async fn retention_delete_removes_only_rows_older_than_the_policy() {
        let _lock = API_TEST_LOCK.lock().await;
        let database = api_database();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (timestamp, value) in [(now - 8 * 24 * 60 * 60, 11.0), (now - 60 * 60, 22.0)] {
            database
                .execute(
                    "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                    (timestamp, "cpu_percent", value),
                )
                .unwrap();
        }

        let deleted = super::delete_expired_metrics(
            &database,
            std::time::Duration::from_secs(7 * 24 * 60 * 60),
        )
        .unwrap();

        assert_eq!(deleted, 1);
        let remaining = database
            .query(
                "SELECT value FROM metrics WHERE metric_name = $1",
                ("cpu_percent",),
            )
            .unwrap();
        let values: Vec<f64> = remaining
            .into_iter()
            .map(|row| row.unwrap().get::<f64>(0).unwrap())
            .collect();
        assert_eq!(values, vec![22.0]);
    }

    #[test]
    fn disk_usage_dedupes_devices_and_skips_virtual_filesystems() {
        let entries = vec![
            ("/dev/sda1".to_owned(), "ext4".to_owned(), 100u64, 40u64),
            ("/dev/sda1".to_owned(), "ext4".to_owned(), 100, 40),
            ("overlay".to_owned(), "overlay".to_owned(), 50, 10),
            ("/dev/shm".to_owned(), "tmpfs".to_owned(), 64, 0),
            ("/dev/sdb1".to_owned(), "ntfs".to_owned(), 200, 150),
        ];

        assert_eq!(super::aggregate_disk_used_bytes(&entries), 110);
    }

    #[test]
    fn config_rejects_unknown_fields() {
        for contents in [
            "collection_interval_seconds = 60\ncpu_percnet = true",
            "[metrics]\ncpu_percnet = true",
        ] {
            let error = Config::from_toml(contents)
                .map_err(|error| error.to_string())
                .err()
                .expect("unknown fields must be rejected");

            assert!(error.contains("unknown field"), "{error}");
        }
    }

    #[test]
    fn config_parses_retention_days() {
        let config = Config::from_toml("retention_days = 3").unwrap();

        assert_eq!(
            config.retention(),
            std::time::Duration::from_secs(3 * 24 * 60 * 60)
        );
    }

    #[test]
    fn config_defaults_to_seven_retention_days() {
        let config = Config::default();

        assert_eq!(
            config.retention(),
            std::time::Duration::from_secs(7 * 24 * 60 * 60)
        );
    }

    #[test]
    fn config_rejects_a_zero_retention_days() {
        let error = Config::from_toml("retention_days = 0")
            .map_err(|error| error.to_string())
            .err()
            .expect("zero retention days must be rejected");

        assert!(error.contains("retention_days must be greater than zero"));
    }

    #[test]
    fn config_parses_listen_address() {
        let config = Config::from_toml("listen_address = \"0.0.0.0:8080\"").unwrap();

        assert_eq!(config.listen_address(), "0.0.0.0:8080");
    }

    #[test]
    fn config_defaults_to_the_local_web_port() {
        assert_eq!(Config::default().listen_address(), WEB_LISTEN_ADDRESS);
    }

    #[test]
    fn config_rejects_an_empty_listen_address() {
        let error = Config::from_toml("listen_address = \"\"")
            .map_err(|error| error.to_string())
            .err()
            .expect("an empty listen_address must be rejected");

        assert!(
            error.contains("listen_address must not be empty"),
            "{error}"
        );
    }

    #[test]
    fn config_rejects_a_retention_days_that_overflows_seconds() {
        let error = Config::from_toml("retention_days = 18446744073709551615")
            .map_err(|error| error.to_string())
            .err()
            .expect("an overflowing retention_days must be rejected");

        assert!(error.contains("retention_days is too large"), "{error}");
    }

    #[test]
    fn config_rejects_a_zero_collection_interval() {
        let error = Config::from_toml("collection_interval_seconds = 0")
            .map_err(|error| error.to_string())
            .err()
            .expect("a zero interval must be rejected");

        assert!(error.contains("collection_interval_seconds must be greater than zero"));
    }

    #[tokio::test]
    async fn configured_pipeline_persists_only_enabled_fake_metrics() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!("file://{}", directory.path().join("metrics.db").display());
        let config = Config::from_toml(
            r#"
[metrics]
cpu_percent = false
ram_used_bytes = true
disk_used_bytes = true
uptime_seconds = false
load_average_1m = false
"#,
        )
        .unwrap();

        run_pipeline_once_with_config(&database_url, FullFakeMetricSource::new(), config)
            .await
            .unwrap();

        let database = stoolap::Database::open(&database_url).unwrap();
        let rows = database
            .query(
                "SELECT metric_name, value FROM metrics ORDER BY metric_name",
                (),
            )
            .unwrap();
        let values = rows
            .into_iter()
            .map(|row| {
                let row = row.unwrap();
                (row.get::<String>(0).unwrap(), row.get::<f64>(1).unwrap())
            })
            .collect::<Vec<_>>();

        assert_eq!(
            values,
            vec![
                ("disk_used_bytes".to_owned(), 8_192.0),
                ("ram_used_bytes".to_owned(), 2_048.0),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_token_stops_the_collector() {
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let collector = tokio::spawn(collect_scheduled_cycles_with_config(
            FakeMetricSource::new(),
            sender,
            None,
            Config::default(),
            shutdown_token.clone(),
        ));

        receiver
            .recv()
            .await
            .expect("first metric from immediate tick");
        assert!(
            !collector.is_finished(),
            "collector must keep running before cancellation"
        );

        shutdown_token.cancel();
        for _ in 0..100 {
            if collector.is_finished() {
                break;
            }
            tokio::time::advance(std::time::Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
        }

        assert!(
            collector.is_finished(),
            "collector must stop once the token is cancelled"
        );
        collector.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn collector_treats_send_failure_after_cancellation_as_shutdown() {
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        let (sender, receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        drop(receiver);
        shutdown_token.cancel();

        let mut source = super::collect_once_with_config(
            FakeMetricSource::new(),
            sender,
            &Config::default().metrics,
            &shutdown_token,
        )
        .await
        .expect("cancelled collection must not report a send failure");

        assert_eq!(source.collect()[0].timestamp, 1_700_000_002);
    }

    #[tokio::test(start_paused = true)]
    async fn configured_collector_uses_the_toml_interval_without_real_time() {
        let config = Config::from_toml(
            r#"
collection_interval_seconds = 15
"#,
        )
        .unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let collector = tokio::spawn(collect_scheduled_cycles_with_config(
            FakeMetricSource::new(),
            sender,
            Some(2),
            config,
            tokio_util::sync::CancellationToken::new(),
        ));

        receiver
            .recv()
            .await
            .expect("first metric from immediate tick");
        receiver
            .recv()
            .await
            .expect("second metric from immediate tick");
        tokio::time::advance(std::time::Duration::from_secs(14)).await;
        assert!(receiver.try_recv().is_err());
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        receiver
            .recv()
            .await
            .expect("second metric from configured tick");

        collector.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn api_metrics_excludes_samples_outside_the_recent_window() {
        let _lock = API_TEST_LOCK.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let database = api_database();
        for (metric_name, timestamp, value) in [
            ("cpu_percent", now - 100_000, 99.0),
            ("cpu_percent", now, 25.0),
        ] {
            database
                .execute(
                    "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                    (timestamp, metric_name, value),
                )
                .unwrap();
        }

        let response = app(database)
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "samples": [{
                    "timestamp": now,
                    "cpu_percent": 25.0,
                }]
            })
        );
    }

    #[tokio::test]
    async fn api_metrics_returns_all_stored_metric_names() {
        let _lock = API_TEST_LOCK.lock().await;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let database = api_database();
        for (metric_name, value) in [
            ("cpu_percent", 25.0),
            ("disk_used_bytes", 8_192.0),
            ("uptime_seconds", 3_600.0),
            ("load_average_1m", 1.25),
        ] {
            database
                .execute(
                    "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                    (now, metric_name, value),
                )
                .unwrap();
        }

        let response = app(database)
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({
                "samples": [{
                    "timestamp": now,
                    "cpu_percent": 25.0,
                    "disk_used_bytes": 8_192.0,
                    "uptime_seconds": 3_600.0,
                    "load_average_1m": 1.25
                }]
            })
        );
    }

    #[tokio::test]
    async fn api_metrics_returns_an_empty_samples_array_for_no_metrics() {
        let _lock = API_TEST_LOCK.lock().await;
        let response = app(api_database())
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/json"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), br#"{"samples":[]}"#);
    }

    #[tokio::test]
    async fn root_serves_the_embedded_uplot_dashboard() {
        let _lock = API_TEST_LOCK.lock().await;
        let response = app(api_database())
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let page = String::from_utf8(body.to_vec()).unwrap();

        assert!(page.contains("fetch(\"/api/metrics\")"));
        assert!(page.contains("new uPlot"));
        assert!(page.contains("aria-live=\"polite\""));
        assert!(page.contains("ResizeObserver"));
        assert!(page.contains("chart-summary"));
        assert!(page.contains("No telemetry has been collected"));
        assert!(page.contains("latest[name] != null"));
        assert!(page.contains("Object.entries(latest)"));
        assert!(page.contains("metricDefinitions"));
    }

    #[tokio::test]
    async fn api_reads_while_shared_writer_persists_a_file_database() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!("file://{}", directory.path().join("metrics.db").display());
        let database = initialize_database(&database_url).unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let writer = tokio::spawn(db_writer_task(
            database.clone(),
            receiver,
            tokio_util::sync::CancellationToken::new(),
            LONG_RETENTION,
        ));
        let producer = tokio::spawn(async move {
            for timestamp in 1_700_000_001..=1_700_000_064 {
                sender
                    .send(Metric {
                        timestamp,
                        metric_name: "cpu_percent".to_owned(),
                        value: 25.0,
                    })
                    .await
                    .unwrap();
            }
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let count = database
                    .query("SELECT COUNT(*) FROM metrics", ())
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .get::<i64>(0)
                    .unwrap();
                if count > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer should persist while the producer is active");

        let response = app(database.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        producer.await.unwrap();
        writer.await.unwrap().unwrap();
        database.close().unwrap();
        drop(database);

        let reopened = stoolap::Database::open(&database_url).unwrap();
        let count = reopened
            .query("SELECT COUNT(*) FROM metrics", ())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap();
        assert_eq!(count, 64);
    }

    #[tokio::test]
    async fn bind_failure_stops_startup_before_the_pipeline_runs() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!("file://{}", directory.path().join("metrics.db").display());
        let database = initialize_database(&database_url).unwrap();
        let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = blocker.local_addr().unwrap().to_string();
        let config = Config::from_toml(&format!(
            "retention_days = 36500\nlisten_address = \"{address}\""
        ))
        .unwrap();

        let error = super::start_agent(database.clone(), config, FakeMetricSource::new())
            .await
            .expect_err("an occupied address must fail to bind");

        assert!(
            error.to_string().contains("web server bind failed"),
            "{error}"
        );
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let count = database
            .query("SELECT COUNT(*) FROM metrics", ())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap();
        assert_eq!(count, 0, "the pipeline must not have been spawned");
    }

    #[tokio::test(start_paused = true)]
    async fn supervise_propagates_pipeline_errors_without_a_signal() {
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        let token_for_server = shutdown_token.clone();
        let mut pipeline = tokio::spawn(async {
            Err(stoolap::Error::internal("collector exploded")) as stoolap::Result<()>
        });
        let mut server = tokio::spawn(async move {
            token_for_server.cancelled().await;
            Ok(())
        });

        let result = super::supervise(
            &shutdown_token,
            &mut pipeline,
            &mut server,
            std::future::pending::<()>(),
        )
        .await;

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("collector exploded")
        );
        assert!(shutdown_token.is_cancelled(), "server must be told to stop");
        assert!(server.is_finished(), "server must have been joined");
    }

    #[tokio::test(start_paused = true)]
    async fn supervise_stops_pipeline_and_server_on_a_termination_signal() {
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        let token_for_pipeline = shutdown_token.clone();
        let token_for_server = shutdown_token.clone();
        let mut pipeline = tokio::spawn(async move {
            token_for_pipeline.cancelled().await;
            Ok(())
        });
        let mut server = tokio::spawn(async move {
            token_for_server.cancelled().await;
            Ok(())
        });

        let result = super::supervise(
            &shutdown_token,
            &mut pipeline,
            &mut server,
            std::future::ready(()),
        )
        .await;

        result.unwrap();
        assert!(shutdown_token.is_cancelled());
        assert!(pipeline.is_finished(), "pipeline must have been joined");
        assert!(server.is_finished(), "server must have been joined");
    }

    #[tokio::test(start_paused = true)]
    async fn writer_periodically_deletes_metrics_older_than_retention() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!("file://{}", directory.path().join("metrics.db").display());
        let database = initialize_database(&database_url).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        for (timestamp, value) in [(now - 8 * 24 * 60 * 60, 11.0), (now - 60 * 60, 22.0)] {
            database
                .execute(
                    "INSERT INTO metrics (timestamp, metric_name, value) VALUES ($1, $2, $3)",
                    (timestamp, "cpu_percent", value),
                )
                .unwrap();
        }

        let shutdown_token = tokio_util::sync::CancellationToken::new();
        let (sender, receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let writer = tokio::spawn(db_writer_task(
            database.clone(),
            receiver,
            shutdown_token.clone(),
            std::time::Duration::from_secs(7 * 24 * 60 * 60),
        ));

        let mut expired_removed = false;
        for _ in 0..500 {
            if expired_removed {
                break;
            }
            tokio::time::advance(std::time::Duration::from_millis(10)).await;
            tokio::task::yield_now().await;
            let count = |database: &stoolap::Database, older_than: i64| -> i64 {
                database
                    .query(
                        "SELECT COUNT(*) FROM metrics WHERE timestamp < $1",
                        (older_than,),
                    )
                    .unwrap()
                    .next()
                    .unwrap()
                    .unwrap()
                    .get::<i64>(0)
                    .unwrap()
            };
            if count(&database, now - 7 * 24 * 60 * 60) == 0 {
                expired_removed = true;
            }
        }

        assert!(
            expired_removed,
            "writer must delete rows older than the retention policy"
        );
        let recent = database
            .query(
                "SELECT COUNT(*) FROM metrics WHERE timestamp >= $1",
                (now - 2 * 60 * 60,),
            )
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap();
        assert_eq!(recent, 1);

        shutdown_token.cancel();
        drop(sender);
        writer.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_writer_flushes_pending_and_late_metrics_before_exit() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!("file://{}", directory.path().join("metrics.db").display());
        let database = initialize_database(&database_url).unwrap();
        let shutdown_token = tokio_util::sync::CancellationToken::new();
        let (sender, receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let writer = tokio::spawn(db_writer_task(
            database.clone(),
            receiver,
            shutdown_token.clone(),
            LONG_RETENTION,
        ));

        for timestamp in 1_700_000_001..=1_700_000_005 {
            sender
                .send(Metric {
                    timestamp,
                    metric_name: "cpu_percent".to_owned(),
                    value: 25.0,
                })
                .await
                .unwrap();
        }

        shutdown_token.cancel();
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert!(
            !writer.is_finished(),
            "writer must stay alive to drain pending metrics after cancellation"
        );

        sender
            .send(Metric {
                timestamp: 1_700_000_006,
                metric_name: "cpu_percent".to_owned(),
                value: 25.0,
            })
            .await
            .unwrap();
        sender
            .send(Metric {
                timestamp: 1_700_000_007,
                metric_name: "cpu_percent".to_owned(),
                value: 25.0,
            })
            .await
            .unwrap();
        drop(sender);

        writer.await.unwrap().unwrap();
        database.close().unwrap();
        drop(database);

        let reopened = stoolap::Database::open(&database_url).unwrap();
        let count = reopened
            .query("SELECT COUNT(*) FROM metrics", ())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .get::<i64>(0)
            .unwrap();
        assert_eq!(count, 7);
    }

    #[test]
    fn database_rejects_unknown_url_schemes() {
        let error = initialize_database("invalid://").err().unwrap();

        assert!(error.to_string().contains("Unsupported scheme"));
    }

    #[tokio::test]
    async fn sysinfo_source_is_primed_before_collection() {
        let mut source = SysinfoMetricSource::new().await.unwrap();
        let metrics = source.collect();

        assert_eq!(metrics.len(), 5);
        assert_eq!(metrics[0].metric_name, "cpu_percent");
        assert_eq!(metrics[1].metric_name, "ram_used_bytes");
        assert_eq!(metrics[2].metric_name, "disk_used_bytes");
        assert_eq!(metrics[3].metric_name, "uptime_seconds");
        assert_eq!(metrics[4].metric_name, "load_average_1m");
    }

    #[tokio::test(start_paused = true)]
    async fn collector_sends_metrics_on_multiple_intervals() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let collector = tokio::spawn(collector_task(FakeMetricSource::new(), sender));

        let first_cpu = receiver.recv().await.expect("first CPU metric");
        let first_ram = receiver.recv().await.expect("first RAM metric");
        tokio::time::advance(COLLECTION_INTERVAL).await;
        let second_cpu = receiver.recv().await.expect("second CPU metric");
        let second_ram = receiver.recv().await.expect("second RAM metric");

        assert_eq!(first_cpu.timestamp, 1_700_000_001);
        assert_eq!(first_ram.timestamp, 1_700_000_001);
        assert_eq!(second_cpu.timestamp, 1_700_000_002);
        assert_eq!(second_ram.timestamp, 1_700_000_002);

        collector.abort();
        assert!(collector.await.unwrap_err().is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn scheduled_collector_persists_metrics_through_writer() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!("file://{}", directory.path().join("metrics.db").display());
        let database = initialize_database(&database_url).unwrap();
        let (sender, receiver) = tokio::sync::mpsc::channel(CHANNEL_CAPACITY);
        let writer = tokio::spawn(db_writer_task(
            database.clone(),
            receiver,
            tokio_util::sync::CancellationToken::new(),
            LONG_RETENTION,
        ));
        let collector = tokio::spawn(collect_scheduled_cycles(
            FakeMetricSource::new(),
            sender,
            Some(1),
        ));

        collector.await.unwrap().unwrap();
        writer.await.unwrap().unwrap();
        database.close().unwrap();
        drop(database);

        let database = stoolap::Database::open(&database_url).unwrap();
        let rows = database
            .query(
                "SELECT timestamp, metric_name, value FROM metrics ORDER BY metric_name",
                (),
            )
            .unwrap();
        let values = rows
            .into_iter()
            .map(|row| {
                let row = row.unwrap();
                (
                    row.get::<i64>(0).unwrap(),
                    row.get::<String>(1).unwrap(),
                    row.get::<f64>(2).unwrap(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            values,
            vec![
                (1_700_000_001, "cpu_percent".to_owned(), 25.0),
                (1_700_000_001, "ram_used_bytes".to_owned(), 2_048.0),
            ]
        );
    }

    #[tokio::test]
    async fn pipeline_persists_cpu_and_ram_from_metric_source() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!("file://{}", directory.path().join("metrics.db").display());

        run_pipeline_once(&database_url, FakeMetricSource::new())
            .await
            .unwrap();

        let database = stoolap::Database::open(&database_url).unwrap();
        let rows = database
            .query(
                "SELECT timestamp, metric_name, value FROM metrics ORDER BY metric_name",
                (),
            )
            .unwrap();
        let values = rows
            .into_iter()
            .map(|row| {
                let row = row.unwrap();
                (
                    row.get::<i64>(0).unwrap(),
                    row.get::<String>(1).unwrap(),
                    row.get::<f64>(2).unwrap(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            values,
            vec![
                (1_700_000_001, "cpu_percent".to_owned(), 25.0),
                (1_700_000_001, "ram_used_bytes".to_owned(), 2_048.0),
            ]
        );
    }

    #[tokio::test]
    async fn metrics_survive_database_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let database_path = directory.path().join("metrics.db");
        let database_url = format!("file://{}", database_path.display());

        run_pipeline_once(&database_url, FakeMetricSource::new())
            .await
            .unwrap();

        let database = stoolap::Database::open(&database_url).unwrap();
        let rows = database
            .query(
                "SELECT timestamp, metric_name, value FROM metrics WHERE metric_name = $1",
                ("cpu_percent",),
            )
            .unwrap();
        let row = rows.into_iter().next().unwrap().unwrap();

        assert_eq!(row.get::<i64>(0).unwrap(), 1_700_000_001);
        assert_eq!(row.get::<String>(1).unwrap(), "cpu_percent");
        assert_eq!(row.get::<f64>(2).unwrap(), 25.0);
    }

    #[tokio::test]
    async fn metrics_schema_has_name_timestamp_index() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!("file://{}", directory.path().join("metrics.db").display());

        run_pipeline_once(&database_url, FakeMetricSource::new())
            .await
            .unwrap();

        let database = stoolap::Database::open(&database_url).unwrap();
        let indexes = database.query("SHOW INDEXES FROM metrics", ()).unwrap();
        let index = indexes
            .into_iter()
            .map(|row| row.unwrap())
            .find(|row| row.get::<String>(1).unwrap() == "idx_metrics_name_time")
            .expect("metrics name/timestamp index should exist");

        assert_eq!(index.get::<String>(2).unwrap(), "(metric_name, timestamp)");
    }
}

#[cfg(test)]
mod config_path_tests {
    use super::{command_line_args, config_env_value, find_config_path, parse_config_arg};
    use std::{ffi::OsString, path::PathBuf};

    // helper: create a real file in a tempdir so `exists()` returns true
    fn make_file(dir: &tempfile::TempDir, name: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, "").unwrap();
        path
    }

    #[test]
    fn cli_arg_takes_priority_when_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = make_file(&dir, "my.toml");
        let result = find_config_path(Some(cfg.to_str().unwrap()), None, &[]).unwrap();
        assert_eq!(result, Some(cfg));
    }

    #[test]
    fn cli_arg_takes_priority_over_env_var() {
        let dir = tempfile::tempdir().unwrap();
        let cli = make_file(&dir, "cli.toml");
        let env = make_file(&dir, "env.toml");

        let result = find_config_path(
            Some(cli.to_str().unwrap()),
            Some(env.to_str().unwrap()),
            &[],
        )
        .unwrap();

        assert_eq!(result, Some(cli));
    }

    #[test]
    fn cli_arg_takes_priority_over_candidate_file() {
        let dir = tempfile::tempdir().unwrap();
        let cli = make_file(&dir, "cli.toml");
        let candidate = make_file(&dir, "candidate.toml");

        let result = find_config_path(Some(cli.to_str().unwrap()), None, &[candidate]).unwrap();

        assert_eq!(result, Some(cli));
    }

    #[test]
    fn config_arg_returns_the_supplied_path() {
        let args = vec![
            "terakzor".to_owned(),
            "--config".to_owned(),
            "custom.toml".to_owned(),
        ];

        assert_eq!(parse_config_arg(&args).unwrap(), Some("custom.toml"));
    }

    #[test]
    fn config_arg_errors_when_path_is_missing() {
        let args = vec!["terakzor".to_owned(), "--config".to_owned()];
        let error = parse_config_arg(&args).unwrap_err();

        assert!(error.contains("--config requires a path"), "{error}");
    }

    #[test]
    fn cli_arg_errors_when_file_missing() {
        let err = find_config_path(Some("/no/such/file.toml"), None, &[]).unwrap_err();
        assert!(err.contains("/no/such/file.toml"), "got: {err}");
    }

    #[test]
    fn cli_arg_errors_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        let err = find_config_path(Some(path), None, &[]).unwrap_err();

        assert!(err.contains(path), "got: {err}");
    }

    #[test]
    fn env_var_used_when_no_cli_arg_and_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = make_file(&dir, "env.toml");
        let result = find_config_path(None, Some(cfg.to_str().unwrap()), &[]).unwrap();
        assert_eq!(result, Some(cfg));
    }

    #[test]
    fn env_var_takes_priority_over_candidate_file() {
        let dir = tempfile::tempdir().unwrap();
        let env = make_file(&dir, "env.toml");
        let candidate = make_file(&dir, "candidate.toml");

        let result = find_config_path(None, Some(env.to_str().unwrap()), &[candidate]).unwrap();

        assert_eq!(result, Some(env));
    }

    #[test]
    fn env_var_errors_when_file_missing() {
        let err = find_config_path(None, Some("/ghost.toml"), &[]).unwrap_err();
        assert!(err.contains("/ghost.toml"), "got: {err}");
    }

    #[test]
    fn env_var_errors_when_path_is_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_str().unwrap();

        let err = find_config_path(None, Some(path), &[]).unwrap_err();

        assert!(err.contains(path), "got: {err}");
    }

    #[test]
    fn first_existing_candidate_wins() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.toml"); // does NOT exist
        let b = make_file(&dir, "b.toml"); // exists
        let c = make_file(&dir, "c.toml"); // exists but lower priority
        let result = find_config_path(None, None, &[a, b.clone(), c]).unwrap();
        assert_eq!(result, Some(b));
    }

    #[test]
    fn directory_candidate_is_skipped_for_a_later_file() {
        let dir = tempfile::tempdir().unwrap();
        let directory_candidate = dir.path().join("config-directory");
        std::fs::create_dir(&directory_candidate).unwrap();
        let file_candidate = make_file(&dir, "terakzor.toml");

        let result =
            find_config_path(None, None, &[directory_candidate, file_candidate.clone()]).unwrap();

        assert_eq!(result, Some(file_candidate));
    }

    #[test]
    fn config_env_value_handles_missing_and_utf8_values() {
        assert_eq!(config_env_value(None).unwrap(), None);
        assert_eq!(
            config_env_value(Some(OsString::from("config.toml"))).unwrap(),
            Some("config.toml".to_owned())
        );
    }

    #[test]
    fn command_line_args_converts_utf8_values() {
        let args =
            command_line_args([OsString::from("terakzor"), OsString::from("--config")]).unwrap();

        assert_eq!(args, ["terakzor", "--config"]);
    }

    #[cfg(unix)]
    #[test]
    fn command_line_args_rejects_non_utf8_values() {
        use std::os::unix::ffi::OsStringExt;

        let error = command_line_args([OsString::from_vec(vec![0xFF])]).unwrap_err();

        assert!(
            error.to_string().contains("command-line argument"),
            "{error}"
        );
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn config_env_value_rejects_non_utf8_values() {
        use std::os::unix::ffi::OsStringExt;

        let error = config_env_value(Some(OsString::from_vec(vec![0xFF]))).unwrap_err();

        assert!(error.to_string().contains("TERAKZOR_CONFIG"), "{error}");
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    #[test]
    fn returns_none_when_no_candidates_exist() {
        let result = find_config_path(
            None,
            None,
            &[PathBuf::from("/no/a"), PathBuf::from("/no/b")],
        )
        .unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn returns_none_when_no_candidates_given() {
        let result = find_config_path(None, None, &[]).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn candidates_include_cwd_and_user_dir() {
        let candidates = super::resolve_config_candidates();
        assert_eq!(candidates[0], std::path::PathBuf::from("terakzor.toml"));
        if let Some(config_dir) = dirs::config_dir() {
            assert_eq!(
                candidates[1],
                config_dir.join("terakzor").join("terakzor.toml")
            );
        }
    }
}
