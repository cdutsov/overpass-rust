//! HTTP server: `/api/interpreter`, `/api/status`, `/api/timestamp`.
//!
//! Queries run on blocking threads, each on its own LMDB read snapshot, and
//! stream their output. A semaphore bounds concurrent queries; requests that
//! cannot get a slot in time get 504 like the C++ dispatcher.

use crate::exec::{RunError, RunOptions, run_script};
use crate::output::{COPYRIGHT, escape_xml};
use crate::ql::ast::OutFormat;
use crate::store::Db;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, mpsc};

pub struct ServerConfig {
    pub max_concurrent: usize,
    /// Upper bound for `[timeout:]`.
    pub max_timeout: Option<u32>,
    /// Upper bound for `[maxsize:]`.
    pub max_maxsize: Option<u64>,
    /// Upper bound for the sum of `[maxsize:]` over the running queries
    /// (the `--space` of the C++ dispatcher).
    pub max_space: Option<u64>,
    pub queue_timeout: Duration,
    pub max_query_len: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            max_concurrent: 2 * std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            max_timeout: Some(1800),
            max_maxsize: Some(4 << 30),
            max_space: None,
            queue_timeout: Duration::from_secs(15),
            max_query_len: 1 << 20,
        }
    }
}

/// Units of the space semaphore: MiB of `[maxsize:]`.
const SPACE_UNIT: u64 = 1 << 20;

struct AppState {
    db: Arc<Db>,
    slots: Arc<Semaphore>,
    /// Room for `[maxsize:]` of running queries, in `SPACE_UNIT`s.
    space: Option<(Arc<Semaphore>, u32)>,
    cfg: ServerConfig,
    started: Instant,
    served: AtomicU64,
}

pub fn router(db: Arc<Db>, cfg: ServerConfig) -> Router {
    let space = cfg.max_space.map(|b| {
        let units = (b / SPACE_UNIT).clamp(1, u32::MAX as u64) as u32;
        (Arc::new(Semaphore::new(units as usize)), units)
    });
    let state = Arc::new(AppState { db, slots: Arc::new(Semaphore::new(cfg.max_concurrent)), space, cfg, started: Instant::now(), served: AtomicU64::new(0) });
    // Bodies are checked against max_query_len ourselves to return the C++ error page.
    let body_limit = 4 * state.cfg.max_query_len + 4096;
    Router::new()
        .route("/api/interpreter", get(interpreter).post(interpreter).head(interpreter).options(interpreter))
        .layer(axum::extract::DefaultBodyLimit::max(body_limit))
        .route("/api/status", get(status))
        .route("/api/timestamp", get(timestamp))
        .with_state(state)
}

pub async fn serve(db: Arc<Db>, addr: &str, cfg: ServerConfig) -> anyhow::Result<()> {
    let app = router(db, cfg);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("listening on http://{}", listener.local_addr()?);
    // The response head and the next small write go out separately. With Nagle the second
    // waits for the ACK of the first, which a client on a reused connection delays by 40 ms.
    let listener = axum::serve::ListenerExt::tap_io(listener, |tcp: &mut tokio::net::TcpStream| {
        let _ = tcp.set_nodelay(true);
    });
    // On SIGTERM (docker stop) or Ctrl-C: accept no new connections and let
    // running queries finish (within the stop grace period of the container).
    let shutdown = async {
        let term = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term => {}
        }
        eprintln!("shutting down: waiting for running queries");
    };
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

fn cors(headers: &HeaderMap, resp: &mut Response) {
    let h = resp.headers_mut();
    if let Some(v) = headers.get("access-control-request-headers") {
        if !v.is_empty() {
            h.insert("access-control-allow-headers", v.clone());
        }
    }
    if headers.get(header::ORIGIN).is_some_and(|v| !v.is_empty()) {
        h.insert("access-control-allow-origin", HeaderValue::from_static("*"));
        h.insert("access-control-max-age", HeaderValue::from_static("600"));
    }
}

