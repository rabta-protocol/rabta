use std::sync::OnceLock;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::{AppState, SSE_ACTIVE};
use crate::error::{ApiError, ApiResult};
use crate::store::events::{self, ListFilters, ListedRow};
use crate::store::queries;
use crate::store::reaper;

type Connection = rusqlite::Connection;

const MAX_ENVELOPE: usize = crate::db::MAX_ENVELOPE;

// ---------------------------------------------------------------------------
// POST /api/v1/events — publish (application/json only)
// ---------------------------------------------------------------------------

pub async fn publish(State(n): State<AppState>, req: Request<Body>) -> Response {
    // The one event door: envelope JSON only. Blobs are a separate plane and
    // their bytes are never embedded in an event; `bin` only references them.
    let bytes = match read_body_capped(req, MAX_ENVELOPE).await {
        Ok(b) => b.to_vec(),
        Err(e) => return e.into_response(),
    };
    run(publish_envelope(n, bytes).await)
}

pub fn run<T: IntoResponse>(r: ApiResult<T>) -> Response {
    match r {
        Ok(x) => x.into_response(),
        Err(e) => e.into_response(),
    }
}

/// Read the whole body with a hard cap; oversize answers with the contract's
/// JSON error (`too_large`), not the default plaintext 413.
pub async fn read_body_capped(req: Request<Body>, cap: usize) -> ApiResult<axum::body::Bytes> {
    match axum::body::to_bytes(req.into_body(), cap).await {
        Ok(b) => Ok(b),
        Err(_) => Err(ApiError::too_large("request body exceeds the cap")),
    }
}

/// Core publish: validate → one atomic insert.
pub async fn publish_envelope(n: AppState, bytes: Vec<u8>) -> ApiResult<Response> {
    let now = reaper::now();
    let ev = events::parse_and_verify(&bytes, now)?;
    let inserted = insert_event(&n, &ev).await?;

    if inserted {
        n.db.notify_event_row();
    }

    let body = json!({
        "status": if inserted { "stored" } else { "duplicate" },
        "id": ev.id_hex,
    });
    let status = if inserted {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(body)).into_response())
}

/// One transaction: pressure check → event row.
async fn insert_event(n: &AppState, ev: &events::ParsedEvent) -> ApiResult<bool> {
    let cfg = n.cfg.clone();
    let ev2 = ev.clone();

    let out: ApiResult<bool> =
        n.db.write(move |c: &rusqlite::Connection| -> ApiResult<bool> {
            let tx = c
                .unchecked_transaction()
                .map_err(|e| ApiError::internal(format!("event begin: {e}")))?;
            // Duplicates add no bytes: check first so a retry cannot evict
            // unrelated events for a write that will not consume space.
            let exists = events::event_exists(&tx, &ev2.id)
                .map_err(|e| ApiError::internal(format!("event lookup: {e}")))?;
            if !exists {
                let claims = events::new_claim_bytes(&tx, ev2.bin.as_deref())
                    .map_err(|e| ApiError::internal(format!("claim lookup: {e}")))?;
                reaper::event_pressure_check(&tx, &cfg, ev2.size as u64 + claims.max(0) as u64)?;
            }
            let inserted = events::insert_event(&tx, &ev2)
                .map_err(|e| ApiError::internal(format!("event insert: {e}")))?;
            tx.commit()
                .map_err(|e| ApiError::internal(format!("event commit: {e}")))?;
            Ok(inserted)
        })
        .await;
    out
}

// ---------------------------------------------------------------------------
// GET /api/v1/events — list with filters + opaque cursor
// ---------------------------------------------------------------------------

