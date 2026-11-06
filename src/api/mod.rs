pub mod blob_get;
pub mod blobs_api;
pub mod events;
pub mod misc;
pub mod openapi;
pub mod routes;

use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use crate::config::Config;
use crate::db::Db;
use crate::store::blobs::UploadGovernor;

/// Shared node state handed to every handler.
pub struct Node {
    pub cfg: Config,
    pub db: Db,
    pub started_at: i64,
    /// Process-wide upload admission (slots, temp bytes, free-space floor).
    pub uploads: Arc<UploadGovernor>,
}

pub type AppState = Arc<Node>;

impl Node {
    pub fn new(cfg: Config, db: Db) -> AppState {
        let uploads = UploadGovernor::from_config(&cfg);
        Arc::new(Node {
            cfg,
            db,
            started_at: crate::store::reaper::now(),
            uploads,
        })
    }
}

/// Live SSE subscriber count. Admission is a CAS against the configured cap.
pub static SSE_ACTIVE: AtomicI64 = AtomicI64::new(0);
