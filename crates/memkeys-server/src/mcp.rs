//! Minimal stdio JSON-RPC 2.0 MCP server (M3, WS-5).
//!
//! Implements the four MemKeys tools over the Model Context Protocol:
//!   memory_store    → service.ingest()
//!   memory_fetch    → service.fetch_by_keys()
//!   memory_traverse → service.traverse()
//!   memory_search   → 501 graceful (semantic search not in M3)
//!
//! The tenant is supplied by the MCP caller as a `tenant_id` argument on every
//! tool call (WS-4 Invariant T — never process state).
//!
//! Protocol: JSON-RPC 2.0 over stdin/stdout, one request per line, one
//! response per line (newline-delimited). Implements `initialize`,
//! `tools/list`, and `tools/call`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::envelope::Envelope;
use crate::error::{IngestError, QueryError};
use crate::graph::{
    Direction, EdgeType, GraphError, NodeRef, TraverseReq, DEFAULT_DEADLINE, DEFAULT_FANOUT_CAP,
    DEFAULT_NODE_BUDGET,
};
use crate::model::{EntityKey, KeyedQuery, Validity};
use crate::pipeline::CallerContext;
use crate::service::MemKeysService;
use crate::telemetry::{self, TRANSPORT_STDIO};
use opentelemetry::trace::{FutureExt, Status, TraceContextExt};
use opentelemetry::Context;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 wire types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcResponse {
    fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    fn err(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool descriptors
// ---------------------------------------------------------------------------

fn tool_list() -> Value {
    serde_json::json!({
        "tools": [
            {
                "name": "memory_store",
                "description": "Store a fact envelope in MemKeys. Returns the fact id and resolved entity keys.",
                "inputSchema": {
                    "type": "object",
                    "required": ["tenant_id", "envelope"],
                    "properties": {
                        "tenant_id": {"type": "string", "description": "WS-4 tenant isolation id"},
                        "envelope": {"type": "object", "description": "MemKeys fact envelope (§3)"}
                    }
                }
            },
            {
                "name": "memory_fetch",
                "description": "Fetch facts by entity keys. At least one entity.* key is required.",
                "inputSchema": {
                    "type": "object",
                    "required": ["tenant_id"],
                    "properties": {
                        "tenant_id": {"type": "string"},
                        "entity.project": {"type": "string"},
                        "entity.issue": {"type": "string"},
                        "entity.pr": {"type": "string"},
                        "entity.agent": {"type": "string"},
                        "entity.run": {"type": "string"},
                        "entity.thread": {"type": "string"},
                        "entity.service": {"type": "string"},
                        "type": {"type": "string", "enum": ["decision","outcome","observation","learning","context"]},
                        "as_of": {"type": "string", "description": "RFC3339 point-in-time replay (§7.2)"},
                        "limit": {"type": "integer", "default": 100},
                        "valid": {"type": "string", "enum": ["current","all"], "default": "current"}
                    }
                }
            },
            {
                "name": "memory_traverse",
                "description": "Traverse the memory graph from a start node (bounded BFS, §7.3).",
                "inputSchema": {
                    "type": "object",
                    "required": ["tenant_id", "start"],
                    "properties": {
                        "tenant_id": {"type": "string"},
                        "start": {
                            "type": "object",
                            "required": ["kind", "id"],
                            "properties": {
                                "kind": {"type": "string"},
                                "id": {"type": "string"}
                            }
                        },
                        "edges": {"type": "array", "items": {"type": "string"}},
                        "direction": {"type": "string", "enum": ["out","in","both"], "default": "out"},
                        "depth": {"type": "integer", "default": 2},
                        "hydrate": {"type": "boolean", "default": false},
                        "deadline_ms": {"type": "integer", "default": 500}
                    }
                }
            },
            {
                "name": "memory_search",
                "description": "Semantic search (not available in M3 — returns feature_disabled).",
                "inputSchema": {
                    "type": "object",
                    "required": ["tenant_id", "query"],
                    "properties": {
                        "tenant_id": {"type": "string"},
                        "query": {"type": "string"}
                    }
                }
            }
        ]
    })
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn dispatch_tool(service: &MemKeysService, name: &str, args: Value) -> Result<Value, String> {
    match name {
        "memory_store" => tool_store(service, args).await,
        "memory_fetch" => tool_fetch(service, args).await,
        "memory_traverse" => tool_traverse(service, args).await,
        // M3 scope: semantic search is not available. Return `Err` so the
        // `tools/call` reply carries `isError: true`, matching the HTTP 501
        // (ISI-6134 M2) — an Ok body with an inline `error` field reads as a
        // successful call to an MCP client.
        "memory_search" => Err("feature_disabled: semantic_search is not available in M3 \
             (parity with HTTP 501 on POST /memory/search)"
            .to_string()),
        other => Err(format!("unknown tool: {other}")),
    }
}

async fn tool_store(service: &MemKeysService, args: Value) -> Result<Value, String> {
    let tenant_id = args["tenant_id"]
        .as_str()
        .ok_or("tenant_id is required")?
        .to_string();
    let envelope: Envelope = serde_json::from_value(args["envelope"].clone())
        .map_err(|e| format!("invalid envelope: {e}"))?;

    let caller = CallerContext {
        tenant_id: tenant_id.clone(),
        principal: "mcp".to_string(),
        trace_id: None,
    };
    let now = chrono::Utc::now();
    match service.ingest(&envelope, &caller, now).await {
        Ok(r) => Ok(serde_json::json!({
            "fact_id": r.fact_id,
            "keyed": r.keyed,
            "entity_keys_resolved": r.entity_keys_resolved
                .into_iter()
                .collect::<HashMap<_, _>>(),
            "duplicate": r.duplicate,
        })),
        Err(IngestError::Rejected { reason }) => Err(format!("rejected: {reason}")),
        Err(IngestError::Conflict { existing_id }) => {
            Err(format!("conflict with existing fact {existing_id}"))
        }
        Err(e) => Err(format!("store error: {e}")),
    }
}

async fn tool_fetch(service: &MemKeysService, args: Value) -> Result<Value, String> {
    let tenant_id = args["tenant_id"]
        .as_str()
        .ok_or("tenant_id is required")?
        .to_string();

    const KEY_MAP: &[(&str, EntityKey)] = &[
        ("entity.project", EntityKey::Project),
        ("entity.issue", EntityKey::Issue),
        ("entity.pr", EntityKey::Pr),
        ("entity.agent", EntityKey::Agent),
        ("entity.run", EntityKey::Run),
        ("entity.thread", EntityKey::Thread),
        ("entity.service", EntityKey::Service),
    ];

    let keys: Vec<(EntityKey, String)> = KEY_MAP
        .iter()
        .filter_map(|(name, ek)| args[name].as_str().map(|v| (*ek, v.to_string())))
        .collect();

    // §7.2 documents `type`; accept `fact_type` as a legacy alias (ISI-6134 B1).
    let fact_type = match args["type"].as_str().or_else(|| args["fact_type"].as_str()) {
        Some(ft) => Some(
            serde_json::from_value(Value::String(ft.to_string()))
                .map_err(|e| format!("invalid type: {e}"))?,
        ),
        None => None,
    };

    // §7.2 `as_of` point-in-time replay (S1); absent = live head.
    let as_of = match args["as_of"].as_str() {
        Some(ts) => Some(
            chrono::DateTime::parse_from_rfc3339(ts)
                .map_err(|_| format!("as_of must be RFC3339: {ts}"))?
                .with_timezone(&chrono::Utc),
        ),
        None => None,
    };

    let limit = args["limit"].as_i64().unwrap_or(KeyedQuery::DEFAULT_LIMIT);
    let valid = match args["valid"].as_str().unwrap_or("current") {
        "all" => Validity::All,
        _ => Validity::Current,
    };

    let query = KeyedQuery {
        keys,
        fact_type,
        as_of,
        valid,
        limit,
    };

    match service.fetch_by_keys(&tenant_id, &query).await {
        // §7.2/§7.5 response-shape invariant: `{ facts: [ … ] }`, not a bare
        // array — parity with the HTTP surface (ISI-6134 B3).
        Ok(records) => Ok(serde_json::json!({
            "facts": serde_json::to_value(records).unwrap_or(Value::Array(vec![])),
        })),
        Err(QueryError::Invalid { reason }) => Err(format!("invalid query: {reason}")),
        Err(e) => Err(format!("store error: {e}")),
    }
}

async fn tool_traverse(service: &MemKeysService, args: Value) -> Result<Value, String> {
    let tenant_id = args["tenant_id"]
        .as_str()
        .ok_or("tenant_id is required")?
        .to_string();

    let start: NodeRef =
        serde_json::from_value(args["start"].clone()).map_err(|e| format!("invalid start: {e}"))?;

    let edges: Vec<EdgeType> = if let Some(arr) = args["edges"].as_array() {
        arr.iter()
            .filter_map(|v| v.as_str().and_then(EdgeType::parse))
            .collect()
    } else {
        vec![]
    };

    // §7.3 documents `direction`; accept `dir` as a legacy alias (ISI-6134 B2).
    let dir: Direction =
        if let Some(s) = args["direction"].as_str().or_else(|| args["dir"].as_str()) {
            serde_json::from_value(Value::String(s.to_string()))
                .map_err(|e| format!("invalid direction: {e}"))?
        } else {
            Direction::default()
        };

    let depth = args["depth"].as_u64().map(|d| d as u8).unwrap_or(2);
    let hydrate = args["hydrate"].as_bool().unwrap_or(false);
    let deadline_ms = args["deadline_ms"]
        .as_u64()
        .unwrap_or(DEFAULT_DEADLINE.as_millis() as u64);

    let req = TraverseReq {
        tenant: tenant_id.clone(),
        project: None,
        start,
        edges,
        dir,
        depth,
        deadline: Duration::from_millis(deadline_ms),
        hydrate,
        fanout_cap: DEFAULT_FANOUT_CAP,
        node_budget: DEFAULT_NODE_BUDGET,
    };

    match service.traverse(&tenant_id, &req).await {
        Ok(result) => Ok(serde_json::to_value(result).unwrap()),
        Err(GraphError::NotFound) => Err("not found".to_string()),
        Err(GraphError::Invalid(msg)) => Err(format!("invalid request: {msg}")),
        Err(e) => Err(format!("store error: {e}")),
    }
}

// ---------------------------------------------------------------------------
// MCP stdio loop
// ---------------------------------------------------------------------------

/// Run the MCP stdio server until stdin closes. Blocks; call from a dedicated
/// `tokio::task::spawn_blocking` or a separate tokio task using
/// `BufReader<tokio::io::Stdin>`.
pub async fn run_stdio(service: Arc<MemKeysService>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(e) => {
                tracing::error!(target: "memkeys", "mcp stdin read error: {e}");
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // JSON-RPC 2.0: notifications (no `id`) get no reply at all — `handle_line`
        // returns `None` for them, and we write nothing (ISI-6134 B4).
        let Some(response) = handle_line(&service, trimmed).await else {
            continue;
        };
        let mut out = serde_json::to_string(&response).unwrap_or_else(|_| {
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"serialize error"}}"#
                .to_string()
        });
        out.push('\n');
        if let Err(e) = stdout.write_all(out.as_bytes()).await {
            tracing::error!(target: "memkeys", "mcp stdout write error: {e}");
            break;
        }
        let _ = stdout.flush().await;
    }
}

async fn handle_line(service: &MemKeysService, line: &str) -> Option<JsonRpcResponse> {
    let req: JsonRpcRequest = match serde_json::from_str(line) {
        Ok(r) => r,
        // A malformed line can't be told apart from a notification, so JSON-RPC
        // 2.0 requires a parse-error reply with null id.
        Err(_) => {
            return Some(JsonRpcResponse::err(Value::Null, -32700, "parse error"));
        }
    };

    // JSON-RPC 2.0: a request with no `id` is a notification — the server MUST
    // NOT reply, not even with method-not-found (ISI-6134 B4).
    let id = match req.id.clone() {
        Some(id) => id,
        None => return None,
    };

    Some(match req.method.as_str() {
        "initialize" => JsonRpcResponse::ok(
            id,
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "memkeys",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "tools": {}
                }
            }),
        ),

        "tools/list" => JsonRpcResponse::ok(id, tool_list()),

        "tools/call" => {
            let tool_name = match req.params["name"].as_str() {
                Some(n) => n.to_string(),
                None => {
                    return Some(JsonRpcResponse::err(id, -32602, "missing tool name"));
                }
            };
            let arguments = req.params["arguments"].clone();

            // ISI-6134 B2: one SERVER span per `tools/call` carrying the
            // transport surface (stdio) + tool name. Attaching its context
            // makes the service's ingest/retrieve spans nest beneath it (they
            // parent to the current context on their first synchronous poll).
            let span = telemetry::transport_span(
                "memkeys.mcp.tool_call",
                TRANSPORT_STDIO,
                Some(&tool_name),
            );
            // `with_context` attaches `cx` for each poll of the dispatch future
            // WITHOUT holding a thread-local `ContextGuard` across `.await` —
            // the guard is `!Send` and would make `run_stdio`'s spawned future
            // `!Send` (telemetry.rs §doc). The service's ingest/retrieve spans
            // still nest beneath `cx` because they read the active context on
            // their first synchronous poll.
            let cx = Context::current_with_span(span);
            let outcome = dispatch_tool(service, &tool_name, arguments)
                .with_context(cx.clone())
                .await;
            cx.span().set_status(match &outcome {
                Ok(_) => Status::Ok,
                Err(_) => Status::Error {
                    description: "tool call failed".into(),
                },
            });
            cx.span().end();

            match outcome {
                Ok(result) => JsonRpcResponse::ok(
                    id,
                    serde_json::json!({
                        "content": [{"type": "text", "text": result.to_string()}]
                    }),
                ),
                Err(msg) => JsonRpcResponse::ok(
                    id,
                    serde_json::json!({
                        "content": [{"type": "text", "text": msg}],
                        "isError": true
                    }),
                ),
            }
        }

        _ => JsonRpcResponse::err(id, -32601, format!("method not found: {}", req.method)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memstore::MemStore;

    fn service() -> Arc<MemKeysService> {
        Arc::new(MemKeysService::new(Arc::new(MemStore::new())))
    }

    #[tokio::test]
    async fn notification_gets_no_reply() {
        let svc = service();
        // No `id` → JSON-RPC notification → server MUST NOT reply (ISI-6134 B4),
        // not even with method-not-found.
        let notif = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(handle_line(&svc, notif).await.is_none());
    }

    #[tokio::test]
    async fn request_with_id_gets_a_reply() {
        let svc = service();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let resp = handle_line(&svc, req)
            .await
            .expect("request must get a reply");
        assert_eq!(resp.id, serde_json::json!(1));
        assert!(resp.result.is_some());
    }

    #[tokio::test]
    async fn parse_error_still_replies_with_null_id() {
        let svc = service();
        // A malformed line is indistinguishable from a notification, so JSON-RPC
        // requires a parse-error reply with null id.
        let resp = handle_line(&svc, "not json")
            .await
            .expect("parse error replies");
        assert_eq!(resp.id, Value::Null);
        assert!(resp.error.is_some());
    }
}
