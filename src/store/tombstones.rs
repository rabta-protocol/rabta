use rusqlite::{Connection, params};

use crate::config::{MAX_TOMBSTONE_INGEST, TOMBSTONE_WINDOW_S};

/// One tombstone entry as carried on the sync wire: the event digest and the
/// unix second of the eviction that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub id: [u8; 32],
    pub deleted_at: i64,
}

/// Record a pressure eviction: the event stops being offered and re-pulled
/// until `deleted_at + TOMBSTONE_WINDOW_S`. The first eviction wins — a
/// re-entry never extends the window.
pub fn record_evicted(c: &Connection, ids: &[[u8; 32]], now: i64) -> rusqlite::Result<()> {
    for id in ids {
        c.execute(
            "INSERT INTO deleted_events (id, deleted_at) VALUES (?1, ?2)
             ON CONFLICT(id) DO NOTHING",
            params![&id[..], now],
        )?;
    }
    Ok(())
}

/// Register tombstones a peer sent us. Returns the number of newly registered
/// entries (capped at `MAX_TOMBSTONE_INGEST`; excess is dropped, not queued).
/// Sanity rules: `deleted_at` must be positive and not in the future, and an
/// entry older than the window is dropped as already-expired. A repeat entry
/// keeps the later of the two timestamps.
pub fn ingest_from_peer(
    c: &Connection,
    entries: &[Tombstone],
    now: i64,
) -> rusqlite::Result<usize> {
    let mut registered = 0;
    for entry in entries {
        if entry.deleted_at <= 0
            || entry.deleted_at > now
            || entry.deleted_at <= now - TOMBSTONE_WINDOW_S
        {
            continue;
        }
        let known: Option<i64> = c
            .query_row(
                "SELECT deleted_at FROM deleted_events WHERE id = ?1",
                params![&entry.id[..]],
                |r| r.get(0),
            )
            .ok();
        match known {
            Some(existing) => {
                if entry.deleted_at > existing {
                    c.execute(
                        "UPDATE deleted_events SET deleted_at = ?1 WHERE id = ?2",
                        params![entry.deleted_at, &entry.id[..]],
                    )?;
                }
            }
            None => {
                if registered >= MAX_TOMBSTONE_INGEST {
                    continue;
                }
                c.execute(
                    "INSERT INTO deleted_events (id, deleted_at) VALUES (?1, ?2)",
                    params![&entry.id[..], entry.deleted_at],
                )?;
                registered += 1;
            }
        }
    }
    Ok(registered)
}

/// True while the event is locally suppressed. Expired rows are purged by the
/// reaper, so no timestamp comparison is needed here.
pub fn is_active(c: &Connection, id: &[u8; 32]) -> bool {
    c.query_row(
        "SELECT 1 FROM deleted_events WHERE id = ?1",
        params![&id[..]],
        |_| Ok(()),
    )
    .is_ok()
}

/// Drop tombstones past the window; those events may be re-offered again.
pub fn purge_expired(c: &Connection, now: i64) -> rusqlite::Result<usize> {
    c.execute(
        "DELETE FROM deleted_events WHERE deleted_at <= ?1 - ?2",
        params![now, TOMBSTONE_WINDOW_S],
    )
}

/// Live tombstone count for /api/v1/stats.
pub fn active_count(c: &Connection) -> i64 {
    c.query_row("SELECT COUNT(*) FROM deleted_events", [], |r| r.get(0))
        .unwrap_or(0)
}

/// Oldest active tombstones, oldest first, for the sync wire.
pub fn oldest_active(c: &Connection, limit: usize) -> rusqlite::Result<Vec<Tombstone>> {
    let mut stmt = c.prepare(
        "SELECT id, deleted_at FROM deleted_events ORDER BY deleted_at ASC, id ASC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        let raw: Vec<u8> = r.get(0)?;
        let deleted_at: i64 = r.get(1)?;
        Ok((raw, deleted_at))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (raw, deleted_at) = row?;
        if let Ok(id) = raw.try_into() {
            out.push(Tombstone { id, deleted_at });
        }
    }
    Ok(out)
}

/// Wire shape: hex event digest plus the eviction timestamp.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TombstoneWire {
    pub id: String,
    pub deleted_at: i64,
}

