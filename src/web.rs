use crate::{RECENT_WINDOW_SECONDS, UPLOT_CSS, UPLOT_JAVASCRIPT};
use axum::{
    Router,
    extract::State,
    http::header,
    response::{Html, IntoResponse, Json, Response},
    routing::get,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use stoolap::Database;

#[derive(Serialize)]
pub struct MetricsResponse {
    pub samples: Vec<MetricSample>,
}

#[derive(Serialize)]
pub struct MetricSample {
    pub timestamp: i64,
    #[serde(flatten)]
    pub values: BTreeMap<String, f64>,
}

pub fn app(database: Database, mcp_token: String) -> Router {
    Router::new()
        .route("/", get(index_handler))
        .route("/api/metrics", get(metrics_handler))
        .route("/assets/uplot-1.6.32/uPlot.min.css", get(uplot_css_handler))
        .route(
            "/assets/uplot-1.6.32/uPlot.iife.min.js",
            get(uplot_javascript_handler),
        )
        .with_state(Arc::new(database.clone()))
        .nest("/mcp", crate::mcp::router(database, mcp_token))
}

pub async fn index_handler() -> Response {
    (
        [(header::CACHE_CONTROL, "no-cache")],
        Html(include_str!("index.html")),
    )
        .into_response()
}

pub async fn uplot_css_handler() -> Response {
    static_asset(UPLOT_CSS, "text/css; charset=utf-8")
}

pub async fn uplot_javascript_handler() -> Response {
    static_asset(UPLOT_JAVASCRIPT, "text/javascript; charset=utf-8")
}

pub fn static_asset(contents: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        contents,
    )
        .into_response()
}

pub async fn metrics_handler(
    State(database): State<Arc<Database>>,
) -> Result<impl IntoResponse, (axum::http::StatusCode, String)> {
    tokio::task::spawn_blocking(move || recent_metrics(&database))
        .await
        .map_err(|error| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("metrics query task failed: {error}"),
            )
        })?
        .map(|metrics| ([(header::CACHE_CONTROL, "no-store")], Json(metrics)))
        .map_err(|error| {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                error.to_string(),
            )
        })
}

pub fn recent_metrics(database: &Database) -> stoolap::Result<MetricsResponse> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_secs() as i64;
    recent_metrics_until(database, now)
}

pub fn recent_metrics_until(database: &Database, now: i64) -> stoolap::Result<MetricsResponse> {
    let cutoff = now - RECENT_WINDOW_SECONDS;
    let mut samples = BTreeMap::<i64, MetricSample>::new();

    let rows = database.query(
        "SELECT timestamp, metric_name, value FROM metrics WHERE timestamp >= $1 ORDER BY timestamp",
        (cutoff,),
    )?;

    for row in rows {
        let row = row?;
        let timestamp = row.get::<i64>(0)?;
        let metric_name = row.get::<String>(1)?;
        let value = row.get::<f64>(2)?;
        samples
            .entry(timestamp)
            .or_insert_with(|| MetricSample {
                timestamp,
                values: BTreeMap::new(),
            })
            .values
            .insert(metric_name, value);
    }

    Ok(MetricsResponse {
        samples: samples.into_values().collect(),
    })
}
