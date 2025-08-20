use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::error::{ApiError, ApiResult};
use crate::store::events::{self, ListFilters, ListedRow};
use crate::store::ingest;
use crate::store::queries;
use crate::store::reaper;

type Connection = rusqlite::Connection;

const MAX_ENVELOPE: usize = crate::db::MAX_ENVELOPE;

// ---------------------------------------------------------------------------
// POST /api/v1/events — publish (application/json only)
// ---------------------------------------------------------------------------

pub async fn publish(State(n): State<AppState>, req: Request<Body>) -> Response {
    // The one event door: envelope JSON only. Blobs are a separate plane and
    // their bytes are never embedded in an event; `blobs` only references them.
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
    let inserted = ingest::event(&n, ev.clone()).await?;

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

// ---------------------------------------------------------------------------
// POST /api/v1/events/query — list with filters + opaque cursor
// ---------------------------------------------------------------------------

/// Typed JSON query body for `POST /api/v1/events/query`. Unknown
/// fields are rejected (`deny_unknown_fields`) so a typo like `collectoin`
/// is a `400 bad_json` instead of a silent match-all.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListQuery {
    pub collection: Option<String>,
    /// Wildcard collection match; mutually exclusive with `collection`.
    pub collection_glob: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub owner: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
    pub created_at: Option<events::TimeRange>,
    pub expires_at: Option<events::TimeRange>,
}

fn parse_list_query(bytes: &[u8]) -> ApiResult<ListQuery> {
    let q: ListQuery = serde_json::from_slice(bytes)
        .map_err(|e| ApiError::bad_json(format!("invalid query: {e}")))?;
    if let Some(limit) = q.limit
        && !(1..=100).contains(&limit)
    {
        return Err(ApiError::bad_json("limit must be between 1 and 100"));
    }
    Ok(q)
}

/// Distinct labels mean AND. Empty or duplicate labels are client
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

