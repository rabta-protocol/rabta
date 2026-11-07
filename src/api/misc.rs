use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::json;

use super::AppState;
use super::routes;
use crate::store::reaper;

fn endpoint(method: &str, path: &str) -> String {
    format!("{method} {}", routes::api_url(path))
}

/// `GET /` — minimal JSON route index; no HTML dashboard.
pub async fn index(State(n): State<AppState>) -> Json<serde_json::Value> {
    let mut endpoints = serde_json::Map::new();
    endpoints.insert(endpoint("GET", routes::HEALTH), json!("liveness probe"));
    endpoints.insert(
        endpoint("GET", routes::STATS),
        json!("storage + eviction counters"),
    );
    endpoints.insert(
        endpoint("GET", routes::OPENAPI),
        json!("OpenAPI 3.1 description of this API"),
    );
    endpoints.insert(
        endpoint("POST", routes::EVENTS),
        json!("publish a signed envelope (application/json)"),
    );
    endpoints.insert(
        endpoint("GET", routes::EVENTS),
        json!("list (collection, label*, owner, limit, cursor)"),
    );
    endpoints.insert(
        endpoint("GET", routes::EVENT_STREAM),
        json!("SSE (Last-Event-ID resume)"),
    );
    endpoints.insert(
        endpoint("POST", routes::EVENT_DELETE),
        json!("owner-signed deletion of stored events"),
    );
    endpoints.insert(
        endpoint("GET", routes::EVENT_BY_ID),
        json!("fetch one (ETag / If-None-Match)"),
    );
    endpoints.insert(
        endpoint("GET", routes::EVENT_SIZE),
        json!("envelope size + attached blob bytes"),
    );
    if n.cfg.blobs_enabled() {
        endpoints.insert(
            endpoint("PUT", routes::BLOBS),
            json!("raw blob upload; identity is the sha256 of the body"),
        );
        endpoints.insert(
            endpoint("GET", routes::BLOB_BY_HASH),
            json!("download a blob (Range / ETag)"),
        );
        endpoints.insert(
            endpoint("HEAD", routes::BLOB_BY_HASH),
            json!("blob metadata"),
        );
    }

    Json(json!({
        "service": "rabta",
        "version": env!("CARGO_PKG_VERSION"),
        "api_version": routes::API_VERSION,
        "base_path": routes::PREFIX,
        "description": "signed-event relay with local content-addressed blobs",
        "routes": endpoints,
    }))
}

/// `GET /api/v1/health` — liveness probe.
pub async fn health(State(n): State<AppState>) -> impl IntoResponse {
    let events =
        n.db.read(|c: &rusqlite::Connection| crate::store::queries::count_live(c))
            .await;
    let blobs =
        n.db.read(|c: &rusqlite::Connection| crate::store::queries::count_live_blobs(c))
            .await;
    (
        StatusCode::OK,
        Json(json!({"status": "ok", "live_events": events, "live_blobs": blobs})),
    )
        .into_response()
}

/// `GET /api/v1/stats` — JSON monitoring (no dashboard).
pub async fn stats(State(n): State<AppState>) -> impl IntoResponse {
    let cfg = n.cfg.clone();
    let now = reaper::now();
    let cfg_stats = cfg.clone();
    let data =
        n.db.read(move |c: &rusqlite::Connection| {
            let events = crate::store::queries::count_live(c);
            let blobs = crate::store::queries::count_live_blobs(c);
            let (event_bytes, claimed_bytes) = crate::db::live_bytes(c);
            let blob_bytes = crate::db::blob_bytes(c);
            let (pinned_events, pinned_blobs) = reaper::whitelist_bytes(c, &cfg_stats);
            let eviction = reaper::eviction_summary(c, &cfg_stats).unwrap_or_default();
            (
                events,
                blobs,
                event_bytes,
                claimed_bytes,
                blob_bytes,
                pinned_events,
                pinned_blobs,
                eviction,
            )
        })
        .await;

    let idx = &reaper::COUNTERS;
    let free = crate::config::free_bytes_public(&cfg.data_dir);
    let last_sweep = reaper::last_sweep_at();
    let body = json!({
        "events": {
            "live": data.0,
            "bytes": data.2,
        },
        "blobs": {
            "enabled": cfg.blobs_enabled(),
            "live": data.1,
            "bytes": data.4,
            "claimed_bytes": data.3,
            "unclaimed_bytes": data.4.saturating_sub(data.3),
            "max_single_bytes": cfg.max_single_blob,
        },
        "storage": {
            "budget_bytes": cfg.watermark_bytes(),
            "target_bytes": cfg.target_bytes(),
            "used_bytes": data.2.saturating_add(data.3),
            "store_bytes": cfg.store,
            "configured_bytes": cfg.store_configured,
            "clamped_to_free": cfg.storage_clamped,
            "free_bytes": free,
            "data_dir": cfg.data_dir.to_string_lossy(),
            "whitelist_keys": cfg.whitelist.len(),
            "whitelist_event_bytes": data.5,
            "whitelist_blob_bytes": data.6,
        },
        "reaper": {
            "interval_s": reaper::REAPER_INTERVAL.as_secs(),
            "last_sweep_at": if last_sweep > 0 { json!(last_sweep) } else { json!(null) },
            "last_sweep_age_s": if last_sweep > 0 {
                json!(now.saturating_sub(last_sweep))
            } else {
                json!(null)
            },
        },
        "eviction": {
            "events_evicted": idx.events_evicted.load(std::sync::atomic::Ordering::Relaxed),
            "blobs_evicted": idx.blobs_evicted.load(std::sync::atomic::Ordering::Relaxed),
            "bytes_reclaimed": idx.bytes_reclaimed.load(std::sync::atomic::Ordering::Relaxed),
            "candidates": {
                "events": data.7.candidates,
                "envelope_bytes": data.7.envelope_bytes,
                "blob_bytes": data.7.blob_bytes,
                "reclaimable_bytes": data.7.reclaimable_bytes(),
                "largest_bytes": data.7.largest_footprint,
            },
        },
        "upload": {
            "inflight_bytes": n.uploads.inflight_bytes(),
            "max_single_bytes": cfg.max_single_blob,
        },
        "sse": {
            "subscribers": super::SSE_ACTIVE.load(std::sync::atomic::Ordering::Relaxed),
            "cap": cfg.sse_subscriber_cap,
        },
        "uptime_s": now - n.started_at,
        "version": env!("CARGO_PKG_VERSION"),
    });
    (StatusCode::OK, Json(body)).into_response()
}
