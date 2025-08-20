use std::time::Duration;

use super::client::{SyncClient, SyncRequest};
use super::range::{RangeSummary, compute_range_summary, get_adaptive_ranges};
use crate::api::AppState;
use crate::sync::battery::{self, EVENT_COST, SYNC_BATTERY_MAX, SYNC_FLOOR};

pub async fn run_peer_worker(
    state: AppState,
    peer_url: String,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    let client = SyncClient::new(state.identity.clone());
    let mut poll_delay = Duration::from_secs(5);
    let mut pending_push_ids: Vec<[u8; 32]> = Vec::new();
    let mut paused_logged = false;
    // Windows the peer asked us to re-examine, as (start, end) bounds only.
    // The peer's own counts/digests are discarded: they must be re-computed
    // against LOCAL state, or the peer would see them as a match and serve
    // nothing, dead-ending bisection.
    let mut suggested_windows: Vec<(i64, i64)> = Vec::new();

    tracing::info!(peer = %peer_url, "started sync worker for peer");

    loop {
        tokio::select! {
            _ = tokio::time::sleep(poll_delay) => {},
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    tracing::info!(peer = %peer_url, "sync worker shutting down");
                    break;
                }
            }
        }

        let now = crate::store::reaper::now();

        // 0. Auto-disable: while our battery is below the sync floor
        //    (half the tank), skip the pass entirely — no push, no pull,
        //    no blob transfers. Resume once the battery regenerates past
        //    floor + exit margin.
        let regen = state.cfg.battery_regen_per_min;
        let battery_now = state
            .db
            .write(move |c| battery::refill_battery(c, now, SYNC_BATTERY_MAX, regen))
            .await
            .ok();
        if let Some(b) = battery_now {
            if !b.sync_admitted() {
                if !paused_logged {
                    paused_logged = true;
                    tracing::info!(
                        peer = %peer_url,
                        battery = b.remaining,
                        floor = SYNC_FLOOR,
                        "battery below sync floor; outbound sync paused"
                    );
                }
                poll_delay = Duration::from_secs(10);
                continue;
            }
            if paused_logged {
                paused_logged = false;
                tracing::info!(peer = %peer_url, "battery recovered; outbound sync resumed");
            }
        }

        // 1. Get peer record from DB
        let peer_url_clone = peer_url.clone();
        let peer_record = state
            .db
            .write(move |c| super::penalty::get_or_create_peer(c, &peer_url_clone, now))
            .await;

        let _peer_record = match peer_record {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(peer = %peer_url, error = %e, "cannot get or create peer record");
                poll_delay = Duration::from_secs(10);
                continue;
            }
        };

        // 2. Fetch local root and the summaries for the windows to query:
        //    either the peer-suggested sub-windows re-computed locally, or
        //    our own adaptive partition.
        let (root, query_ranges) = state
            .db
            .read({
                let suggested_windows = suggested_windows.clone();
                move |c| {
                    let root = compute_range_summary(c, now, 0, i64::MAX).unwrap_or(RangeSummary {
                        start: 0,
                        end: i64::MAX,
                        count: 0,
                        digest: "0000000000000000".into(),
                    });
                    let windows: Vec<(i64, i64)> = if suggested_windows.is_empty() {
                        get_adaptive_ranges(c, now, 16)
                            .unwrap_or_else(|_| vec![root.clone()])
                            .iter()
                            .map(|s| (s.start, s.end))
                            .collect()
                    } else {
                        suggested_windows
                    };
                    let mut ranges = Vec::with_capacity(windows.len());
                    for (start, end) in windows.iter().take(32) {
                        if let Ok(s) = compute_range_summary(c, now, *start, *end) {
                            ranges.push(s);
                        }
                    }
                    if ranges.is_empty() {
                        ranges.push(root.clone());
                    }
                    (root, ranges)
                }
            })
            .await;

        // 3. Collect events to push
        let ids_to_push = std::mem::take(&mut pending_push_ids);
        let push_events = state
            .db
            .read(move |c| {
                let mut events = Vec::new();
                if !ids_to_push.is_empty()
                    && let Ok(evs) = super::range::fetch_events_by_ids(c, now, &ids_to_push)
                {
                    events.extend(evs);
                }

                // If fewer than 50 events queued, also push the most recent events
                if events.len() < 50 {
                    let limit = 50 - events.len();
                    if let Ok(recent) =
                        super::range::fetch_events_in_range(c, now, 0, i64::MAX, limit)
                    {
                        for ev in recent {
                            if !events.contains(&ev) {
                                events.push(ev);
                            }
                        }
                    }
                }
                events
            })
            .await;

        let (epoch_hex, local_tombstones) = {
            state
                .db
                .read(|c| {
                    let ep: Option<Vec<u8>> = c
                        .query_row("SELECT epoch FROM sync_meta WHERE id = 1", [], |r| r.get(0))
                        .ok();
                    let epoch = ep.map(hex::encode).unwrap_or_else(|| "0".repeat(32));
                    let tombs = crate::store::tombstones::oldest_active(
                        c,
                        crate::config::MAX_SYNC_TOMBSTONES,
                    )
                    .unwrap_or_default()
                    .iter()
                    .map(crate::store::tombstones::TombstoneWire::to_wire)
                    .collect();
                    (epoch, tombs)
                })
                .await
        };

        let req = SyncRequest {
            epoch: epoch_hex,
            root,
            ranges: query_ranges,
            push_events,
            tombstones: local_tombstones,
            request_id: format!("req-{}", now),
        };

        // 4. Send sync request to peer
        match client.send_sync(&peer_url, &req, now).await {
            Ok(resp) => {
                let peer_battery_remaining = resp.battery.remaining;
                let peer_battery_max = resp.battery.max;
                let next_poll_ms = resp.peer.next_poll_after_ms.max(1000);
                poll_delay = Duration::from_millis(next_poll_ms);

                // Ingest pulled events (including any missing/deleted events the peer served!)
                let events_to_ingest = resp.pull_events;
                let peer_tombstones = resp.tombstones.clone();
                let state_clone = state.clone();
                let peer_url_for_db = peer_url.clone();

                let (_, quota_stopped) = state
                    .db
                    .write(move |c| {
                        // Register the peer's evictions first, so the tombstone
                        // guard below also covers this response's pull_events.
                        let received = peer_tombstones
                            .iter()
                            .filter_map(|t| t.parse())
                            .collect::<Vec<_>>();
                        if !received.is_empty()
                            && let Ok(n) =
                                crate::store::tombstones::ingest_from_peer(c, &received, now)
                        {
                            crate::store::reaper::record_tombstones_received(n as u64);
                        }

                        let battery_now = battery::refill_battery(c, now, SYNC_BATTERY_MAX, regen)?;
                        let mut quota_stopped = !battery_now.sync_admitted();
                        let mut newly_accepted = 0;

                        if !quota_stopped {
                            for ev_val in events_to_ingest {
                                let Ok(bytes) = serde_json::to_vec(&ev_val) else {
                                    continue;
                                };
                                let Ok(parsed) =
                                    crate::store::events::parse_and_verify(&bytes, now)
                                else {
                                    continue;
                                };

                                if state_clone.dedup.is_duplicate(&parsed.id) {
                                    continue;
                                }

                                // Suppressed: locally evicted (or peer-reported
                                // evicted). No battery charge, no insert.
                                if crate::store::tombstones::is_active(c, &parsed.id) {
                                    crate::store::reaper::record_tombstone_skip(1);
                                    continue;
                                }

                                if battery::reserve_battery(
                                    c,
                                    EVENT_COST,
                                    now,
                                    SYNC_BATTERY_MAX,
                                    regen,
                                )
                                .is_err()
                                {
                                    quota_stopped = true;
                                    break;
                                }

                                match crate::store::events::insert_event(c, &parsed) {
                                    Ok(true) => {
                                        state_clone.dedup.record(parsed.id);
                                        let _ = super::blob::enqueue_missing_blobs(
                                            c,
                                            &parsed.blobs,
                                            now,
                                        );
                                        newly_accepted += 1;
                                    }
                                    Ok(false) => {
                                        state_clone.dedup.record(parsed.id);
                                    }
                                    Err(_) => {}
                                }
                            }
                        }

                        let _ = c.execute(
                            "UPDATE sync_peers SET last_success_at = ?1 WHERE url = ?2",
                            rusqlite::params![now, peer_url_for_db],
                        );

                        let _ = c.execute(
                            "INSERT INTO peer_battery (url, remaining, max, updated_at)
                             VALUES (?1, ?2, ?3, ?4)
                             ON CONFLICT(url) DO UPDATE SET
                               remaining = excluded.remaining,
                               max = excluded.max,
                               updated_at = excluded.updated_at",
                            rusqlite::params![
                                peer_url_for_db,
                                peer_battery_remaining,
                                peer_battery_max,
                                now
                            ],
                        );

                        let _ = super::penalty::record_peer_clean(c, &peer_url_for_db, now);
                        Ok::<(i32, bool), rusqlite::Error>((newly_accepted, quota_stopped))
                    })
                    .await
                    .unwrap_or((0, true));

                if quota_stopped {
                    poll_delay = poll_delay.max(Duration::from_secs(15));
                }

                // If peer reported known IDs in divergent ranges, check what we have that peer lacks
                if !resp.known_ids.is_empty() {
                    let known_hex_set = resp.known_ids.clone();
                    let missing_on_peer = state
                        .db
                        .read(move |c| {
                            let mut needed = Vec::new();
                            let all_local =
                                super::range::fetch_event_ids_in_range(c, now, 0, i64::MAX, 200)
                                    .unwrap_or_default();
                            for id in all_local {
                                let hex_id = hex::encode(id);
                                if !known_hex_set.contains(&hex_id) {
                                    needed.push(id);
                                }
                            }
                            needed
                        })
                        .await;
                    pending_push_ids.extend(missing_on_peer);
                }

                // Peer-suggested sub-ranges: keep only the window bounds; the
                // next pass re-computes local summaries for them.
                suggested_windows = resp.sub_ranges.iter().map(|s| (s.start, s.end)).collect();

                // Download pending blobs from peer
                let blob_hashes = resp.missing_blobs;
                if !quota_stopped {
                    let pending_blobs = state
                        .db
                        .read(move |c| {
                            super::blob::next_pending_blobs(c, 2, now).unwrap_or_default()
                        })
                        .await;

                    for item in pending_blobs {
                        let hash = item.hash;
                        let _ = state
                            .db
                            .write(move |c| super::blob::mark_blob_downloading(c, &hash))
                            .await;

                        let dl_res = super::blob::download_blob(
                            client.http(),
                            &peer_url,
                            &item.hash_hex,
                            &state.cfg.data_dir,
                            state.cfg.max_single_blob,
                        )
                        .await;

                        match dl_res {
                            Ok(tmp_path) => {
                                let size = tokio::fs::metadata(&tmp_path)
                                    .await
                                    .map(|m| m.len())
                                    .unwrap_or(0);
                                let staged =
                                    crate::store::blobs::Staged::new(item.hash, size, tmp_path);
                                match crate::store::blobs::install_staged(
                                    &state.db,
                                    &state.cfg,
                                    staged,
                                    battery::BLOB_COST,
                                )
                                .await
                                {
                                    Ok(crate::store::blobs::Install::Quota { .. }) => {
                                        let h = item.hash;
                                        let _ = state
                                            .db
                                            .write(move |c| {
                                                super::blob::mark_blob_failed(c, &h, now)
                                            })
                                            .await;
                                        break;
                                    }
                                    Ok(crate::store::blobs::Install::NotEncrypted) => {
                                        tracing::warn!(peer = %peer_url, hash = %item.hash_hex, "peer served non-encrypted blob; refused");
                                        let h = item.hash;
                                        let _ = state
                                            .db
                                            .write(move |c| {
                                                super::blob::mark_blob_failed(c, &h, now)
                                            })
                                            .await;
                                    }
                                    Ok(_) => {
                                        let h = item.hash;
                                        let _ = state
                                            .db
                                            .write(move |c| super::blob::mark_blob_completed(c, &h))
                                            .await;
                                    }
                                    Err(e) => {
                                        tracing::warn!(peer = %peer_url, hash = %item.hash_hex, error = %e, "install downloaded blob failed");
                                        let h = item.hash;
                                        let _ = state
                                            .db
                                            .write(move |c| {
                                                super::blob::mark_blob_failed(c, &h, now)
                                            })
                                            .await;
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(peer = %peer_url, hash = %item.hash_hex, error = %e, "blob download failed");
                                let h = item.hash;
                                let _ = state
                                    .db
                                    .write(move |c| super::blob::mark_blob_failed(c, &h, now))
                                    .await;
                            }
                        }
                    }
                }

                // Upload missing blobs to peer if we have them locally
                for missing_hex in blob_hashes.iter().take(2) {
                    let _ = super::blob::upload_blob(
                        client.http(),
                        &peer_url,
                        missing_hex,
                        &state.cfg.data_dir,
                    )
                    .await;
                }
            }
            Err(e) => {
                tracing::warn!(peer = %peer_url, error = %e, "sync call failed");
                let peer_url_err = peer_url.clone();
                let err_text = e.to_string();
                let new_penalty = state
                    .db
                    .write(move |c| {
                        super::penalty::record_peer_error(c, &peer_url_err, now, &err_text)
                            .unwrap_or(2.0)
                    })
                    .await;

                let backoff_ms = super::penalty::poll_interval_ms(new_penalty);
                poll_delay = Duration::from_millis(backoff_ms);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;
    use tokio::net::TcpListener;

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("rabta-worker-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_test_event(
        key: &SigningKey,
        collection: &str,
        now: i64,
    ) -> (serde_json::Value, [u8; 32]) {
        let created_at = now - 10;
        let expires_at = now + 3600;
        let owner = format!("ed25519:{}", hex::encode(key.verifying_key().to_bytes()));
        let data = serde_json::json!({"msg": "replication test"});
        let canonical =
            crate::canon::digest_value(collection, created_at, expires_at, &owner, &[], &data, &[]);
        let digest: [u8; 32] = Sha256::digest(crate::canon::canonical_value(&canonical)).into();
        let sig = key.sign(&digest);
        let val = serde_json::json!({
            "collection": collection,
            "created_at": created_at,
            "data": data,
            "expires_at": expires_at,
            "labels": [],
            "blobs": [],
            "owner": owner,
            "id": hex::encode(digest),
            "sig": hex::encode(sig.to_bytes()),
        });
        (val, digest)
    }

    #[tokio::test]
    async fn test_two_nodes_sync_and_repull_deleted_event() {
        let dir_a = temp_dir("node-a");
        let dir_b = temp_dir("node-b");

        let cfg_a = crate::config::Config::test_cfg(&dir_a, 10_000_000);
        let identity_a = crate::identity::NodeIdentity::generate();
        let state_a = crate::api::Node::new(cfg_a, crate::db::Db::open(&dir_a), identity_a);

        let cfg_b = crate::config::Config::test_cfg(&dir_b, 10_000_000);
        let identity_b = crate::identity::NodeIdentity::generate();
        let state_b = crate::api::Node::new(cfg_b, crate::db::Db::open(&dir_b), identity_b);

        // Bind servers
        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_a = listener_a.local_addr().unwrap();
        let url_a = format!("http://{}", addr_a);

        let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_b = listener_b.local_addr().unwrap();
        let url_b = format!("http://{}", addr_b);

        let app_a = crate::router(state_a.clone());
        let app_b = crate::router(state_b.clone());

        tokio::spawn(async move {
            axum::serve(listener_a, app_a).await.ok();
        });
        tokio::spawn(async move {
            axum::serve(listener_b, app_b).await.ok();
        });

        let (shutdown_tx, shutdown_rx_b) = tokio::sync::watch::channel(false);
        let (_shutdown_tx_a, shutdown_rx_a) = tokio::sync::watch::channel(false);

        // Run worker on Node B syncing from Node A
        let state_b_clone = state_b.clone();
        let url_a_clone = url_a.clone();
        tokio::spawn(async move {
            run_peer_worker(state_b_clone, url_a_clone, shutdown_rx_b).await;
        });

        // Run worker on Node A syncing from Node B
        let state_a_clone = state_a.clone();
        let url_b_clone = url_b.clone();
        tokio::spawn(async move {
            run_peer_worker(state_a_clone, url_b_clone, shutdown_rx_a).await;
        });

        let now = crate::store::reaper::now();
        let event_key = SigningKey::from_bytes(&[7u8; 32]);
        let (event_val, event_id) = make_test_event(&event_key, "chat", now);

        // 1. Insert event on Node A
        let bytes = serde_json::to_vec(&event_val).unwrap();
        let parsed = crate::store::events::parse_and_verify(&bytes, now).unwrap();
        state_a
            .db
            .write(move |c| crate::store::events::insert_event(c, &parsed))
            .await
            .unwrap();

        // 2. Wait for event to replicate from Node A to Node B
        let mut replicated = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let exists = state_b
                .db
                .read(move |c| crate::store::events::event_exists(c, &event_id).unwrap_or(false))
                .await;
            if exists {
                replicated = true;
                break;
            }
        }
        assert!(replicated, "event should replicate from Node A to Node B");

        // 3. Delete event on Node A (simulating disk eviction / data loss on Node A)
        let ev_id_copy = event_id;
        state_a
            .db
            .write(move |c| c.execute("DELETE FROM events WHERE id = ?1", [&ev_id_copy[..]]))
            .await
            .unwrap();

        let exists_on_a = state_a
            .db
            .read(move |c| crate::store::events::event_exists(c, &event_id).unwrap_or(false))
            .await;
        assert!(!exists_on_a, "event must be deleted from Node A");

        // 4. Wait for Node A to repull the missing event from Node B via range reconciliation!
        let mut repulled = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let exists = state_a
                .db
                .read(move |c| crate::store::events::event_exists(c, &event_id).unwrap_or(false))
                .await;
            if exists {
                repulled = true;
                break;
            }
        }
        assert!(
            repulled,
            "Node A should automatically repull deleted event from Node B"
        );

        let _ = shutdown_tx.send(true);
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    /// Auto-disable: with the battery drained below the sync floor the
    /// worker must not pull, and must resume once the battery refills.
    #[tokio::test]
    async fn test_outbound_sync_pauses_below_sync_floor() {
        let dir_a = temp_dir("node-pa");
        let dir_b = temp_dir("node-pb");

        let cfg_a = crate::config::Config::test_cfg(&dir_a, 10_000_000);
        let identity_a = crate::identity::NodeIdentity::generate();
        let state_a = crate::api::Node::new(cfg_a, crate::db::Db::open(&dir_a), identity_a);

        let cfg_b = crate::config::Config::test_cfg(&dir_b, 10_000_000);
        let identity_b = crate::identity::NodeIdentity::generate();
        let state_b = crate::api::Node::new(cfg_b, crate::db::Db::open(&dir_b), identity_b);

        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_a = listener_a.local_addr().unwrap();
        let url_a = format!("http://{}", addr_a);
        let app_a = crate::router(state_a.clone());
        tokio::spawn(async move {
            axum::serve(listener_a, app_a).await.ok();
        });

        let (shutdown_tx, shutdown_rx_b) = tokio::sync::watch::channel(false);
        let state_b_clone = state_b.clone();
        let url_a_clone = url_a.clone();
        tokio::spawn(async move {
            run_peer_worker(state_b_clone, url_a_clone, shutdown_rx_b).await;
        });

        let now = crate::store::reaper::now();
        let event_key = SigningKey::from_bytes(&[13u8; 32]);
        let (event_val, event_id) = make_test_event(&event_key, "chat", now);

        let bytes = serde_json::to_vec(&event_val).unwrap();
        let parsed = crate::store::events::parse_and_verify(&bytes, now).unwrap();
        state_a
            .db
            .write(move |c| crate::store::events::insert_event(c, &parsed))
            .await
            .unwrap();

        // Drain node B's battery below the sync floor before its first pass.
        state_b
            .db
            .write(move |c| {
                c.execute(
                    "INSERT INTO sync_meta (id, epoch, battery, battery_at, tier, created_at)
                     VALUES (1, x'00000000000000000000000000000000', 400000, ?1, 0, ?1)
                     ON CONFLICT(id) DO UPDATE SET battery = 400000, battery_at = ?1, tier = 0",
                    [now],
                )
            })
            .await
            .unwrap();

        // First pass fires ~5 s in; while paused it must store nothing.
        tokio::time::sleep(Duration::from_secs(7)).await;
        let exists_paused = state_b
            .db
            .read(move |c| crate::store::events::event_exists(c, &event_id).unwrap_or(false))
            .await;
        assert!(
            !exists_paused,
            "worker must not pull while the battery is below the sync floor"
        );

        // Refill past floor + exit margin; the next pass must pull the event.
        state_b
            .db
            .write(move |c| {
                c.execute(
                    "UPDATE sync_meta SET battery = 1000000, battery_at = 0 WHERE id = 1",
                    [],
                )
            })
            .await
            .unwrap();

        let mut pulled = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let exists = state_b
                .db
                .read(move |c| crate::store::events::event_exists(c, &event_id).unwrap_or(false))
                .await;
            if exists {
                pulled = true;
                break;
            }
        }
        assert!(
            pulled,
            "worker must resume pulling once the battery recovers"
        );

        let _ = shutdown_tx.send(true);
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    /// The eviction scenario end-to-end: a pressure-evicted event is
    /// tombstoned, so the peer cannot sync it back within the window; once the
    /// tombstones expire, reconciliation restores it.
    #[tokio::test]
    async fn test_two_nodes_tombstoned_event_is_not_repulled_until_window_ends() {
        let dir_a = temp_dir("node-ta");
        let dir_b = temp_dir("node-tb");

        let cfg_a = crate::config::Config::test_cfg(&dir_a, 10_000_000);
        let identity_a = crate::identity::NodeIdentity::generate();
        let state_a = crate::api::Node::new(cfg_a, crate::db::Db::open(&dir_a), identity_a);

        let cfg_b = crate::config::Config::test_cfg(&dir_b, 10_000_000);
        let identity_b = crate::identity::NodeIdentity::generate();
        let state_b = crate::api::Node::new(cfg_b, crate::db::Db::open(&dir_b), identity_b);

        let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_a = listener_a.local_addr().unwrap();
        let url_a = format!("http://{}", addr_a);

        let listener_b = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_b = listener_b.local_addr().unwrap();
        let url_b = format!("http://{}", addr_b);

        let app_a = crate::router(state_a.clone());
        let app_b = crate::router(state_b.clone());

        tokio::spawn(async move {
            axum::serve(listener_a, app_a).await.ok();
        });
        tokio::spawn(async move {
            axum::serve(listener_b, app_b).await.ok();
        });

        let (shutdown_tx, shutdown_rx_b) = tokio::sync::watch::channel(false);
        let (_shutdown_tx_a, shutdown_rx_a) = tokio::sync::watch::channel(false);

        let state_b_clone = state_b.clone();
        let url_a_clone = url_a.clone();
        tokio::spawn(async move {
            run_peer_worker(state_b_clone, url_a_clone, shutdown_rx_b).await;
        });

        let state_a_clone = state_a.clone();
        let url_b_clone = url_b.clone();
        tokio::spawn(async move {
            run_peer_worker(state_a_clone, url_b_clone, shutdown_rx_a).await;
        });

        let now = crate::store::reaper::now();
        let event_key = SigningKey::from_bytes(&[11u8; 32]);
        let (event_val, event_id) = make_test_event(&event_key, "chat", now);

        // 1. Insert event on Node A, wait for replication to Node B.
        let bytes = serde_json::to_vec(&event_val).unwrap();
        let parsed = crate::store::events::parse_and_verify(&bytes, now).unwrap();
        state_a
            .db
            .write(move |c| crate::store::events::insert_event(c, &parsed))
            .await
            .unwrap();

        let mut replicated = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let exists = state_b
                .db
                .read(move |c| crate::store::events::event_exists(c, &event_id).unwrap_or(false))
                .await;
            if exists {
                replicated = true;
                break;
            }
        }
        assert!(replicated, "event should replicate from Node A to Node B");

        // 2. Pressure-evict on Node A: delete + tombstone, one transaction.
        let ev_id = event_id;
        state_a
            .db
            .write(move |c| {
                c.execute("DELETE FROM events WHERE id = ?1", [&ev_id[..]])?;
                crate::store::tombstones::record_evicted(c, std::slice::from_ref(&ev_id), now)
            })
            .await
            .unwrap();

        // 3. Several worker cycles must NOT bring it back.
        let mut came_back = false;
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let exists = state_a
                .db
                .read(move |c| crate::store::events::event_exists(c, &event_id).unwrap_or(false))
                .await;
            if exists {
                came_back = true;
                break;
            }
        }
        assert!(
            !came_back,
            "tombstoned event must not sync back within the window"
        );

        // 4. Simulate the 32h window ending: purge tombstones on both nodes,
        // then reconciliation must restore the event.
        let now_purge = now + crate::config::TOMBSTONE_WINDOW_S;
        let id_a = event_id;
        let id_b = event_id;
        state_a
            .db
            .write(move |c| crate::store::tombstones::purge_expired(c, now_purge))
            .await
            .unwrap();
        state_b
            .db
            .write(move |c| crate::store::tombstones::purge_expired(c, now_purge))
            .await
            .unwrap();

        let mut restored = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let exists = state_a
                .db
                .read(move |c| crate::store::events::event_exists(c, &id_a).unwrap_or(false))
                .await;
            if exists {
                restored = true;
                break;
            }
        }
        let b_still_has = state_b
            .db
            .read(move |c| crate::store::events::event_exists(c, &id_b).unwrap_or(false))
            .await;
        assert!(
            restored && b_still_has,
            "event must return via reconciliation after the window"
        );

        let _ = shutdown_tx.send(true);
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }
}