impl TombstoneWire {
    pub fn to_wire(t: &Tombstone) -> Self {
        TombstoneWire {
            id: hex::encode(t.id),
            deleted_at: t.deleted_at,
        }
    }

    /// Decode and sanity-check one wire entry.
    pub fn parse(&self) -> Option<Tombstone> {
        let id = hex::decode(&self.id).ok()?;
        let id: [u8; 32] = id.try_into().ok()?;
        if self.deleted_at <= 0 {
            return None;
        }
        Some(Tombstone {
            id,
            deleted_at: self.deleted_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(crate::db::SCHEMA).unwrap();
        c
    }

    fn id(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn recorded_tombstone_is_active_until_purged() {
        let c = conn();
        let now = 10_000;
        record_evicted(&c, &[id(1)], now).unwrap();
        assert!(is_active(&c, &id(1)));
        purge_expired(&c, now + TOMBSTONE_WINDOW_S - 1).unwrap();
        assert!(is_active(&c, &id(1)), "window not yet over");
        purge_expired(&c, now + TOMBSTONE_WINDOW_S).unwrap();
        assert!(!is_active(&c, &id(1)), "tombstone expired");
    }

    #[test]
    fn first_eviction_wins_the_window() {
        let c = conn();
        record_evicted(&c, &[id(2)], 1_000).unwrap();
        record_evicted(&c, &[id(2)], 50_000).unwrap();
        let at: i64 = c
            .query_row("SELECT deleted_at FROM deleted_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(at, 1_000);
    }

    #[test]
    fn ingest_drops_future_stale_and_keeps_later_timestamp() {
        let c = conn();
        let now = 10_000;
        record_evicted(&c, &[id(3)], 5_000).unwrap();

        let entries = vec![
            Tombstone {
                id: id(4),
                deleted_at: now + 10, // future: dropped
            },
            Tombstone {
                id: id(5),
                deleted_at: now - TOMBSTONE_WINDOW_S, // stale: dropped
            },
            Tombstone {
                id: id(6),
                deleted_at: 0, // invalid: dropped
            },
            Tombstone {
                id: id(3),
                deleted_at: 8_000, // known, later: updates
            },
            Tombstone {
                id: id(7),
                deleted_at: 9_000, // new: registered
            },
        ];
        let n = ingest_from_peer(&c, &entries, now).unwrap();
        assert_eq!(n, 1, "only id(7) was newly registered");

        let at3: i64 = c
            .query_row(
                "SELECT deleted_at FROM deleted_events WHERE id = ?1",
                [&id(3)[..]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(at3, 8_000, "later peer timestamp wins");
    }

    #[test]
    fn ingest_is_capped_per_cycle() {
        let c = conn();
        let now = 10_000;
        let entries: Vec<Tombstone> = (0..MAX_TOMBSTONE_INGEST + 50)
            .map(|i| {
                let mut id_ = [0u8; 32];
                id_[..4].copy_from_slice(&(i as u32).to_be_bytes());
                Tombstone {
                    id: id_,
                    deleted_at: now,
                }
            })
            .collect();
        let n = ingest_from_peer(&c, &entries, now).unwrap();
        assert_eq!(n, MAX_TOMBSTONE_INGEST);
        assert_eq!(active_count(&c), MAX_TOMBSTONE_INGEST as i64);
    }

    #[test]
    fn oldest_active_orders_and_limits() {
        let c = conn();
        record_evicted(&c, &[id(1), id(2)], 100).unwrap();
        record_evicted(&c, &[id(3)], 50).unwrap();
        let all = oldest_active(&c, crate::config::MAX_SYNC_TOMBSTONES).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].id, id(3));
        let two = oldest_active(&c, 2).unwrap();
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].id, id(3));
    }

    #[test]
    fn wire_roundtrip() {
        let t = Tombstone {
            id: id(9),
            deleted_at: 1234,
        };
        let w = TombstoneWire::to_wire(&t);
        assert_eq!(w.parse().unwrap(), t);
        assert!(
            TombstoneWire {
                id: "nothex".into(),
                deleted_at: 1
            }
            .parse()
            .is_none()
        );
    }
}
