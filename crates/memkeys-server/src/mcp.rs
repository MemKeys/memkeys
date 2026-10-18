//! Minimal stdio JSON-RPC 2.0 MCP server (M3, WS-5).
//!
//! Implements the four MemKeys tools over the Model Context Protocol:
//!   memory_store    → service.ingest()
//!   memory_fetch    → service.fetch_by_keys()
//!   memory_traverse → service.traverse()
//!   memory_search   → 501 graceful (semantic search not in M3)
//!
//! The tenant is NOT supplied by the caller (ISI-6148, closes ISI-6134 S1).
//! A single [`Principal`] is bound once at launch from the run token (WS-5
//! §2.1 — the stdio pipe is the trust boundary); every tool call inherits its
//! verified tenant floor (WS-4 Invariant T), and the expiry window is
//! re-checked on each call (F12) so a long-lived process hard-fails
//! `unauthenticated` once the window closes. Admission gates
//! (concurrency shedding, F14 write-size, F15 boundary-probe) wrap every call.
//!
//! Protocol: JSON-RPC 2.0 over stdin/stdout, one request per line, one
//! response per line (newline-delimited). Implements `initialize`,
//! `tools/list`, and `tools/call`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::admission::AdmissionControl;
use crate::auth::{Principal, TokenVerifier};
use crate::envelope::Envelope;
use crate::error::{IngestError, QueryError};
use crate::graph::{
    Direction, EdgeType, GraphError, NodeRef, TraverseReq, DEFAULT_DEADLINE, DEFAULT_FANOUT_CAP,
    DEFAULT_NODE_BUDGET,
};
use crate::model::{EntityKey, KeyedQuery, Validity};
use crate::service::MemKeysService;
use crate::telemetry::{self, TRANSPORT_STDIO};
use crate::wire::{ErrorCode, WireError};
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
                "description": "Store a fact envelope in MemKeys. Returns the fact id and resolved entity keys. The tenant floor is derived from the launch-bound principal (§2.1) — never a tool argument.",
                "inputSchema": {
                    "type": "object",
                    "required": ["envelope"],
                    "properties": {
                        "envelope": {"type": "object", "description": "MemKeys fact envelope (§3)"}
                    }
                }
            },
            {
                "name": "memory_fetch",
                "description": "Fetch facts by entity keys. At least one entity.* key is required. The tenant floor is the launch-bound principal's (§2.1).",
                "inputSchema": {
                    "type": "object",
                    "required": [],
                    "properties": {
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
                "description": "Traverse the memory graph from a start node (bounded BFS, §7.3). The tenant floor is the launch-bound principal's (§2.1).",
                "inputSchema": {
                    "type": "object",
                    "required": ["start"],
                    "properties": {
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
                    "required": ["query"],
                    "properties": {
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

async fn dispatch_tool(
    service: &MemKeysService,
    principal: &Principal,
    admission: &AdmissionControl,
    name: &str,
    args: Value,
) -> Result<Value, WireError> {
    match name {
        "memory_store" => tool_store(service, principal, admission, args).await,
        "memory_fetch" => tool_fetch(service, principal, args).await,
        "memory_traverse" => tool_traverse(service, principal, admission, args).await,
        // M3 scope: semantic search is not available. Return `Err` so the
        // `tools/call` reply carries `isError: true`, matching the HTTP 501
        // (ISI-6134 M2) — an Ok body with an inline `error` field reads as a
        // successful call to an MCP client.
        "memory_search" => Err(WireError::new(
            ErrorCode::FeatureDisabled,
            "semantic_search is not available in M3 (parity with HTTP 501 on POST /memory/search)",
        )),
        other => Err(WireError::new(
            ErrorCode::InvalidArgument,
            format!("unknown tool: {other}"),
        )),
    }
}

async fn tool_store(
    service: &MemKeysService,
    principal: &Principal,
    admission: &AdmissionControl,
    args: Value,
) -> Result<Value, WireError> {
    let envelope: Envelope = serde_json::from_value(args["envelope"].clone()).map_err(|e| {
        WireError::new(ErrorCode::InvalidEnvelope, format!("invalid envelope: {e}"))
    })?;

    // F14 write-size cap BEFORE the pipeline.
    let body_bytes = serde_json::to_vec(&envelope.fact.body)
        .map(|v| v.len())
        .unwrap_or(0);
    admission.check_write_size(body_bytes, envelope.attachments.len())?;

    // The caller context is the VERIFIED principal's tenant/agent (§4 rule 1) —
    // never a tool argument.
    let caller = principal.caller_context();
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
        Err(IngestError::Rejected { reason }) => Err(WireError::new(
            ErrorCode::InvalidEnvelope,
            format!("rejected: {reason}"),
        )),
        Err(IngestError::Conflict { existing_id }) => Err(WireError::new(
            ErrorCode::Conflict,
            format!("conflict with existing fact {existing_id}"),
        )),
        Err(e) => Err(WireError::new(
            ErrorCode::BackendUnavailable,
            format!("store error: {e}"),
        )),
    }
}

async fn tool_fetch(
    service: &MemKeysService,
    principal: &Principal,
    args: Value,
) -> Result<Value, WireError> {
    let tenant_id = principal.tenant_id.clone();

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
            serde_json::from_value(Value::String(ft.to_string())).map_err(|e| {
                WireError::new(ErrorCode::InvalidArgument, format!("invalid type: {e}"))
            })?,
        ),
        None => None,
    };

    // §7.2 `as_of` point-in-time replay (S1); absent = live head.
    let as_of = match args["as_of"].as_str() {
        Some(ts) => Some(
            chrono::DateTime::parse_from_rfc3339(ts)
                .map_err(|_| {
                    WireError::new(
                        ErrorCode::InvalidArgument,
                        format!("as_of must be RFC3339: {ts}"),
                    )
                })?
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
        Err(QueryError::Invalid { reason }) => Err(WireError::new(
            ErrorCode::InvalidArgument,
            format!("invalid query: {reason}"),
        )),
        Err(e) => Err(WireError::new(
            ErrorCode::BackendUnavailable,
            format!("store error: {e}"),
        )),
    }
}

async fn tool_traverse(
    service: &MemKeysService,
    principal: &Principal,
    admission: &AdmissionControl,
    args: Value,
) -> Result<Value, WireError> {
    let tenant_id = principal.tenant_id.clone();

    let start: NodeRef = serde_json::from_value(args["start"].clone())
        .map_err(|e| WireError::new(ErrorCode::InvalidArgument, format!("invalid start: {e}")))?;

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
            serde_json::from_value(Value::String(s.to_string())).map_err(|e| {
                WireError::new(
                    ErrorCode::InvalidArgument,
                    format!("invalid direction: {e}"),
                )
            })?
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
        // F15: a boundary-crossing lookup is `not_found` (§3.3); record the miss
        // so a principal over its probe budget is throttled `rate_limited`
        // instead of getting an unlimited existence oracle.
        Err(GraphError::NotFound) => {
            let now = chrono::Utc::now().timestamp();
            match admission.record_boundary_miss(&tenant_id, now) {
                Ok(()) => Err(WireError::new(ErrorCode::NotFound, "not found")),
                Err(throttle) => Err(throttle),
            }
        }
        Err(GraphError::Invalid(msg)) => Err(WireError::new(
            ErrorCode::InvalidArgument,
            format!("invalid request: {msg}"),
        )),
        Err(e) => Err(WireError::new(
            ErrorCode::BackendUnavailable,
            format!("store error: {e}"),
        )),
    }
}

// ---------------------------------------------------------------------------
// MCP stdio loop
// ---------------------------------------------------------------------------

/// Run the MCP stdio server until stdin closes. Blocks; call from a dedicated
/// `tokio::task::spawn_blocking` or a separate tokio task using
/// `BufReader<tokio::io::Stdin>`.
///
/// `verifier` carries the launch-bound principal (§2.1): stdio binds ONE
/// [`StaticVerifier`] at launch, and every `tools/call` re-verifies it to
/// enforce the expiry window (F12). `admission` applies the shared
/// self-protection gates.
pub async fn run_stdio(
    service: Arc<MemKeysService>,
    verifier: Arc<dyn TokenVerifier>,
    admission: Arc<AdmissionControl>,
) {
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
        let Some(response) = handle_line(&service, &*verifier, &admission, trimmed).await else {
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

async fn handle_line(
    service: &MemKeysService,
    verifier: &dyn TokenVerifier,
    admission: &AdmissionControl,
    line: &str,
) -> Option<JsonRpcResponse> {
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

            // Gate 1 (concurrency shedding): take an in-flight permit; a
            // saturated server sheds `rate_limited` rather than piling up work.
            // The guard releases on scope exit.
            let _admit = match admission.try_admit() {
                Ok(g) => g,
                Err(e) => return Some(tool_error_reply(id, e)),
            };

            // Principal binding (§2.1): re-verify the launch-bound principal on
            // every call so the expiry window is enforced (F12). The token value
            // is irrelevant for stdio (the pipe is the trust boundary) — the
            // `StaticVerifier` ignores it and only checks the window.
            let now = chrono::Utc::now().timestamp();
            let principal = match verifier.verify("", now) {
                Ok(p) => p,
                Err(e) => return Some(tool_error_reply(id, e)),
            };

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
            let outcome = dispatch_tool(service, &principal, admission, &tool_name, arguments)
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
                Err(e) => tool_error_reply(id, e),
            }
        }

        _ => JsonRpcResponse::err(id, -32601, format!("method not found: {}", req.method)),
    })
}

/// Render a [`WireError`] as an MCP `tools/call` error result. Per the ISI-6134
/// M2 decision an errored tool call is an `Ok` JSON-RPC reply carrying
/// `isError: true`; the content text is the WS-5 §7 error body (JSON) so the
/// client reads a stable `code`/`retriable`, matching the HTTP surface.
fn tool_error_reply(id: Value, err: WireError) -> JsonRpcResponse {
    let text =
        serde_json::to_string(&err).unwrap_or_else(|_| format!("{{\"code\":\"{}\"}}", err.code));
    JsonRpcResponse::ok(
        id,
        serde_json::json!({
            "content": [{"type": "text", "text": text}],
            "isError": true
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::StaticVerifier;
    use crate::memstore::MemStore;

    fn service() -> Arc<MemKeysService> {
        Arc::new(MemKeysService::new(Arc::new(MemStore::new())))
    }

    /// A launch-bound principal for tenant `t` whose window closes at `exp`.
    fn verifier(tenant: &str, exp: i64) -> Arc<dyn TokenVerifier> {
        Arc::new(StaticVerifier::new(Principal {
            tenant_id: tenant.to_string(),
            agent_id: "stdio-agent".to_string(),
            run_id: Some("run-1".to_string()),
            expires_at: exp,
            trace_id: None,
        }))
    }

    fn admission() -> Arc<AdmissionControl> {
        Arc::new(AdmissionControl::with_defaults())
    }

    #[tokio::test]
    async fn notification_gets_no_reply() {
        let svc = service();
        // No `id` → JSON-RPC notification → server MUST NOT reply (ISI-6134 B4),
        // not even with method-not-found.
        let notif = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        assert!(
            handle_line(&svc, &*verifier("t", i64::MAX), &admission(), notif)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn request_with_id_gets_a_reply() {
        let svc = service();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let resp = handle_line(&svc, &*verifier("t", i64::MAX), &admission(), req)
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
        let resp = handle_line(&svc, &*verifier("t", i64::MAX), &admission(), "not json")
            .await
            .expect("parse error replies");
        assert_eq!(resp.id, Value::Null);
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn tool_call_past_expiry_window_is_unauthenticated() {
        let svc = service();
        // The launch window has already closed (exp=1000, now >> that): every
        // tool call hard-fails `unauthenticated` (F12) — no fetch runs.
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"memory_fetch","arguments":{"entity.issue":"I1"}}}"#;
        let resp = handle_line(&svc, &*verifier("t", 1000), &admission(), req)
            .await
            .expect("a request gets a reply");
        let result = resp.result.expect("errored tool call is an Ok reply");
        assert_eq!(result["isError"], serde_json::json!(true));
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("unauthenticated"),
            "expired window must surface `unauthenticated`, got: {text}"
        );
    }

    #[tokio::test]
    async fn fetch_uses_bound_tenant_not_a_client_arg() {
        let svc = service();
        // No `tenant_id` arg is supplied; the fetch runs against the bound
        // principal's tenant and returns an (empty) `{facts:[]}` success — proof
        // the tenant floor comes from the principal, not the call.
        let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_fetch","arguments":{"entity.issue":"I1"}}}"#;
        let resp = handle_line(&svc, &*verifier("tenant-a", i64::MAX), &admission(), req)
            .await
            .expect("a request gets a reply");
        let result = resp.result.expect("ok reply");
        assert!(result.get("isError").is_none(), "should succeed: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("facts"), "got: {text}");
    }
}
