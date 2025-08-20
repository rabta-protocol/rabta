use rusqlite::{Connection, OptionalExtension, params};

pub const PEER_PENALTY_MIN: f64 = 1.0;
pub const PEER_PENALTY_MAX: f64 = 64.0;
pub const BASE_POLL_MS: u64 = 5_000;

#[derive(Debug, Clone)]
pub struct PeerRecord {
    pub penalty: f64,
    pub strikes: u32,
}

pub fn multiplier_for_strikes(strikes: u32) -> f64 {
    (2.0f64)
        .powi(strikes as i32)
        .clamp(PEER_PENALTY_MIN, PEER_PENALTY_MAX)
}

pub fn poll_interval_ms(penalty: f64) -> u64 {
    (BASE_POLL_MS as f64 * penalty).round() as u64
}

pub fn get_or_create_peer(c: &Connection, url: &str, now: i64) -> rusqlite::Result<PeerRecord> {
    let row = c
        .query_row(
            "SELECT penalty, strikes
             FROM sync_peers WHERE url = ?1",
            [url],
            |r| {
                Ok(PeerRecord {
                    penalty: r.get(0)?,
                    strikes: r.get::<_, f64>(1)? as u32,
                })
            },
        )
        .optional()?;

    match row {
        Some(p) => Ok(p),
        None => {
            c.execute(
                "INSERT INTO sync_peers (url, penalty, strikes, penalty_at, next_poll_at)
                 VALUES (?1, 1.0, 0, ?2, ?2)",
                params![url, now],
            )?;
            Ok(PeerRecord {
                penalty: 1.0,
                strikes: 0,
            })
        }
    }
}

pub fn record_peer_error(
    c: &Connection,
    url: &str,
    now: i64,
    error: &str,
) -> rusqlite::Result<f64> {
    let peer = get_or_create_peer(c, url, now)?;
    let strikes = peer.strikes.saturating_add(1);
    let penalty = multiplier_for_strikes(strikes);
    let next_poll_at = now + (poll_interval_ms(penalty) / 1000) as i64;
    let error: String = error.chars().take(300).collect();

    c.execute(
        "UPDATE sync_peers
         SET strikes = ?1, penalty = ?2, penalty_at = ?3, next_poll_at = ?4,
             last_error = ?5, last_error_at = ?3
         WHERE url = ?6",
        params![strikes, penalty, now, next_poll_at, error, url],
    )?;

    Ok(penalty)
}

pub fn record_peer_clean(c: &Connection, url: &str, now: i64) -> rusqlite::Result<f64> {
    let peer = get_or_create_peer(c, url, now)?;
    let strikes = peer.strikes.saturating_sub(1);
    let penalty = multiplier_for_strikes(strikes);
    let next_poll_at = now + (poll_interval_ms(penalty) / 1000) as i64;

    c.execute(
        "UPDATE sync_peers
         SET strikes = ?1, penalty = ?2, penalty_at = ?3, next_poll_at = ?4, last_success_at = ?3
         WHERE url = ?5",
        params![strikes, penalty, now, next_poll_at, url],
    )?;

    Ok(penalty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exponential_backoff_multipliers() {
        assert_eq!(multiplier_for_strikes(0), 1.0);
        assert_eq!(multiplier_for_strikes(1), 2.0);
        assert_eq!(multiplier_for_strikes(2), 4.0);
        assert_eq!(multiplier_for_strikes(3), 8.0);
        assert_eq!(multiplier_for_strikes(4), 16.0);
        assert_eq!(multiplier_for_strikes(5), 32.0);
        assert_eq!(multiplier_for_strikes(6), 64.0);
        assert_eq!(multiplier_for_strikes(10), 64.0); // capped at MAX
    }

    #[test]
    fn test_poll_interval_pacing() {
        assert_eq!(poll_interval_ms(1.0), 5_000);
        assert_eq!(poll_interval_ms(2.0), 10_000);
        assert_eq!(poll_interval_ms(4.0), 20_000);
        assert_eq!(poll_interval_ms(8.0), 40_000);
    }

    #[test]
    fn peer_error_records_message_and_backoff() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE sync_peers (
                url TEXT PRIMARY KEY,
                penalty REAL NOT NULL DEFAULT 1.0, penalty_at INTEGER NOT NULL DEFAULT 0,
                strikes INTEGER NOT NULL DEFAULT 0, next_poll_at INTEGER NOT NULL DEFAULT 0,
                last_success_at INTEGER, last_error TEXT, last_error_at INTEGER
            );",
        )
        .unwrap();

        let penalty = record_peer_error(&c, "https://peer.example", 1_000, "boom").unwrap();
        assert_eq!(penalty, 2.0);
        let (strikes, error, at): (u32, String, i64) = c
            .query_row(
                "SELECT strikes, last_error, last_error_at FROM sync_peers WHERE url = ?1",
                ["https://peer.example"],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(strikes, 1);
        assert_eq!(error, "boom");
        assert_eq!(at, 1_000);

        record_peer_clean(&c, "https://peer.example", 2_000).unwrap();
        let (strikes, success): (u32, i64) = c
            .query_row(
                "SELECT strikes, last_success_at FROM sync_peers WHERE url = ?1",
                ["https://peer.example"],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(strikes, 0);
        assert_eq!(success, 2_000);
    }
}
