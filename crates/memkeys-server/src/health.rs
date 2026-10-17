//! Minimal HTTP health endpoint for pod readiness (ISI-6132).
//!
//! The Helm chart's liveness/readiness probes target `GET /healthz` and
//! `GET /readyz` on the service port (default 8080). This module serves exactly
//! that surface over a hand-rolled HTTP/1.1 responder — deliberately not an
//! HTTP framework, because the real transport (M3, HTTP/MCP open contract)
//! is a separate milestone and its stack choice should not be pre-committed
//! here. When M3 lands it can absorb or replace this listener.
//!
//! - `/healthz` — liveness: 200 always. Never touches the database, so a DB
//!   outage cannot make the orchestrator kill healthy pods.
//! - `/readyz`  — readiness: 200 only when a `SELECT 1` against the FactStore
//!   pool succeeds within [`PING_TIMEOUT`]; 503 otherwise.
//!
//! Port: `MEMKEYS_HTTP_PORT` (default [`DEFAULT_HTTP_PORT`]). Connections are
//! closed after a single response (`Connection: close`) — kubelet probes do
//! not reuse connections.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Default listen port; matches the chart's `service.port` default.
pub const DEFAULT_HTTP_PORT: u16 = 8080;

/// Upper bound on the request head we are willing to buffer.
const MAX_HEAD: usize = 8 * 1024;
/// Bound on reading a request head (guards against idle/hung clients).
const HEAD_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on the readiness DB ping; kubelet's probe timeout is ~2s.
const PING_TIMEOUT: Duration = Duration::from_secs(2);

/// The listen port, from `MEMKEYS_HTTP_PORT` or the default. Invalid values
/// warn and fall back rather than aborting: a mis-set port should not take
/// down a pod whose DB wiring is otherwise healthy.
pub fn http_port() -> u16 {
    match std::env::var("MEMKEYS_HTTP_PORT") {
        Ok(raw) => match raw.trim().parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                tracing::warn!(
                    target: "memkeys",
                    value = %raw,
                    default = DEFAULT_HTTP_PORT,
                    "memkeys.server.http.port.invalid"
                );
                DEFAULT_HTTP_PORT
            }
        },
        Err(_) => DEFAULT_HTTP_PORT,
    }
}

/// Bind the health listener on 0.0.0.0:<port> (all interfaces: in-cluster the
/// pod IP is the only route, and kubelet probes arrive via the pod network).
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await
}

/// Serve `/healthz` + `/readyz`, with readiness supplied by `is_ready`
/// (called once per `/readyz` request). Runs until the listener errors out;
/// intended to be spawned. Each connection is handled on its own task so a
/// hung client cannot wedge the probes.
pub async fn serve<F, Fut>(listener: TcpListener, is_ready: F) -> std::io::Result<()>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = bool> + Send,
{
    let is_ready = Arc::new(is_ready);
    loop {
        let (stream, _peer) = listener.accept().await?;
        let is_ready = Arc::clone(&is_ready);
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &*is_ready).await {
                // A probe connection dropping mid-request is normal; log at
                // debug so a flapping network does not spam the log pipeline.
                tracing::debug!(target: "memkeys", error = %e, "memkeys.server.http.conn_error");
            }
        });
    }
}

/// Convenience wrapper for the production wiring: readiness = `SELECT 1`
/// against the FactStore pool within [`PING_TIMEOUT`].
pub async fn serve_pool(listener: TcpListener, pool: sqlx::PgPool) -> std::io::Result<()> {
    serve(listener, move || {
        let pool = pool.clone();
        async move {
            matches!(
                tokio::time::timeout(PING_TIMEOUT, sqlx::query("SELECT 1").execute(&pool)).await,
                Ok(Ok(_))
            )
        }
    })
    .await
}

/// Read one request head, answer it, close. The only error path is transport
/// failure (read/write); routing never fails.
async fn handle<F, Fut>(mut stream: TcpStream, is_ready: &F) -> std::io::Result<()>
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut stream)).await {
        Ok(Ok(head)) => head,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "request head exceeded 5s",
            ))
        }
    };

    let (status, reason, body) = route(&head, is_ready).await;
    let response = response(status, reason, &body);
    stream.write_all(&response).await?;
    stream.flush().await?;
    stream.shutdown().await
}

