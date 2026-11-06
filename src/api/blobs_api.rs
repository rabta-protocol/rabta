use axum::Json;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::AppState;
use super::events::run;
use super::routes;
use crate::error::{ApiError, ApiResult};
use crate::store::blobs::{self, Abort, BlobSink, Install, Staged};

// ---------------------------------------------------------------------------
// PUT /api/v1/blobs — raw blob upload. The server derives identity from the
// bytes; there is no filename, no extension gate, and no multipart path.
// ---------------------------------------------------------------------------

pub async fn put(State(n): State<AppState>, req: Request<Body>) -> Response {
    run(put_inner(n, req).await)
}

async fn put_inner(n: AppState, req: Request<Body>) -> ApiResult<Response> {
    if !n.cfg.blobs_enabled() {
        return Err(ApiError::blobs_disabled());
    }

    // Early rejection when Content-Length already says "too big".
    if let Some(cl) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if cl > n.cfg.max_single_blob {
            return Err(ApiError::too_large(
                "single blob exceeds 1% of STORAGE_LIMIT",
            ));
        }
    }

    let staged = stage_body(&n, req.into_body()).await?;
    respond_stored(&n, staged).await
}

/// Stream one raw body into a verified temp file. Shared by the upload door
/// and the blob reconciler.
pub(crate) async fn stage_body(n: &AppState, body: Body) -> ApiResult<Staged> {
    let mut sink = BlobSink::start(&n.uploads, &n.cfg.data_dir, n.cfg.max_single_blob).await?;

    let mut stream = body.into_data_stream();
    loop {
        let next = tokio::time::timeout(
            blobs::UPLOAD_IDLE_TIMEOUT,
            futures_util::StreamExt::next(&mut stream),
        )
        .await;
        match next {
            Err(_) => {
                sink.discard();
                return Err(ApiError::timeout());
            }
            Ok(Some(Ok(chunk))) => {
                if let Err(a) = sink.push(&chunk) {
                    sink.discard();
                    return Err(a.into());
                }
            }
            Ok(Some(Err(_))) => {
                sink.discard();
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "bad_body",
                    "request body stream failed",
                ));
            }
            Ok(None) => break,
        }
    }

    if sink.written() == 0 {
        sink.discard();
        return Err(ApiError::empty_body());
    }

    sink.finish().map_err(|a| match a {
        Abort::Io(m) => ApiError::internal(m),
        _ => ApiError::internal("finalize failed"),
    })
}

/// Finalize the public blob door: one accept-or-refuse transaction, then
/// materialize the file. A duplicate refreshes access metadata only; the
/// response always carries the blob's current deadline.
pub async fn respond_stored(n: &AppState, staged: Staged) -> ApiResult<Response> {
    let hash_hex = staged.hash_hex.clone();
    let hash = staged.hash;
    let size = staged.size;

    let outcome = blobs::install_staged(&n.db, &n.cfg, staged)
        .await
        .map_err(|e| ApiError::internal(format!("blob row: {e}")))?;

    if outcome == Install::Refused {
        return Err(ApiError::at_watermark());
    }

    let expires_at: i64 =
        n.db.read(move |c: &rusqlite::Connection| {
            c.query_row(
                "SELECT expires_at FROM blobs WHERE hash = ?1",
                [&hash[..]],
                |r| r.get(0),
            )
            .unwrap_or(0)
        })
        .await;

    let status = if outcome == Install::Fresh {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let body = json!({
        "hash": hash_hex,
        "size": size,
        "url": routes::blob_url(&hash_hex),
        "expires_at": expires_at,
    });
    let mut resp = (status, Json(body)).into_response();
    if status == StatusCode::CREATED {
        resp.headers_mut().insert(
            header::LOCATION,
            routes::blob_url(&hash_hex).parse().expect("location"),
        );
    }
    Ok(resp)
}
