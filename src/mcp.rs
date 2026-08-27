use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, convert::Infallible, sync::Arc, time::{SystemTime, UNIX_EPOCH}};
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

use stoolap::Database;

pub type SessionMap = Arc<RwLock<HashMap<String, mpsc::Sender<String>>>>;

#[derive(Clone)]
pub struct McpState {
    pub sessions: SessionMap,
    pub db: Database,
    pub api_token: String,
}

#[derive(Deserialize)]
pub struct SessionQuery {
    #[serde(rename = "sessionId")]
    pub session_id: String,
}

#[derive(Deserialize, Debug)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

pub fn router(db: Database, api_token: String) -> Router {
    let state = McpState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
        db,
        api_token,
    };

    Router::new()
        .route("/sse", get(sse_handler))
        .route("/messages", post(message_handler))
        .with_state(state)
}

fn check_auth(headers: &HeaderMap, expected_token: &str) -> bool {
    if let Some(auth_header) = headers.get("Authorization") {
        if let Ok(auth_str) = auth_header.to_str() {
            if auth_str == format!("Bearer {}", expected_token) {
                return true;
            }
        }
    }
    false
}

async fn sse_handler(
    headers: HeaderMap,
    State(state): State<McpState>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    if !check_auth(&headers, &state.api_token) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let session_id = Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::channel::<String>(100);

    state.sessions.write().await.insert(session_id.clone(), tx);

    let stream = async_stream::stream! {
        yield Ok(Event::default().event("endpoint").data(format!("/mcp/messages?sessionId={}", session_id)));

        while let Some(msg) = rx.recv().await {
            yield Ok(Event::default().event("message").data(msg));
        }
    };

    Ok(Sse::new(stream))
}

async fn message_handler(
    headers: HeaderMap,
    State(state): State<McpState>,
    Query(query): Query<SessionQuery>,
    Json(req): Json<JsonRpcRequest>,
) -> Result<(), StatusCode> {
    if !check_auth(&headers, &state.api_token) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let tx = {
        let sessions = state.sessions.read().await;
        sessions.get(&query.session_id).cloned()
    };

    let tx = match tx {
        Some(tx) => tx,
        None => return Err(StatusCode::BAD_REQUEST),
    };

    let response = handle_rpc_method(req, &state).await;

    if let Some(resp) = response {
        if let Ok(resp_str) = serde_json::to_string(&resp) {
            let _ = tx.send(resp_str).await;
        }
    }

    Ok(())
}

async fn handle_rpc_method(req: JsonRpcRequest, state: &McpState) -> Option<JsonRpcResponse> {
    let id = req.id.clone().unwrap_or(Value::Null);

    // Notifications (no id) don't need a response
    if id.is_null() && req.method == "notifications/initialized" {
        return None;
    }

    let result = match req.method.as_str() {
        "initialize" => {
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {}
                },
                "serverInfo": {
                    "name": "terakzor-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            })
        }
        "tools/list" => {
            json!({
                "tools": [
                    {
                        "name": "get_current_status",
                        "description": "Returns the most recent system metrics (CPU, RAM, Disk, Load, Network) for the current moment.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {},
                            "required": []
                        }
                    },
                    {
                        "name": "get_historical_metrics",
                        "description": "Returns historical data for a specific metric over the last N minutes.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "metric": {
                                    "type": "string",
                                    "description": "The exact name of the metric (e.g., cpu_percent, ram_used_bytes, load_average_1m, network_rx_bytes_eth0)"
                                },
                                "minutes": {
                                    "type": "integer",
                                    "description": "Number of minutes into the past to fetch data for (e.g., 60 for the last hour)"
                                }
                            },
                            "required": ["metric", "minutes"]
                        }
                    },
                    {
                        "name": "get_host_info",
                        "description": "Returns information about the host OS, kernel version, hostname, and basic hardware specs.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {},
                            "required": []
                        }
                    }
                ]
            })
        }
        "tools/call" => {
            let params = req.params.unwrap_or(json!({}));
            let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let null_val = Value::Null;
            let arguments = params.get("arguments").unwrap_or(&null_val);

            let tool_result = match tool_name {
                "get_current_status" => execute_get_current_status(state).await,
                "get_historical_metrics" => execute_get_historical_metrics(arguments, state).await,
                "get_host_info" => execute_get_host_info(),
                _ => format!("Error: Unknown tool '{}'", tool_name),
            };

            json!({
                "content": [
                    {
                        "type": "text",
                        "text": tool_result
                    }
                ]
            })
        }
        _ => {
            return Some(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(json!({
                    "code": -32601,
                    "message": "Method not found"
                })),
            });
        }
    };

    Some(JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id,
        result: Some(result),
        error: None,
    })
}

async fn execute_get_current_status(state: &McpState) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let rows = match state.db.query(
        "SELECT metric_name, value FROM metrics WHERE timestamp >= $1",
        stoolap::params![now - 60],
    ) {
        Ok(rows) => rows,
        Err(_) => return "Database error".to_string(),
    };

    let mut out = format!("Current System Status (collected within last minute):\n");
    let mut count = 0;
    for row in rows {
        if let Ok(row) = row {
            if let (Ok(name), Ok(value)) = (row.get::<String>(0), row.get::<f64>(1)) {
                out.push_str(&format!("- {}: {}\n", name, value));
                count += 1;
            }
        }
    }
    
    if count == 0 {
        return "No recent telemetry data available.".to_string();
    }
    out
}

async fn execute_get_historical_metrics(args: &Value, state: &McpState) -> String {
    let metric = args.get("metric").and_then(|m| m.as_str()).unwrap_or("");
    let minutes = args.get("minutes").and_then(|m| m.as_i64()).unwrap_or(60);

    if metric.is_empty() {
        return "Error: metric name is required.".to_string();
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let start = now - (minutes * 60);

    let rows = match state.db.query(
        "SELECT timestamp, value FROM metrics WHERE metric_name = $1 AND timestamp >= $2 ORDER BY timestamp ASC",
        stoolap::params![metric.to_string(), start],
    ) {
        Ok(rows) => rows,
        Err(_) => return "Database error".to_string(),
    };

    let mut points = Vec::new();
    for row in rows {
        if let Ok(row) = row {
            if let (Ok(ts), Ok(val)) = (row.get::<i64>(0), row.get::<f64>(1)) {
                points.push(format!("  [{}] => {}", ts, val));
            }
        }
    }

    if points.is_empty() {
        return format!("No data found for metric '{}' in the last {} minutes.", metric, minutes);
    }

    format!("Historical data for '{}' (last {} minutes, {} samples):\n{}", metric, minutes, points.len(), points.join("\n"))
}

fn execute_get_host_info() -> String {
    use sysinfo::System;
    let mut sys = System::new_all();
    sys.refresh_all();
    
    format!(
        "Host Information:\n\
         - OS: {} {}\n\
         - Kernel: {}\n\
         - Hostname: {}\n\
         - CPU Cores: {}\n\
         - Total Memory: {} bytes",
        System::name().unwrap_or_else(|| "Unknown".to_string()),
        System::os_version().unwrap_or_else(|| "".to_string()),
        System::kernel_version().unwrap_or_else(|| "Unknown".to_string()),
        System::host_name().unwrap_or_else(|| "Unknown".to_string()),
        sys.cpus().len(),
        sys.total_memory(),
    )
}
