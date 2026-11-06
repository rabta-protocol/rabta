mod api;
mod canon;
mod config;
mod db;
mod error;
mod store;

use std::net::SocketAddr;

use axum::Router;
use axum::routing::{get, head, post, put};
use tower_http::cors::CorsLayer;

use api::AppState;
use api::routes;

fn data_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".to_string()))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let dir = data_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        panic!("cannot create data dir {}: {e}", dir.display());
    }

    let cfg = config::Config::load();
    let port = cfg.port;

    if cfg.blobs_enabled()
        && let Err(e) = store::blobs::init_dirs(&dir)
    {
        panic!("cannot initialize blob layout: {e}");
    }

    let database = db::Db::open(&dir);

    // Blob plane boot hygiene: drop crashed temp files and orphan bytes.
    if cfg.blobs_enabled()
        && let Err(e) = store::blobs::boot_hygiene(&database, cfg.data_dir.clone()).await
    {
        tracing::error!("blob boot hygiene: {e}");
    }

    // Apply expiry + watermark immediately instead of waiting one interval.
    store::reaper::reaper_tick(&database, cfg.clone()).await;

    let state = api::Node::new(cfg.clone(), database.clone());

    if !cfg.whitelist.is_empty() {
        tracing::info!(
            keys = cfg.whitelist.len(),
            "whitelisted owners are exempt from pressure eviction"
        );
    }

    let app = router(state.clone());

    // Reaper: expiry sweep (Layer 1) + pressure cleanup every 5 minutes.
    {
        let db = database.clone();
        let rcfg = cfg.clone();
        tokio::spawn(async move {
            let start = tokio::time::Instant::now() + store::reaper::REAPER_INTERVAL;
            let mut ticker = tokio::time::interval_at(start, store::reaper::REAPER_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                store::reaper::reaper_tick(&db, rcfg.clone()).await;
            }
        });
    }

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    tracing::info!("rabta listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");
    tracing::info!("shutdown complete");
}

fn router(state: AppState) -> Router {
    let big = axum::extract::DefaultBodyLimit::disable();

    let v1 = Router::new()
        .route(routes::HEALTH, get(api::misc::health))
        .route(routes::STATS, get(api::misc::stats))
        .route(routes::OPENAPI, get(api::openapi::serve))
        .route(routes::EVENTS, post(api::events::publish).layer(big))
        .route(routes::EVENTS, get(api::events::list))
        .route(routes::EVENT_STREAM, get(api::events::stream))
        .route(routes::EVENT_BY_ID, get(api::events::get_one))
        .route(routes::EVENT_SIZE, get(api::events::size))
        .route(routes::BLOBS, put(api::blobs_api::put).layer(big))
        .route(routes::BLOB_BY_HASH, get(api::blob_get::get))
        .route(routes::BLOB_BY_HASH, head(api::blob_get::head));

    Router::new()
        .route("/", get(api::misc::index))
        .nest(routes::PREFIX, v1)
        .with_state(state)
        .layer(CorsLayer::permissive())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
    tracing::info!("shutdown signal received");
}