/// Parsed `GET /api/v1/events` query. `label` may repeat (AND semantics), so the
/// query string is parsed manually — serde_urlencoded collapses duplicates.
#[derive(Debug, Default, Clone)]
pub struct RawQ {
    pub collection: Option<String>,
    pub labels: Vec<String>,
    pub owner: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

pub fn parse_raw_query(q: Option<&str>) -> RawQ {
    let mut out = RawQ::default();
    let Some(q) = q else { return out };
    for part in q.split('&') {
        let (k, v) = match part.split_once('=') {
            Some((k, v)) => (k, v),
            None => (part, ""),
        };
        let val = urldecode(v);
        match k {
            "collection" => out.collection = Some(val),
            "label" => out.labels.push(val),
            "owner" => out.owner = Some(val),
            "limit" => out.limit = val.parse::<u32>().ok(),
            "cursor" => out.cursor = Some(val),
            _ => {}
        }
    }
    out
}

/// Repeated distinct labels mean AND. Empty or duplicate labels are client
/// mistakes (duplicates add no selectivity and inflate the SQL predicate).
fn validate_labels(labels: &[String]) -> ApiResult<()> {
    let mut seen = std::collections::HashSet::with_capacity(labels.len());
    for label in labels {
        if label.is_empty() {
            return Err(ApiError::bad_label("label filter must not be empty"));
        }
        if !seen.insert(label.as_str()) {
            return Err(ApiError::bad_label(format!(
                "duplicate label filter `{label}`"
            )));
        }
    }
    Ok(())
}

/// Percent-decode (+→space); lone `%` passes through literally.
fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let h = |c: u8| -> Option<u8> { (c as char).to_digit(16).map(|d| d as u8) };
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (h(b[i + 1]), h(b[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
            continue;
        }
        if b[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub async fn list(State(n): State<AppState>, uri: axum::http::Uri) -> Response {
    run(list_inner(n, parse_raw_query(uri.query())).await)
}

/// `owner` filter takes the canonical `ed25519:<64hex>` form (same as the
/// envelope), normalized lowercase. No bare-hex shorthand.
fn normalize_owner(s: &str) -> Option<String> {
    let hexed = s.strip_prefix("ed25519:")?.to_ascii_lowercase();
    if hexed.len() == 64 && hexed.bytes().all(|b| b.is_ascii_hexdigit()) {
        Some(hexed)
    } else {
        None
    }
}

async fn list_inner(n: AppState, p: RawQ) -> ApiResult<Response> {
    validate_labels(&p.labels)?;
    let now = reaper::now();
    let filters = ListFilters {
        collection: p.collection.clone(),
        labels: p.labels,
        owner_hex: owner_filter_for(&p.owner)?,
        limit: p.limit.unwrap_or(20).clamp(1, 100),
    };
    let cursor = match &p.cursor {
        Some(c) => match events::decode_cursor(c) {
            Some(x) => Some(x),
            None => return Err(ApiError::bad_json("invalid cursor")),
        },
        None => None,
    };

    let (pred, mut binds) = events::build_predicate(&filters, now);
    let sql = events::list_sql(pred, &mut binds, cursor, filters.limit);
    let rows: Vec<ListedRow> =
        n.db.read(move |c: &Connection| events::run_list(c, &sql, &binds))
            .await
            .map_err(ApiError::from)?;

    let mut items = Vec::with_capacity(rows.len());
    for r in &rows {
        let v: serde_json::Value = serde_json::from_slice(&r.served)
            .map_err(|e| ApiError::internal(format!("stored envelope corrupt: {e}")))?;
        items.push(v);
    }
    let next_cursor = if rows.len() == filters.limit as usize {
        rows.last()
            .map(|r| events::encode_cursor(r.created_at, &r.id))
    } else {
        None
    };
    let body = json!({ "count": items.len(), "events": items, "next_cursor": next_cursor });
    Ok(Json(body).into_response())
}

fn owner_filter_for(s: &Option<String>) -> ApiResult<Option<String>> {
    match s {
        None => Ok(None),
        Some(raw) => normalize_owner(raw)
            .map(Some)
            .ok_or_else(ApiError::bad_owner),
    }
}

// ---------------------------------------------------------------------------
// GET /api/v1/events/{id} + GET /api/v1/events/{id}/size
// ---------------------------------------------------------------------------

pub fn parse_id_hex(s: &str) -> ApiResult<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::not_found("not a valid event id"));
    }
    let mut id = [0u8; 32];
    id.copy_from_slice(&hex::decode(s).expect("checked"));
    Ok(id)
}

fn if_none_match_ok(headers: &HeaderMap, etag: &str) -> bool {
    let Some(im) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let wanted = format!("\"{etag}\"");
    im.split(',')
        .any(|p| p.trim() == "*" || p.trim() == wanted || p.trim() == etag)
}

fn etag_headers(id_hex: &str) -> (header::HeaderValue, header::HeaderValue) {
    (
        header::HeaderValue::from_str(&format!("\"{id_hex}\"")).unwrap(),
        header::HeaderValue::from_static("application/json"),
    )
}

pub async fn get_one(
    State(n): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    run(get_one_inner(n, headers, id).await)
}

async fn get_one_inner(n: AppState, headers: HeaderMap, id: String) -> ApiResult<Response> {
    let idb = parse_id_hex(&id)?;
    let now = reaper::now();
    let row: Option<queries::EventRow> =
        n.db.read(move |c: &Connection| queries::get_event(c, &idb))
            .await;
    let Some(row) = row else {
        return Err(ApiError::not_found("no such event"));
    };
    if row.expires_at <= now {
        return Err(ApiError::not_found("event expired"));
    }
    if if_none_match_ok(&headers, &row.id_hex) {
        let (etag, _ct) = etag_headers(&row.id_hex);
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        resp.headers_mut().insert(header::ETAG, etag);
        return Ok(resp);
    }
    let (etag, ct) = etag_headers(&row.id_hex);
    let mut resp = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, etag)
        .header(header::CONTENT_TYPE, ct)
        .body(Body::from(row.served))
        .unwrap();
    resp.headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    Ok(resp)
}

