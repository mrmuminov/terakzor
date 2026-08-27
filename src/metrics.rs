use crate::config::{Config, MetricsConfig};
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, Networks, MINIMUM_CPU_UPDATE_INTERVAL, System};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct Metric {
    pub timestamp: i64,
    pub metric_name: String,
    pub value: f64,
}

pub trait MetricSource: Send + 'static {
    fn collect(&mut self) -> Vec<Metric>;
}

pub struct SysinfoMetricSource {
    pub system: System,
    pub disks: Disks,
    pub networks: Networks,
}

impl SysinfoMetricSource {
    pub async fn new() -> stoolap::Result<Self> {
        tokio::task::spawn_blocking(|| {
            let mut system = System::new();
            system.refresh_cpu_usage();
            std::thread::sleep(MINIMUM_CPU_UPDATE_INTERVAL);
            system.refresh_cpu_usage();
            Self {
                system,
                disks: Disks::new_with_refreshed_list(),
                networks: Networks::new_with_refreshed_list(),
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
        self.networks.refresh(true);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_secs() as i64;

        let mut metrics = vec![
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
            Metric {
                timestamp,
                metric_name: "load_average_5m".to_owned(),
                value: System::load_average().five,
            },
            Metric {
                timestamp,
                metric_name: "load_average_15m".to_owned(),
                value: System::load_average().fifteen,
            },
            Metric {
                timestamp,
                metric_name: "swap_used_bytes".to_owned(),
                value: self.system.used_swap() as f64,
            },

        ];

        for (interface, data) in &self.networks {
            metrics.push(Metric {
                timestamp,
                metric_name: format!("network_rx_bytes_{}", interface),
                value: data.received() as f64,
            });
            metrics.push(Metric {
                timestamp,
                metric_name: format!("network_tx_bytes_{}", interface),
                value: data.transmitted() as f64,
            });
        }

        metrics
    }
}

#[cfg(test)]
pub async fn collector_task<S: MetricSource>(
    source: S,
    sender: mpsc::Sender<Metric>,
) -> stoolap::Result<()> {
    collector_task_with_config(source, sender, Config::default(), CancellationToken::new()).await
}

pub async fn collector_task_with_config<S: MetricSource>(
    source: S,
    sender: mpsc::Sender<Metric>,
    config: Config,
    shutdown_token: CancellationToken,
) -> stoolap::Result<()> {
    collect_scheduled_cycles_with_config(source, sender, None, config, shutdown_token).await
}

#[cfg(test)]
pub async fn collect_scheduled_cycles<S: MetricSource>(
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

pub async fn collect_scheduled_cycles_with_config<S: MetricSource>(
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

pub async fn collect_once_with_config<S: MetricSource>(
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

pub fn is_virtual_filesystem(file_system: &str) -> bool {
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

pub fn aggregate_disk_used_bytes(entries: &[(String, String, u64, u64)]) -> u64 {
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

pub fn disk_usage_entries(disks: &Disks) -> Vec<(String, String, u64, u64)> {
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

pub fn metric_is_enabled(metric: &Metric, config: &MetricsConfig) -> bool {
    if metric.metric_name.starts_with("network_rx_bytes_") {
        return config.network_rx_bytes;
    }
    if metric.metric_name.starts_with("network_tx_bytes_") {
        return config.network_tx_bytes;
    }
    match metric.metric_name.as_str() {
        "cpu_percent" => config.cpu_percent,
        "ram_used_bytes" => config.ram_used_bytes,
        "disk_used_bytes" => config.disk_used_bytes,
        "uptime_seconds" => config.uptime_seconds,
        "load_average_1m" => config.load_average_1m,
        "load_average_5m" => config.load_average_5m,
        "load_average_15m" => config.load_average_15m,
        "swap_used_bytes" => config.swap_used_bytes,
        _ => false,
    }
}
