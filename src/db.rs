use crate::RETENTION_CLEANUP_INTERVAL;
use crate::metrics::Metric;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use stoolap::Database;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_secs() as i64
}

pub fn delete_expired_metrics(database: &Database, retention: Duration) -> stoolap::Result<i64> {
    let cutoff = unix_seconds_now() - retention.as_secs() as i64;
    database.execute("DELETE FROM metrics WHERE timestamp < $1", (cutoff,))
}

pub async fn db_writer_task(
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

pub async fn persist_batch(
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

pub fn initialize_database(database_url: &str) -> stoolap::Result<Database> {
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