/// The C++ XHTML error page.
fn error_page(status: StatusCode, messages: &[String]) -> Response {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\"\n    \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd\">\n<html xmlns=\"http://www.w3.org/1999/xhtml\" xml:lang=\"en\" lang=\"en\">\n<head>\n  <meta http-equiv=\"content-type\" content=\"text/html; charset=utf-8\" lang=\"en\"/>\n  <title>OSM3S Response</title>\n</head>\n<body>\n\n",
    );
    body.push_str(&format!("<p>{COPYRIGHT}</p>\n"));
    for m in messages {
        let mut e = Vec::new();
        escape_xml(m, &mut e);
        body.push_str(&format!("<p><strong style=\"color:#FF0000\">Error</strong>: {} </p>\n", String::from_utf8_lossy(&e)));
    }
    body.push_str("\n</body>\n</html>\n");
    (status, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

/// Writer that forwards output chunks to the HTTP body stream. A client that
/// stops reading cannot block the query thread past its deadline.
struct ChannelWriter {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    buf: Vec<u8>,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    rt: tokio::runtime::Handle,
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        if self.buf.len() >= 1 << 16 {
            self.flush()?;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = Ok(Bytes::from(std::mem::take(&mut self.buf)));
        let wait = self.deadline.saturating_duration_since(Instant::now());
        // Blocking threads may wait on the runtime; a client that stops
        // reading releases the thread at the deadline.
        let sent = self.rt.block_on(tokio::time::timeout(wait, self.tx.send(chunk)));
        if matches!(sent, Ok(Ok(()))) {
            return Ok(());
        }
        self.cancel.store(true, Ordering::Relaxed);
        Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client went away"))
    }
}

/// Replace `(bbox)` / `[bbox]` with the `bbox=w,s,e,n` request parameter.
fn substitute_bbox(q: &str, bbox: Option<&String>) -> String {
    let Some(b) = bbox else { return q.to_string() };
    let parts: Vec<&str> = b.split(',').collect();
    if parts.len() != 4 {
        return q.to_string();
    }
    let (w, s, e, n) = (parts[0], parts[1], parts[2], parts[3]);
    q.replace("(bbox)", &format!("({s},{w},{n},{e})")).replace("[bbox]", &format!("[bbox:{s},{w},{n},{e}]"))
}

/// Request parameters like the C++ CGI: the query string wins; otherwise a
/// form-encoded body, or the raw body when it has no `data` field.
fn request_params(query_string: Option<&str>, body: &[u8]) -> HashMap<String, String> {
    if let Some(q) = query_string.filter(|q| !q.is_empty()) {
        return form_urlencoded::parse(q.as_bytes()).into_owned().collect();
    }
    let mut params: HashMap<String, String> = form_urlencoded::parse(body).into_owned().collect();
    if params.get("data").is_none_or(|d| d.is_empty()) && !body.is_empty() {
        params.insert("data".into(), String::from_utf8_lossy(body).into_owned());
    }
    params
}

async fn interpreter(State(st): State<Arc<AppState>>, method: Method, headers: HeaderMap, uri: axum::http::Uri, body: Bytes) -> Response {
    let params = request_params(uri.query(), &body);
    let mut query = params.get("data").cloned().unwrap_or_default();
    if method == Method::OPTIONS && query.is_empty() {
        query = "out;".into();
    }
    // The C++ CGI layer hands an absent query on as a line break.
    if query.is_empty() {
        query.push('\n');
    }
    query = substitute_bbox(&query, params.get("bbox"));
    let mut resp = run_request(st, method.clone(), query, params.get("jsonp").cloned()).await;
    cors(&headers, &mut resp);
    if method == Method::OPTIONS {
        resp.headers_mut().insert("access-control-allow-methods", HeaderValue::from_static("GET, POST, OPTIONS"));
    }
    resp
}

fn busy_page() -> Response {
    error_page(
        StatusCode::GATEWAY_TIMEOUT,
        &["runtime error: open64: 0 Success /osm3s_osm_base Dispatcher_Client::request_read_and_idx::timeout. The server is probably too busy to handle your request.".into()],
    )
}

async fn run_request(st: Arc<AppState>, method: Method, query: String, jsonp: Option<String>) -> Response {
    if query.len() > st.cfg.max_query_len {
        return error_page(StatusCode::BAD_REQUEST, &[format!("encoding error: Input Input too long (length: {}). The maximum query length is {} MB.", query.len(), st.cfg.max_query_len >> 20)]);
    }
    if let Some(j) = &jsonp {
        if !j.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            return error_page(StatusCode::BAD_REQUEST, &["encoding error: Parameter \"jsonp\" must contain only letters, digits, or the underscore.".into()]);
        }
    }
    // Parsing and execution both run on blocking threads behind a slot, so
    // hostile queries cannot stall the async workers.
    let permit = match tokio::time::timeout(st.cfg.queue_timeout, st.slots.clone().acquire_owned()).await {
        Ok(Ok(p)) => p,
        _ => return busy_page(),
    };
    let head_only = method == Method::HEAD || method == Method::OPTIONS;
    let queue_deadline = tokio::time::Instant::now() + st.cfg.queue_timeout;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<Ready>();
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    let cancel = Arc::new(AtomicBool::new(false));
    let db = st.db.clone();
    let opts = RunOptions { max_timeout: st.cfg.max_timeout, max_maxsize: st.cfg.max_maxsize, cancel: Some(cancel.clone()), cli: false };
    let max_timeout = st.cfg.max_timeout;
    let rt = tokio::runtime::Handle::current();
    let st2 = st.clone();
    let cancel2 = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let script = match crate::ql::parse(&query) {
            Ok(s) => s,
            Err(e) => {
                let _ = ready_tx.send(Ready::Error(e.to_string()));
                return;
            }
        };
        let content_type = match script.settings.out {
            OutFormat::Xml => "application/osm3s+xml",
            OutFormat::Json => "application/json",
            OutFormat::Csv(_) => "text/csv",
        };
        // Room for the query's maxsize, as the C++ dispatcher reserves it.
        let _space = match (&st2.space, head_only) {
            (Some((sem, total)), false) => {
                let maxsize = st2.cfg.max_maxsize.map_or(script.settings.maxsize, |m| script.settings.maxsize.min(m));
                let units = maxsize.div_ceil(SPACE_UNIT).clamp(1, *total as u64) as u32;
                match rt.block_on(tokio::time::timeout_at(queue_deadline, sem.clone().acquire_many_owned(units))) {
                    Ok(Ok(p)) => Some(p),
                    _ => {
                        let _ = ready_tx.send(Ready::Busy);
                        return;
                    }
                }
            }
            _ => None,
        };
        if ready_tx.send(Ready::Ok(content_type)).is_err() || head_only {
            return;
        }
        st2.served.fetch_add(1, Ordering::Relaxed);
        let timeout = max_timeout.unwrap_or(u32::MAX).min(script.settings.timeout);
        let mut w = ChannelWriter { tx, buf: Vec::new(), cancel: cancel2, deadline: Instant::now() + Duration::from_secs(timeout as u64 + 30), rt };
        let jsonp = jsonp.filter(|j| !j.is_empty() && matches!(script.settings.out, OutFormat::Json));
        if let Some(j) = &jsonp {
            let _ = w.write_all(format!("{j}(").as_bytes());
        }
        let snap = match db.snapshot() {
            Ok(s) => s,
            Err(e) => {
                let _ = w.write_all(format!("runtime error: {e}").as_bytes());
                let _ = w.flush();
                return;
            }
        };
        let result = run_script(&snap, script, w, &opts, jsonp.is_some());
        let mut w = match result {
            Ok(p) => p.w,
            Err((Some(p), _)) => p.w,
            Err((None, RunError::Io(_))) | Err((None, _)) => return,
        };
        let _ = w.flush();
    });
    match ready_rx.await {
        Ok(Ready::Ok(content_type)) => {
            if head_only {
                return ([(header::CONTENT_TYPE, content_type)], Body::empty()).into_response();
            }
            ([(header::CONTENT_TYPE, content_type)], Body::from_stream(RxStream { rx, cancel })).into_response()
        }
        // C++ answers HEAD/OPTIONS with headers only, even for bad queries.
        Ok(Ready::Error(_)) if head_only => ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], Body::empty()).into_response(),
        Ok(Ready::Error(msg)) => error_page(StatusCode::BAD_REQUEST, &msg.lines().map(String::from).collect::<Vec<_>>()),
        Ok(Ready::Busy) | Err(_) => busy_page(),
    }
}