pub async fn query(State(n): State<AppState>, req: Request<Body>) -> Response {
    let bytes = match read_body_capped(req, crate::db::MAX_QUERY_BODY).await {
        Ok(b) => b.to_vec(),
        Err(e) => return e.into_response(),
    };
    let q = match parse_list_query(&bytes) {
        Ok(q) => q,
        Err(e) => return e.into_response(),
    };
    run(list_inner(n, q).await)
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

async fn list_inner(n: AppState, p: ListQuery) -> ApiResult<Response> {
    validate_labels(&p.labels)?;
    validate_filters(
        &p.collection,
        &p.collection_glob,
        &p.created_at,
        &p.expires_at,
    )?;
    let now = reaper::now();
    let filters = ListFilters {
        collection: p.collection.clone(),
        collection_glob: p.collection_glob,
        labels: p.labels,
        owner_hex: owner_filter_for(&p.owner)?,
        limit: p.limit.unwrap_or(20),
        created_at: p.created_at.unwrap_or_default(),
        expires_at: p.expires_at.unwrap_or_default(),
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
// POST /api/v1/events/query/count — reconcile a local copy against the relay
// ---------------------------------------------------------------------------

/// Typed JSON body for `POST /api/v1/events/query/count`: the `/events/query`
/// filters (no `limit`/`cursor` — one number, no paging) plus half-open
/// `created_at`/`expires_at` ranges so a client can diff bucket by bucket.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CountQuery {
    pub collection: Option<String>,
    /// Wildcard collection match (`*` / `?`); mutually exclusive with
    /// `collection`. Charset mirrors `collection` plus the metacharacters.
    pub collection_glob: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub owner: Option<String>,
    pub created_at: Option<events::TimeRange>,
    pub expires_at: Option<events::TimeRange>,
}

fn parse_count_query(bytes: &[u8]) -> ApiResult<CountQuery> {
    serde_json::from_slice(bytes).map_err(|e| ApiError::bad_json(format!("invalid query: {e}")))
}

/// A range body must bind at least one side and keep `[gte, lt)` ordered;
/// otherwise it is a client mistake, not a silent match-all.
fn validate_time_range(r: &events::TimeRange) -> ApiResult<()> {
    if r.gte.is_none() && r.lt.is_none() {
        return Err(ApiError::bad_json(
            "time range needs at least one of `gte` or `lt`",
        ));
    }
    if let (Some(gte), Some(lt)) = (r.gte, r.lt)
        && gte >= lt
    {
        return Err(ApiError::bad_json("time range is empty (gte >= lt)"));
    }
    Ok(())
}

/// Glob patterns share the collection charset plus `*` / `?`; character
/// classes (`[...]`) stay rejected so patterns never need escaping — stored
/// collections cannot contain metacharacters either.
fn validate_collection_glob(pat: &str) -> ApiResult<()> {
    if pat.is_empty() {
        return Err(ApiError::bad_json("collection_glob must not be empty"));
    }
    if pat.len() > 64 {
        return Err(ApiError::bad_json("collection_glob exceeds 64 characters"));
    }
    if !pat.bytes().all(|b| {
        b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || matches!(b, b'/' | b'_' | b'-' | b'*' | b'?')
    }) {
        return Err(ApiError::bad_json(
            "collection_glob allows a-z, 0-9, /, _, -, *, ? only",
        ));
    }
    Ok(())
}

/// Filter rules shared by `/events/query` and `/events/query/count`: exact and
/// wildcard collection are one-of, patterns follow the glob charset, and each
/// time range must bind at least one ordered side.
fn validate_filters(
    collection: &Option<String>,
    collection_glob: &Option<String>,
    created_at: &Option<events::TimeRange>,
    expires_at: &Option<events::TimeRange>,
) -> ApiResult<()> {
    if collection.is_some() && collection_glob.is_some() {
        return Err(ApiError::bad_json(
            "`collection` and `collection_glob` are mutually exclusive",
        ));
    }
    if let Some(pat) = collection_glob {
        validate_collection_glob(pat)?;
    }
    if let Some(r) = created_at {
        validate_time_range(r)?;
    }
    if let Some(r) = expires_at {
        validate_time_range(r)?;
    }
    Ok(())
}

pub async fn count(State(n): State<AppState>, req: Request<Body>) -> Response {
    let bytes = match read_body_capped(req, crate::db::MAX_QUERY_BODY).await {
        Ok(b) => b.to_vec(),
        Err(e) => return e.into_response(),
    };
    let q = match parse_count_query(&bytes) {
        Ok(q) => q,
        Err(e) => return e.into_response(),
    };
    run(count_inner(n, q).await)
}

async fn count_inner(n: AppState, p: CountQuery) -> ApiResult<Response> {
    validate_labels(&p.labels)?;
    validate_filters(
        &p.collection,
        &p.collection_glob,
        &p.created_at,
        &p.expires_at,
    )?;
    let now = reaper::now();
    let filters = ListFilters {
        collection: p.collection.clone(),
        collection_glob: p.collection_glob,
        labels: p.labels,
        owner_hex: owner_filter_for(&p.owner)?,
        limit: 0,
        created_at: p.created_at.unwrap_or_default(),
        expires_at: p.expires_at.unwrap_or_default(),
    };

    let (pred, binds) = events::build_predicate(&filters, now);
    let count =
        n.db.read(move |c: &Connection| events::run_count(c, &pred, &binds))
            .await
            .map_err(ApiError::from)?;
    Ok(Json(json!({ "count": count })).into_response())
}

// ---------------------------------------------------------------------------
// GET /api/v1/events/{id} + GET /api/v1/events/{id}/size
// ---------------------------------------------------------------------------

pub fn parse_id_hex(s: &str) -> ApiResult<[u8; 32]> {
    if s.len() != 64 {
        return Err(ApiError::not_found("not a valid event id"));
    }
    let mut id = [0u8; 32];
    if hex::decode_to_slice(s, &mut id).is_err() {
        return Err(ApiError::not_found("not a valid event id"));
    }
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
    let etag = header::HeaderValue::from_str(&format!("\"{id_hex}\""))
        .unwrap_or_else(|_| header::HeaderValue::from_static("\"\""));
    (etag, header::HeaderValue::from_static("application/json"))
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
    let resp = axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::ETAG, etag)
        .header(header::CONTENT_TYPE, ct)
        .header(
            header::X_CONTENT_TYPE_OPTIONS,
            header::HeaderValue::from_static("nosniff"),
        )
        .body(Body::from(row.served))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
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

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use std::time::{SystemTime, UNIX_EPOCH};

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
        let cfg = crate::config::Config::test_cfg(dir.to_path_buf(), 10_000_000);
        let identity = crate::identity::NodeIdentity::generate();
        crate::api::Node::new(cfg, crate::db::Db::open(dir), identity)
    }

    /// Insert one live row directly, without the publish verify path.
    async fn insert_chat_row(n: &AppState) {
        let now = reaper::now();
        let served = br#"{"collection":"chat","data":{"n":1}}"#.to_vec();
        let size = served.len() as i64;
        n.db.write(move |c: &Connection| {
            c.execute(
                "INSERT INTO events(id,owner,collection,created_at,expires_at,data,size)
                 VALUES (?1,?2,'chat',?3,?4,?5,?6)",
                rusqlite::params![&[1u8; 32], &[9u8; 32], now - 1, now + 3600, &served, size],
            )
            .unwrap();
        })
        .await;
    }

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
            crate::canon::digest_value("test", created_at, expires_at, &owner, &[], &data, &[]);
        let digest: [u8; 32] = Sha256::digest(crate::canon::canonical_value(&canonical)).into();
        let sig = key.sign(&digest);
        let envelope = json!({
            "collection": "test",
            "created_at": created_at,
            "data": data,
            "expires_at": expires_at,
            "labels": [],
            "blobs": [],
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

    #[tokio::test]
    async fn query_returns_matching_window() {
        let dir = temp_dir("event-query-list");
        let n = test_state(&dir);
        insert_chat_row(&n).await;

        let resp = list_inner(
            n.clone(),
            ListQuery {
                collection: Some("chat".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["count"], 1);
        assert!(body["events"].is_array());
        assert!(body.get("next_cursor").is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Insert one row with full identity/timing control, including labels,
    /// without the publish verify path.
    async fn insert_row(
        n: &AppState,
        id: u8,
        collection: &str,
        owner: u8,
        created_at: i64,
        expires_at: i64,
        labels: &[&str],
    ) {
        let served = br#"{"data":{}}"#.to_vec();
        let size = served.len() as i64;
        let collection = collection.to_string();
        let labels: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
        n.db.write(move |c: &Connection| {
            c.execute(
                "INSERT INTO events(id,owner,collection,created_at,expires_at,data,size)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                rusqlite::params![
                    &[id; 32][..],
                    &[owner; 32][..],
                    collection,
                    created_at,
                    expires_at,
                    &served,
                    size
                ],
            )
            .unwrap();
            for l in &labels {
                c.execute(
                    "INSERT OR IGNORE INTO labels(event_id, label) VALUES (?1, ?2)",
                    rusqlite::params![&[id; 32][..], l],
                )
                .unwrap();
            }
        })
        .await;
    }

    #[tokio::test]
    async fn count_matches_filters_and_ranges() {
        let dir = temp_dir("event-count");
        let n = test_state(&dir);

        // Two bucketed chat rows, one labelled, one expired, one other collection.
        insert_row(&n, 1, "chat", 9, 1_700_000_000, 9_999_999_999, &["team"]).await;
        insert_row(&n, 2, "chat", 9, 1_700_500_000, 9_999_999_999, &[]).await;
        insert_row(&n, 3, "chat", 9, 1_700_000_000, 0, &[]).await;
        insert_row(&n, 4, "docs", 8, 1_700_000_000, 9_999_999_999, &[]).await;

        async fn count(n: &AppState, p: CountQuery) -> i64 {
            let resp = count_inner(n.clone(), p).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            body["count"].as_i64().unwrap()
        }

        let owner_9 = format!("ed25519:{}", hex::encode([9u8; 32]));
        assert_eq!(
            count(&n, CountQuery::default()).await,
            3,
            "expired row counted"
        );
        assert_eq!(
            count(
                &n,
                CountQuery {
                    collection: Some("docs".into()),
                    ..Default::default()
                }
            )
            .await,
            1
        );
        assert_eq!(
            count(
                &n,
                CountQuery {
                    owner: Some(owner_9),
                    ..Default::default()
                }
            )
            .await,
            2,
            "owner 9 owns rows 1..3, docs owner differs"
        );
        assert_eq!(
            count(
                &n,
                CountQuery {
                    labels: vec!["team".into()],
                    ..Default::default()
                }
            )
            .await,
            1
        );

        // Half-open buckets [lo, hi) tile the timeline without overlap.
        let created_bucket = |lo: Option<i64>, hi: Option<i64>| CountQuery {
            created_at: Some(events::TimeRange { gte: lo, lt: hi }),
            ..Default::default()
        };
        assert_eq!(
            count(&n, created_bucket(Some(1_700_300_000), None)).await,
            1
        );
        assert_eq!(
            count(&n, created_bucket(Some(1_700_000_000), Some(1_700_300_000))).await,
            2
        );
        assert_eq!(
            count(&n, created_bucket(Some(1_700_300_000), Some(1_700_600_000))).await,
            1
        );
        // A matching expires_at window still requires liveness.
        assert_eq!(
            count(
                &n,
                CountQuery {
                    expires_at: Some(events::TimeRange {
                        lt: Some(1_000_000),
                        ..Default::default()
                    }),
                    ..Default::default()
                }
            )
            .await,
            0
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn query_supports_glob_and_time_ranges() {
        let dir = temp_dir("event-query-glob");
        let n = test_state(&dir);

        insert_row(&n, 1, "chat/room-42", 9, 1_700_000_000, 9_999_999_999, &[]).await;
        insert_row(&n, 2, "chat/room-43", 9, 1_700_500_000, 9_999_999_999, &[]).await;
        insert_row(
            &n,
            3,
            "sensor/temp-east",
            9,
            1_700_000_000,
            9_999_999_999,
            &[],
        )
        .await;

        async fn list(n: &AppState, p: ListQuery) -> ApiResult<serde_json::Value> {
            let resp = list_inner(n.clone(), p).await?;
            Ok(serde_json::from_slice(
                &axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap())
        }

        let body = list(
            &n,
            ListQuery {
                collection_glob: Some("chat/*".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(body["count"], 2);

        // Glob and range compose: only the newer chat row is in the bucket.
        let body = list(
            &n,
            ListQuery {
                collection_glob: Some("chat/*".into()),
                created_at: Some(events::TimeRange {
                    gte: Some(1_700_300_000),
                    lt: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(body["count"], 1);

        let err = list(
            &n,
            ListQuery {
                collection: Some("chat".into()),
                collection_glob: Some("chat/*".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "bad_json", "exact and wildcard are one-of");

        let err = list(
            &n,
            ListQuery {
                collection_glob: Some("[x]".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "bad_json", "classes are not part of the glob");

        let err = list(
            &n,
            ListQuery {
                expires_at: Some(events::TimeRange {
                    gte: None,
                    lt: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, "bad_json", "an empty range binds no side");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn query_body_is_strict() {
        let err = parse_list_query(br#"{"collection":"chat","collectoin":"chat"}"#).unwrap_err();
        assert_eq!(err.code, "bad_json");
        assert_eq!(
            parse_list_query(b"{}").unwrap().collection,
            None,
            "an empty object is the explicit match-all query"
        );
        assert_eq!(parse_list_query(b"").unwrap_err().code, "bad_json");
        assert_eq!(
            parse_list_query(br#"{"limit":0}"#).unwrap_err().code,
            "bad_json"
        );
        assert_eq!(
            parse_list_query(br#"{"limit":101}"#).unwrap_err().code,
            "bad_json"
        );

        let q = parse_list_query(
            br#"{"collection_glob":"chat/*","created_at":{"gte":1,"lt":2},"expires_at":{"gte":3}}"#,
        )
        .unwrap();
        assert_eq!(q.collection_glob.as_deref(), Some("chat/*"));
        assert_eq!(q.created_at.unwrap().gte, Some(1));
        assert_eq!(q.expires_at.unwrap().gte, Some(3));
        assert_eq!(
            parse_list_query(br#"{"created_at":{"gt":1}}"#)
                .unwrap_err()
                .code,
            "bad_json",
            "range bound names are strict"
        );
    }

    #[test]
    fn label_filters_reject_empty_and_duplicates() {
        let q = parse_list_query(br#"{"labels":["team","urgent"]}"#).unwrap();
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
    fn count_body_is_strict() {
        let q = parse_count_query(br#"{"collection":"chat"}"#).unwrap();
        assert_eq!(q.collection.as_deref(), Some("chat"));
        assert!(q.created_at.is_none());

        // `{}` is the explicit match-all count.
        assert!(parse_count_query(b"{}").unwrap().collection.is_none());
        assert_eq!(parse_count_query(b"").unwrap_err().code, "bad_json");

        // Paging is deliberately absent from the count contract.
        assert_eq!(
            parse_count_query(br#"{"limit":10}"#).unwrap_err().code,
            "bad_json"
        );
        assert_eq!(
            parse_count_query(br#"{"cursor":"abc"}"#).unwrap_err().code,
            "bad_json"
        );
        assert_eq!(
            parse_count_query(br#"{"collectoin":"chat"}"#)
                .unwrap_err()
                .code,
            "bad_json"
        );
    }

    #[test]
    fn count_time_range_validation() {
        let err = validate_time_range(&events::TimeRange {
            gte: None,
            lt: None,
        })
        .unwrap_err();
        assert_eq!(err.code, "bad_json", "an empty range binds no side");

        let err = validate_time_range(&events::TimeRange {
            gte: Some(5),
            lt: Some(5),
        })
        .unwrap_err();
        assert_eq!(
            err.code, "bad_json",
            "an inverted range is a client mistake"
        );

        validate_time_range(&events::TimeRange {
            gte: Some(4),
            lt: Some(5),
        })
        .unwrap();
        validate_time_range(&events::TimeRange {
            gte: None,
            lt: Some(5),
        })
        .unwrap();
        validate_time_range(&events::TimeRange {
            gte: Some(4),
            lt: None,
        })
        .unwrap();
    }

    #[test]
    fn count_glob_validation() {
        assert!(validate_collection_glob("chat/*").is_ok());
        assert!(validate_collection_glob("chat/room-4?").is_ok());
        assert!(validate_collection_glob("chat/room-42").is_ok());
        assert!(validate_collection_glob("*").is_ok());

        assert_eq!(
            validate_collection_glob("").unwrap_err().code,
            "bad_json",
            "an empty pattern matches nothing by intent"
        );
        assert_eq!(
            validate_collection_glob("chat/*[a-z]").unwrap_err().code,
            "bad_json",
            "character classes are not supported"
        );
        assert_eq!(
            validate_collection_glob("Chat/*").unwrap_err().code,
            "bad_json"
        );
        assert_eq!(
            validate_collection_glob("chat/бый").unwrap_err().code,
            "bad_json",
            "non-ascii bytes are not part of the collection charset"
        );

        let long = format!("{}/{}", "a".repeat(30), "*".repeat(35));
        assert_eq!(
            validate_collection_glob(&long).unwrap_err().code,
            "bad_json",
            "patterns exceeding 64 chars are rejected"
        );
    }

    #[tokio::test]
    async fn count_rejects_collection_and_glob_together() {
        let dir = temp_dir("event-count-both");
        let n = test_state(&dir);

        let mut p = CountQuery {
            collection: Some("chat".into()),
            ..Default::default()
        };
        assert!(
            count_inner(n.clone(), p.clone()).await.is_ok(),
            "collection alone stays valid"
        );

        p.collection_glob = Some("chat/*".into());
        let err = count_inner(n.clone(), p).await.unwrap_err();
        assert_eq!(err.code, "bad_json");
        assert_eq!(err.status, StatusCode::BAD_REQUEST);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn count_glob_matches_subtrees() {
        let dir = temp_dir("event-count-glob");
        let n = test_state(&dir);

        insert_row(&n, 1, "chat/room-42", 9, 1_700_000_000, 9_999_999_999, &[]).await;
        insert_row(&n, 2, "chat/room-43", 9, 1_700_000_000, 9_999_999_999, &[]).await;
        insert_row(
            &n,
            3,
            "sensor/temp-east",
            9,
            1_700_000_000,
            9_999_999_999,
            &[],
        )
        .await;
        insert_row(&n, 4, "chat/room-44", 9, 1_700_000_000, 0, &[]).await; // expired

        async fn count(n: &AppState, glob: &str) -> i64 {
            let resp = count_inner(
                n.clone(),
                CountQuery {
                    collection_glob: Some(glob.into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let body: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap(),
            )
            .unwrap();
            body["count"].as_i64().unwrap()
        }

        assert_eq!(count(&n, "chat/*").await, 2, "expired subtree row excluded");
        assert_eq!(count(&n, "chat/room-4?").await, 2);
        assert_eq!(count(&n, "sensor/*").await, 1);
        assert_eq!(count(&n, "chat/room-42").await, 1);
        assert_eq!(count(&n, "*").await, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