pub async fn size(State(n): State<AppState>, Path(id): Path<String>) -> Response {
    run(size_inner(n, id).await)
}

async fn size_inner(n: AppState, id: String) -> ApiResult<Response> {
    let idb = parse_id_hex(&id)?;
    let now = reaper::now();
    let row: Option<queries::EventRow> =
        n.db.read(move |c: &Connection| queries::get_event(c, &idb))
            .await;
    let Some(row) = row else {
        return Err(ApiError::not_found("no such event"));
    };
    if row.expires_at <= now {
        return Err(ApiError::not_found("event expired"));
    }
    let attachment_bytes =
        n.db.read(move |c: &Connection| queries::attached_blob_bytes(c, &idb))
            .await;
    let body = json!({
        "id": row.id_hex,
        "envelope_size": row.size,
        "total_size": row.size.saturating_add(attachment_bytes),
    });
    Ok(Json(body).into_response())
}

// ---------------------------------------------------------------------------
// GET /api/v1/events/stream — SSE with Last-Event-ID resume + 15s keepalives
// ---------------------------------------------------------------------------

pub async fn stream(
    State(n): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> Response {
    run(stream_inner(n, headers, parse_raw_query(uri.query())).await)
}

struct SseGuard;
impl Drop for SseGuard {
    fn drop(&mut self) {
        SSE_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Atomically reserve a subscriber slot. The old load-then-increment dance let
/// concurrent requests all pass the check and oversubscribe the cap.
fn try_acquire_sse_slot(active: &std::sync::atomic::AtomicI64, cap: i64) -> bool {
    let mut current = active.load(Ordering::Acquire);
    loop {
        if current >= cap {
            return false;
        }
        match active.compare_exchange_weak(
            current,
            current + 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return true,
            Err(actual) => current = actual,
        }
    }
}

/// One live row, broadcast to every SSE subscriber. Each subscriber applies its
/// own filters in memory, so one shared feed serves every subscriber.
struct SseRow {
    seq: i64,
    collection: String,
    owner: Vec<u8>,
    labels: Vec<String>,
    expires_at: i64,
    state: String,
    served: Vec<u8>,
}

type SseSender = tokio::sync::mpsc::Sender<Result<axum::body::Bytes, axum::Error>>;

type SseFeed = tokio::sync::broadcast::Sender<std::sync::Arc<SseRow>>;

/// One feed per database, keyed by `index.db` path. Production has exactly one
/// node/database; the map keeps independent in-process databases from leaking
/// rows into each other's subscribers.
static SSE_FEEDS: OnceLock<
    tokio::sync::Mutex<std::collections::HashMap<std::path::PathBuf, SseFeed>>,
> = OnceLock::new();
const SSE_FEED_CAPACITY: usize = 2048;
const SSE_FEED_BATCH: i64 = 1000;

fn sse_feeds() -> &'static tokio::sync::Mutex<std::collections::HashMap<std::path::PathBuf, SseFeed>>
{
    SSE_FEEDS.get_or_init(|| tokio::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Start (once) the shared feed for this database and subscribe to it. The
/// subscription is created while the feed map is locked, so the feed cannot
/// retire itself between handing out the sender and the caller subscribing.
async fn ensure_sse_feed(
    db: &crate::db::Db,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> tokio::sync::broadcast::Receiver<std::sync::Arc<SseRow>> {
    let mut feeds = sse_feeds().lock().await;
    if let Some(tx) = feeds.get(db.path()) {
        return tx.subscribe();
    }

    let cursor = db
        .read(|c: &Connection| {
            c.query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |r| r.get(0))
                .unwrap_or(0)
        })
        .await;
    let (tx, _) = tokio::sync::broadcast::channel(SSE_FEED_CAPACITY);
    let rx = tx.subscribe();
    feeds.insert(db.path().to_path_buf(), tx.clone());
    tokio::spawn(sse_feed_loop(
        db.clone(),
        db.path().to_path_buf(),
        tx,
        cursor,
        shutdown,
    ));
    rx
}

/// Drain committed rows, then sleep until ingest signals another insert. With
/// no subscribers the feed removes itself, so an idle node does no SSE DB work.
async fn sse_feed_loop(
    db: crate::db::Db,
    path: std::path::PathBuf,
    tx: SseFeed,
    mut cursor: i64,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        // Read until a short batch, so a burst is fully drained before sleeping.
        loop {
            let from = cursor;
            let rows = db
                .read(move |c: &Connection| read_sse_rows(c, from, SSE_FEED_BATCH))
                .await
                .unwrap_or_default();
            let full = rows.len() as i64 == SSE_FEED_BATCH;
            if let Some(last) = rows.last() {
                cursor = last.seq;
            }
            for row in rows {
                let _ = tx.send(std::sync::Arc::new(row));
            }
            if !full || *shutdown.borrow() {
                break;
            }
        }

        if *shutdown.borrow() {
            remove_sse_feed(&path, &tx).await;
            tracing::debug!(path = %path.display(), "sse feed stopped for shutdown");
            return;
        }
        if tx.receiver_count() == 0 && retire_sse_feed(&path, &tx).await {
            tracing::debug!(path = %path.display(), "sse feed retired (no subscribers)");
            return;
        }
        tokio::select! {
            _ = super::wait_for_shutdown(&mut shutdown) => {}
            _ = db.wait_for_event_row() => {}
            _ = tx.closed() => {}
        }
    }
}

/// Drop a feed from the map only if it is still the registered channel.
async fn remove_sse_feed(path: &std::path::Path, tx: &SseFeed) {
    let mut feeds = sse_feeds().lock().await;
    if feeds
        .get(path)
        .is_some_and(|current| current.same_channel(tx))
    {
        feeds.remove(path);
    }
}

/// Remove a feed only while holding the map lock and re-checking the receiver
/// count, so a subscriber that attaches during shutdown is never stranded.
async fn retire_sse_feed(path: &std::path::Path, tx: &SseFeed) -> bool {
    let mut feeds = sse_feeds().lock().await;
    let Some(current) = feeds.get(path) else {
        return true; // already removed; no channel left to serve
    };
    if !current.same_channel(tx) {
        return true; // superseded by a newer feed for this path
    }
    if tx.receiver_count() == 0 {
        feeds.remove(path);
        return true;
    }
    false
}

/// Read rows in sequence order without an expiry predicate: the cursor must
/// move past dead rows too, otherwise they would be rescanned on every feed tick.
fn read_sse_rows(c: &Connection, after: i64, limit: i64) -> rusqlite::Result<Vec<SseRow>> {
    let mut stmt = c.prepare(
        "SELECT seq, collection, owner, labels, expires_at, state, data
         FROM events WHERE seq > ?1 ORDER BY seq ASC LIMIT ?2",
    )?;
    let mut rows = stmt.query(rusqlite::params![after, limit])?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        out.push(SseRow {
            seq: r.get(0)?,
            collection: r.get(1)?,
            owner: r.get(2)?,
            labels: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
            expires_at: r.get(4)?,
            state: r.get(5)?,
            served: r.get(6)?,
        });
    }
    Ok(out)
}

fn sse_matches(filters: &ListFilters, row: &SseRow) -> bool {
    if let Some(collection) = &filters.collection
        && &row.collection != collection
    {
        return false;
    }
    if let Some(owner) = &filters.owner_hex {
        let Ok(owner) = hex::decode(owner) else {
            return false;
        };
        if row.owner != owner {
            return false;
        }
    }
    if !filters.labels.is_empty()
        && !filters
            .labels
            .iter()
            .all(|label| row.labels.contains(label))
    {
        return false;
    }
    true
}

fn sse_frame(seq: i64, served: &[u8]) -> axum::body::Bytes {
    axum::body::Bytes::from(format!(
        "id: {seq}\nevent: message\ndata: {}\n\n",
        String::from_utf8_lossy(served)
    ))
}

/// Fetch the filtered backlog in sequence order. The returned cursor is the
/// high-water mark, not just the last matching row, so missed filtered rows
/// are never rescanned.
async fn sse_catch_up(
    db: &crate::db::Db,
    filters: &ListFilters,
    from: i64,
    to: i64,
    tx: &SseSender,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<i64, ()> {
    let mut next = from;
    while next < to {
        if *shutdown.borrow() {
            return Err(());
        }
        let filters = filters.clone();
        let lo = next;
        let hi = to;
        let batch = db
            .read(move |c: &Connection| {
                // rebuild the predicate per batch (clock moves; binds aren't clonable)
                let (pred, mut binds) = events::build_predicate(&filters, reaper::now());
                let r_at = events::next_bind(&mut binds, events::SqlParam::I(lo));
                let h_at = events::next_bind(&mut binds, events::SqlParam::I(hi));
                let l_at = events::next_bind(&mut binds, events::SqlParam::I(200));
                let sql = format!(
                    "{pred} AND seq > {r_at} AND seq <= {h_at} ORDER BY seq ASC LIMIT {l_at}"
                );
                events::run_list(c, &sql, &binds)
            })
            .await;
        let batch = match batch {
            Ok(batch) => batch,
            Err(e) => {
                tracing::debug!(error = format!("{e}"), "sse catch-up error");
                return Err(());
            }
        };
        if batch.is_empty() {
            return Ok(to);
        }
        let full = batch.len() == 200;
        for r in &batch {
            tokio::select! {
                sent = tx.send(Ok(sse_frame(r.seq, &r.served))) => {
                    if sent.is_err() {
                        return Err(());
                    }
                }
                _ = super::wait_for_shutdown(shutdown) => return Err(()),
            }
        }
        next = batch.last().map(|r| r.seq).unwrap_or(next);
        if !full {
            return Ok(to);
        }
    }
    Ok(to)
}

async fn stream_inner(n: AppState, headers: HeaderMap, p: RawQ) -> ApiResult<Response> {
    validate_labels(&p.labels)?;
    let filters = ListFilters {
        collection: p.collection.clone(),
        labels: p.labels,
        owner_hex: owner_filter_for(&p.owner)?,
        limit: 100,
    };
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());

    // Reserve the slot before spawning; the guard releases it when the
    // connection task ends. No await between this CAS and the spawn.
    if !try_acquire_sse_slot(&SSE_ACTIVE, n.cfg.sse_subscriber_cap as i64) {
        return Err(ApiError::subscriber_cap());
    }

    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    let (tx, rx) = mpsc::channel::<Result<axum::body::Bytes, axum::Error>>(32);
    let db = n.db.clone();
    let mut shutdown = n.shutdown.clone();

    tokio::spawn(async move {
        // subscriber accounting lives with the connection, not the response
        let _guard = SseGuard;

        // A connection admitted during shutdown must not open a backlog burst.
        if *shutdown.borrow() {
            return;
        }

        // Subscribe before taking the high-water mark. Rows committed while
        // the backlog is fetched are then either in that backlog or queued.
        let mut feed = ensure_sse_feed(&db, shutdown.clone()).await;
        let high_water = db
            .read(|c: &Connection| {
                c.query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |r| r.get(0))
                    .unwrap_or(0)
            })
            .await;

        // resume point: after the given sequence, else the newest row (future tail)
        let mut next_seq = match last_event_id {
            Some(x) => x,
            None => high_water,
        };
        tracing::debug!(sse_start = next_seq, high_water, "sse worker attached");

        if next_seq < high_water {
            match sse_catch_up(&db, &filters, next_seq, high_water, &tx, &mut shutdown).await {
                Ok(next) => next_seq = next,
                Err(()) => return, // client went away, shutting down, or DB failed
            }
        }

        let mut keepalive = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(15),
            std::time::Duration::from_secs(15),
        );
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                // Dropping `tx` here ends the response body.
                _ = super::wait_for_shutdown(&mut shutdown) => return,
                received = feed.recv() => {
                    match received {
                        Ok(row) => {
                            if row.seq <= next_seq {
                                continue;
                            }
                            next_seq = row.seq;
                            if row.state != "live"
                                || row.expires_at <= reaper::now()
                                || !sse_matches(&filters, &row)
                            {
                                continue;
                            }
                            if tx.send(Ok(sse_frame(row.seq, &row.served))).await.is_err() {
                                return;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::debug!(skipped, "sse subscriber lagged; catching up");
                            let current = db
                                .read(|c: &Connection| {
                                    c.query_row(
                                        "SELECT COALESCE(MAX(seq),0) FROM events",
                                        [],
                                        |r| r.get(0),
                                    )
                                    .unwrap_or(0)
                                })
                                .await;
                            if current > next_seq {
                                match sse_catch_up(
                                    &db,
                                    &filters,
                                    next_seq,
                                    current,
                                    &tx,
                                    &mut shutdown,
                                )
                                .await
                                {
                                    Ok(next) => next_seq = next,
                                    Err(()) => return,
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
                _ = keepalive.tick() => {
                    if tx.send(Ok(axum::body::Bytes::from_static(b": keepalive\n\n"))).await.is_err() {
                        return;
                    }
                }
                _ = tx.closed() => return,
            }
            if tx.is_closed() {
                return;
            }
        }
    });

    Ok(axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rabta-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn test_state(dir: &std::path::Path) -> AppState {
        let (n, shutdown) = test_state_with_shutdown(dir);
        // A dropped sender means "shutdown", so keep it alive for the test.
        let _ = Box::leak(Box::new(shutdown));
        n
    }

    /// Also hands back the flag's sender so a test can act as SIGTERM.
    fn test_state_with_shutdown(
        dir: &std::path::Path,
    ) -> (AppState, tokio::sync::watch::Sender<bool>) {
        let cfg = crate::config::Config::test_cfg(dir.to_path_buf(), 10_000_000);
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        (
            crate::api::Node::new(cfg, crate::db::Db::open(dir), rx),
            shutdown,
        )
    }

    /// SSE admission is process-wide; stream tests must not overlap.
    static STREAM_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn publish_returns_json_content_type() {
        let dir = temp_dir("event-content-type");
        let n = test_state(&dir);

        let now = reaper::now();
        let created_at = now - 10;
        let expires_at = now + 3600;
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let owner = format!("ed25519:{}", hex::encode(key.verifying_key().to_bytes()));
        let data = json!({"message": "hello"});
        let canonical =
            crate::canon::digest_value("test", created_at, expires_at, &owner, &[], &data, None);
        let digest: [u8; 32] = Sha256::digest(crate::canon::canonical_value(&canonical)).into();
        let sig = key.sign(&digest);
        let envelope = json!({
            "collection": "test",
            "created_at": created_at,
            "data": data,
            "expires_at": expires_at,
            "labels": [],
            "owner": owner,
            "id": hex::encode(digest),
            "sig": hex::encode(sig.to_bytes()),
        });

        let resp = publish_envelope(n, serde_json::to_vec(&envelope).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Insert one live row directly (the HTTP door adds the
    /// same notify call after their committed write).
    async fn insert_chat_row(n: &AppState) {
        let now = reaper::now();
        let served = br#"{"collection":"chat","data":{"n":1}}"#.to_vec();
        let size = served.len() as i64;
        n.db.write(move |c: &Connection| {
            let seq = crate::db::next_event_seq(c).unwrap();
            c.execute(
                "INSERT INTO events(id,seq,owner,collection,created_at,expires_at,data,labels,size,state)
                 VALUES (?1,?2,?3,'chat',?4,?5,?6,'[]',?7,'live')",
                rusqlite::params![&[1u8; 32], seq, &[9u8; 32], now - 1, now + 3600, &served, size],
            )
            .unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn feed_wakes_on_event_signal_and_retires_idle() {
        let dir = temp_dir("event-feed-wake");
        let n = test_state(&dir);
        let mut feed = ensure_sse_feed(&n.db, n.shutdown.clone()).await;

        insert_chat_row(&n).await;
        n.db.notify_event_row();

        let row = tokio::time::timeout(Duration::from_secs(2), feed.recv())
            .await
            .expect("feed did not wake on event signal")
            .expect("feed closed before delivering the row");
        assert_eq!(row.seq, 1);
        assert_eq!(row.collection, "chat");
        assert_eq!(row.state, "live");

        // With its last receiver gone the feed must remove itself, leaving an
        // idle node with no SSE database work.
        drop(feed);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !sse_feeds().lock().await.contains_key(n.db.path()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("idle feed did not retire");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stream_replays_backlog_on_resume() {
        use futures_util::StreamExt;

        let _stream_guard = STREAM_TEST_LOCK.lock().await;
        let dir = temp_dir("event-stream-resume");
        let n = test_state(&dir);

        // Resume from the start so the row is delivered whether it is committed
        // before or after the subscriber captures its high-water mark.
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", axum::http::HeaderValue::from_static("0"));
        let resp = stream_inner(
            n.clone(),
            headers,
            RawQ {
                collection: Some("chat".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body().into_data_stream();

        insert_chat_row(&n).await;
        n.db.notify_event_row();

        let frame = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("stream did not deliver the resumed row")
            .expect("event stream ended")
            .unwrap();
        let text = String::from_utf8_lossy(&frame);
        assert!(text.contains("event: message"), "unexpected frame: {text}");
        assert!(
            text.contains("\"collection\":\"chat\""),
            "unexpected frame: {text}"
        );

        drop(body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Regression: an open SSE stream used to block shutdown until SIGKILL.
    #[tokio::test]
    async fn stream_closes_on_shutdown_signal() {
        use futures_util::StreamExt;

        let _stream_guard = STREAM_TEST_LOCK.lock().await;
        let dir = temp_dir("event-stream-shutdown");
        let (n, shutdown) = test_state_with_shutdown(&dir);

        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", axum::http::HeaderValue::from_static("0"));
        let resp = stream_inner(
            n.clone(),
            headers,
            RawQ {
                collection: Some("chat".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut body = resp.into_body().into_data_stream();

        // One real frame proves the worker attached and is streaming.
        insert_chat_row(&n).await;
        n.db.notify_event_row();
        let frame = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("stream did not deliver a row")
            .expect("event stream ended early")
            .unwrap();
        assert!(
            String::from_utf8_lossy(&frame).contains("event: message"),
            "unexpected frame: {frame:?}"
        );

        shutdown.send(true).unwrap();

        let end = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("stream stayed open after the shutdown signal");
        assert!(end.is_none(), "stream produced a frame after shutdown");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn shared_feed_filters_in_memory() {
        let row = SseRow {
            seq: 9,
            collection: "chat".into(),
            owner: vec![0x11; 32],
            labels: vec!["team".into(), "urgent".into()],
            expires_at: reaper::now() + 3600,
            state: "live".into(),
            served: b"{}".to_vec(),
        };

        // no filters: everything matches
        assert!(sse_matches(&ListFilters::default(), &row));

        let collection = ListFilters {
            collection: Some("chat".into()),
            ..Default::default()
        };
        assert!(sse_matches(&collection, &row));
        let other = ListFilters {
            collection: Some("mail".into()),
            ..Default::default()
        };
        assert!(!sse_matches(&other, &row));

        let owner = ListFilters {
            owner_hex: Some(hex::encode([0x11; 32])),
            ..Default::default()
        };
        assert!(sse_matches(&owner, &row));
        let stranger = ListFilters {
            owner_hex: Some(hex::encode([0x22; 32])),
            ..Default::default()
        };
        assert!(!sse_matches(&stranger, &row));

        // repeated labels are AND, not OR
        let both = ListFilters {
            labels: vec!["team".into(), "urgent".into()],
            ..Default::default()
        };
        assert!(sse_matches(&both, &row));
        let missing = ListFilters {
            labels: vec!["team".into(), "missing".into()],
            ..Default::default()
        };
        assert!(!sse_matches(&missing, &row));
    }

    #[test]
    fn label_filters_reject_empty_and_duplicates() {
        let q = parse_raw_query(Some("label=team&label=urgent"));
        assert_eq!(q.labels, vec!["team".to_string(), "urgent".to_string()]);
        validate_labels(&q.labels).unwrap();

        let empty = vec![String::new()];
        assert_eq!(validate_labels(&empty).unwrap_err().code, "bad_label");
        let duplicate = vec!["team".to_string(), "team".to_string()];
        assert_eq!(validate_labels(&duplicate).unwrap_err().code, "bad_label");
    }

    #[test]
    fn invalid_owner_filter_is_rejected() {
        let err = owner_filter_for(&Some("ed25519:not-hex".into())).unwrap_err();
        assert_eq!(err.code, "bad_owner");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert!(owner_filter_for(&None).unwrap().is_none());

        let canonical = format!("ed25519:{}", "AB".repeat(32));
        let ok = owner_filter_for(&Some(canonical)).unwrap();
        assert_eq!(ok, Some("ab".repeat(32)));
    }

    #[test]
    fn subscriber_slots_are_never_oversubscribed() {
        use std::sync::atomic::AtomicUsize;

        let active = std::sync::atomic::AtomicI64::new(0);
        let admitted = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..64 {
                s.spawn(|| {
                    if try_acquire_sse_slot(&active, 8) {
                        admitted.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
        assert_eq!(admitted.load(Ordering::Relaxed), 8);
        assert_eq!(active.load(Ordering::Relaxed), 8);
    }
}