/// What the query thread reports before it streams output.
enum Ready {
    /// The query runs; its output has this content type.
    Ok(&'static str),
    /// The query could not be parsed.
    Error(String),
    /// No room for the query's maxsize in time.
    Busy,
}

/// Stream of body chunks produced by the query thread. Dropping it (the
/// client went away) cancels the query.
struct RxStream {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    cancel: Arc<AtomicBool>,
}

impl futures_core::Stream for RxStream {
    type Item = Result<Bytes, std::io::Error>;
    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for RxStream {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

async fn status(State(st): State<Arc<AppState>>) -> Response {
    let free = st.slots.available_permits();
    let snap_ts = st.db.snapshot().ok().and_then(|s| s.info("osm_base").ok().flatten()).unwrap_or_default();
    let body = format!(
        "Connected as: 0\nCurrent time: {}\nData timestamp: {}\nRate limit: 0\n{} slots available now.\nQueries served: {}\nUptime: {} seconds\n",
        crate::model::format_timestamp(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)),
        snap_ts,
        free,
        st.served.load(Ordering::Relaxed),
        st.started.elapsed().as_secs()
    );
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

async fn timestamp(State(st): State<Arc<AppState>>) -> Response {
    let ts = st.db.snapshot().ok().and_then(|s| s.info("osm_base").ok().flatten()).unwrap_or_default();
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], format!("{ts}\n")).into_response()
}