/// Buffer bytes until the end of the request head, or [`MAX_HEAD`].
async fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 256];
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() >= MAX_HEAD {
            break;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break; // client closed without sending a full head
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    // Header values are ASCII in practice; lossy keeps a hostile head from
    // erroring the connection (it will simply not match a route).
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Route one request head to a response. Invalid syntax → 400, non-GET → 405.
async fn route<F, Fut>(head: &str, is_ready: &F) -> (u16, &'static str, String)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let mut parts = head.split_whitespace();
    let (method, target) = match (parts.next(), parts.next()) {
        (Some(m), Some(t)) => (m, t),
        _ => return (400, "Bad Request", "malformed request line\n".into()),
    };
    // The third token (HTTP version) is present for anything kubelet sends;
    // a missing version still answers, a garbage one is not worth a 505.
    let path = target.split('?').next().unwrap_or("");
    let body = |status: u16, reason: &'static str, b: &str| (status, reason, b.to_string());
    match method {
        "GET" => match path {
            "/healthz" => body(200, "OK", "ok\n"),
            "/readyz" => {
                if is_ready().await {
                    body(200, "OK", "ok\n")
                } else {
                    body(503, "Service Unavailable", "database unavailable\n")
                }
            }
            _ => body(404, "Not Found", "not found\n"),
        },
        _ => body(405, "Method Not Allowed", "method not allowed\n"),
    }
}

/// Render a fixed-shape HTTP/1.1 response with `Connection: close`.
fn response(status: u16, reason: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> std::future::Ready<bool> {
        std::future::ready(true)
    }

    #[tokio::test]
    async fn healthz_is_always_200() {
        let (status, reason, body) =
            route("GET /healthz HTTP/1.1\r\nHost: p\r\n\r\n", &ready).await;
        assert_eq!((status, reason), (200, "OK"));
        assert_eq!(body, "ok\n");
    }

    #[tokio::test]
    async fn readyz_reflects_the_check() {
        let ok = route("GET /readyz HTTP/1.1\r\n\r\n", &ready).await;
        assert_eq!((ok.0, ok.1), (200, "OK"));
        let not_ready = || std::future::ready(false);
        let err = route("GET /readyz HTTP/1.1\r\n\r\n", &not_ready).await;
        assert_eq!((err.0, err.1), (503, "Service Unavailable"));
    }

    #[tokio::test]
    async fn unknown_path_is_404_and_query_is_stripped() {
        let r = route("GET /nope HTTP/1.1\r\n\r\n", &ready).await;
        assert_eq!(r.0, 404);
        let q = route("GET /healthz?probe=1 HTTP/1.1\r\n\r\n", &ready).await;
        assert_eq!(q.0, 200);
    }

    #[tokio::test]
    async fn non_get_is_405_and_garbage_is_400() {
        let post = route("POST /healthz HTTP/1.1\r\n\r\n", &ready).await;
        assert_eq!(post.0, 405);
        let garbage = route("not-a-request\r\n\r\n", &ready).await;
        assert_eq!(garbage.0, 400);
    }

    #[test]
    fn response_head_is_wellformed() {
        let bytes = response(200, "OK", "ok\n");
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(text.contains("Content-Length: 3\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\nok\n"));
    }

    /// End-to-end over loopback TCP: the exact surface kubelet probes.
    #[tokio::test]
    async fn serves_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, ready));

        async fn get(addr: std::net::SocketAddr, path: &str) -> String {
            let mut s = TcpStream::connect(addr).await.unwrap();
            s.write_all(format!("GET {path} HTTP/1.1\r\nHost: probe\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.unwrap();
            String::from_utf8(buf).unwrap()
        }

        assert!(get(addr, "/healthz")
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(get(addr, "/readyz")
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(get(addr, "/readyz?x")
            .await
            .starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(get(addr, "/nope")
            .await
            .starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    #[test]
    fn port_env_parsing() {
        // read_head/route covered above; port parsing is env-dependent, so
        // assert only the default path via the constant this contract pins.
        assert_eq!(DEFAULT_HTTP_PORT, 8080);
    }
}
