use rusqlite::{Connection, OptionalExtension, params};

/// Sync request budget. Hardcoded — not a config knob.
pub const SYNC_BATTERY_MAX: f64 = 1_000_000.0;

// Ingress cost model. Local submissions and pulls are receiver-chosen
// (1x); a remote push carries a consent surcharge that the per-peer
// penalty multiplier escalates on abuse.
pub const EVENT_COST: f64 = 1.0;
pub const BLOB_COST: f64 = 100.0;
pub const PUSH_MULTIPLIER: f64 = 10.0;

// Read-side serve protection (per sync request).
pub const REQUEST_COST: f64 = 1.0;
pub const RANGE_EVAL_COST: f64 = 0.2;
pub const EVENT_SERVE_COST: f64 = 1.0;

// Below the sync floor sync is disabled in both directions: inbound sync
// requests are refused and outbound passes pause; local writes keep working.
// Below the read-only floor nothing writes.
pub const SYNC_FLOOR: f64 = SYNC_BATTERY_MAX * 0.75;
pub const READONLY_FLOOR: f64 = BLOB_COST;
// A degraded tier is left only this far above its entry threshold.
pub const TIER_EXIT_MARGIN: f64 = 1_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    ReadWrite,
    SyncDisabled,
    ReadOnly,
}

impl Tier {
    pub fn from_db(v: i64) -> Self {
        match v {
            1 => Tier::SyncDisabled,
            2 => Tier::ReadOnly,
            _ => Tier::ReadWrite,
        }
    }

    pub fn as_db(self) -> i64 {
        match self {
            Tier::ReadWrite => 0,
            Tier::SyncDisabled => 1,
            Tier::ReadOnly => 2,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::ReadWrite => "readwrite",
            Tier::SyncDisabled => "sync_disabled",
            Tier::ReadOnly => "read_only",
        }
    }

    /// Degrade as soon as a floor is crossed; recover only after the
    /// battery climbs TIER_EXIT_MARGIN back above it.
    pub fn current(remaining: f64, stored: Tier) -> Tier {
        match stored {
            Tier::ReadWrite => {
                if remaining < READONLY_FLOOR {
                    Tier::ReadOnly
                } else if remaining < SYNC_FLOOR {
                    Tier::SyncDisabled
                } else {
                    Tier::ReadWrite
                }
            }
            Tier::SyncDisabled => {
                if remaining < READONLY_FLOOR {
                    Tier::ReadOnly
                } else if remaining >= SYNC_FLOOR + TIER_EXIT_MARGIN {
                    Tier::ReadWrite
                } else {
                    Tier::SyncDisabled
                }
            }
            Tier::ReadOnly => {
                if remaining >= SYNC_FLOOR + TIER_EXIT_MARGIN {
                    Tier::ReadWrite
                } else if remaining >= READONLY_FLOOR + TIER_EXIT_MARGIN {
                    Tier::SyncDisabled
                } else {
                    Tier::ReadOnly
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BatteryState {
    pub remaining: f64,
    pub regen_per_min: f64,
    pub tier: Tier,
}

impl BatteryState {
    /// Seconds until the battery regenerates past `floor + exit margin`.
    pub fn retry_after_to(&self, floor: f64) -> u64 {
        let target = floor + TIER_EXIT_MARGIN;
        let deficit = (target - self.remaining).max(0.0);
        let per_sec = (self.regen_per_min / 60.0).max(0.1);
        (deficit / per_sec).ceil() as u64
    }

    /// Sync ingress (push or pull) and outbound passes are admitted only at
    /// full tier.
    pub fn sync_admitted(&self) -> bool {
        self.tier == Tier::ReadWrite
    }

    /// Local writes work below the sync floor, until read-only.
    pub fn local_admitted(&self) -> bool {
        self.tier != Tier::ReadOnly
    }
}

/// Work performed by one reconcile request: the events it pushes, ranges checked,
/// and pull events served, priced with the peer's penalty multiplier.
#[derive(Debug, Clone, Copy)]
pub struct ReconcileWork {
    pub plain_events: usize,
    pub blob_events: usize,
    pub ranges_evaluated: usize,
    pub events_served: usize,
}

impl ReconcileWork {
    pub fn compute_cost(&self, penalty: f64) -> f64 {
        let read_work = (self.ranges_evaluated as f64 * RANGE_EVAL_COST)
            + (self.events_served as f64 * EVENT_SERVE_COST);
        let write_work =
            ((self.plain_events + self.blob_events) as f64 * EVENT_COST * PUSH_MULTIPLIER)
                * penalty;
        (REQUEST_COST + read_work + write_work).ceil()
    }
}

/// Re-evaluate and persist the tier after a battery mutation.
fn update_tier(c: &Connection, remaining: f64, stored: i64) -> rusqlite::Result<Tier> {
    let tier = Tier::current(remaining, Tier::from_db(stored));
    if tier.as_db() != stored {
        c.execute(
            "UPDATE sync_meta SET tier = ?1 WHERE id = 1",
            params![tier.as_db()],
        )?;
    }
    Ok(tier)
}

/// Refill the global battery lazily based on elapsed seconds.
pub fn refill_battery(
    c: &Connection,
    now: i64,
    max_battery: f64,
    regen_per_min: f64,
) -> rusqlite::Result<BatteryState> {
    let row: Option<(f64, i64, i64)> = c
        .query_row(
            "SELECT battery, battery_at, tier FROM sync_meta WHERE id = 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let (mut battery, battery_at, stored_tier) = match row {
        Some((b, at, tier)) => (b, at, tier),
        None => {
            let mut epoch = [0u8; 16];
            getrandom::getrandom(&mut epoch).expect("random epoch");
            c.execute(
                "INSERT INTO sync_meta (id, epoch, battery, battery_at, created_at)
                 VALUES (1, ?1, ?2, ?3, ?3)",
                params![&epoch[..], max_battery, now],
            )?;
            (max_battery, now, 0)
        }
    };

    let elapsed = (now - battery_at).max(0) as f64;
    let regen_per_sec = regen_per_min / 60.0;
    battery = (battery + elapsed * regen_per_sec).min(max_battery);

    c.execute(
        "UPDATE sync_meta SET battery = ?1, battery_at = ?2 WHERE id = 1",
        params![battery, now],
    )?;

    let tier = update_tier(c, battery, stored_tier)?;

    Ok(BatteryState {
        remaining: battery,
        regen_per_min,
        tier,
    })
}

/// Reserve battery for a request. Returns Ok(cost) if battery was sufficient,
/// or Err(retry_after_seconds) if insufficient.
pub fn reserve_battery(
    c: &Connection,
    cost: f64,
    now: i64,
    max_battery: f64,
    regen_per_min: f64,
) -> Result<f64, u64> {
    let state = refill_battery(c, now, max_battery, regen_per_min).map_err(|_| 1u64)?;

    if state.remaining < cost {
        let deficit = cost - state.remaining;
        let regen_per_sec = (regen_per_min / 60.0).max(0.1);
        let retry_after = (deficit / regen_per_sec).ceil() as u64;
        return Err(retry_after.max(1));
    }

    let remaining = state.remaining - cost;
    c.execute(
        "UPDATE sync_meta SET battery = ?1, battery_at = ?2 WHERE id = 1",
        params![remaining, now],
    )
    .map_err(|_| 1u64)?;
    update_tier(c, remaining, state.tier.as_db()).map_err(|_| 1u64)?;

    Ok(cost)
}

/// Refund unused reservation when a request ends up processing less work.
pub fn refund_battery(
    c: &Connection,
    refund_amount: f64,
    max_battery: f64,
) -> rusqlite::Result<()> {
    if refund_amount <= 0.0 {
        return Ok(());
    }
    c.execute(
        "UPDATE sync_meta SET battery = MIN(?1, battery + ?2) WHERE id = 1",
        params![max_battery, refund_amount],
    )?;
    let (remaining, stored): (f64, i64) = c.query_row(
        "SELECT battery, tier FROM sync_meta WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    update_tier(c, remaining, stored)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconcile_cost_scales_with_penalty() {
        let work = ReconcileWork {
            plain_events: 10,
            blob_events: 2,
            ranges_evaluated: 5,
            events_served: 2,
        };
        // write work = 12 x 1 x 10x = 120; read work = 5 x 0.2 + 2 x 1 = 3.0
        // cost = ceil(1.0 + 3.0 + 120 * penalty)
        assert_eq!(work.compute_cost(1.0), 124.0); // 1 + 3 + 120
        assert_eq!(work.compute_cost(2.0), 244.0); // 1 + 3 + 240
        assert_eq!(work.compute_cost(4.0), 484.0); // 1 + 3 + 480
    }

    #[test]
    fn test_tier_degrades_immediately() {
        assert_eq!(Tier::current(1_000_000.0, Tier::ReadWrite), Tier::ReadWrite);
        assert_eq!(Tier::current(750_000.0, Tier::ReadWrite), Tier::ReadWrite);
        assert_eq!(
            Tier::current(749_999.9, Tier::ReadWrite),
            Tier::SyncDisabled
        );
        assert_eq!(Tier::current(100.0, Tier::ReadWrite), Tier::SyncDisabled);
        assert_eq!(Tier::current(99.9, Tier::ReadWrite), Tier::ReadOnly);
    }

    #[test]
    fn test_tier_hysteresis_on_recovery() {
        // Disabled does not recover at the floor, only above floor+margin.
        assert_eq!(
            Tier::current(750_500.0, Tier::SyncDisabled),
            Tier::SyncDisabled
        );
        assert_eq!(
            Tier::current(751_000.0, Tier::SyncDisabled),
            Tier::ReadWrite
        );
        // Read-only recovers stepwise: first to disabled, then to read-write.
        assert_eq!(Tier::current(150.0, Tier::ReadOnly), Tier::ReadOnly);
        assert_eq!(Tier::current(1_100.0, Tier::ReadOnly), Tier::SyncDisabled);
        assert_eq!(Tier::current(5_000.0, Tier::ReadOnly), Tier::SyncDisabled);
        assert_eq!(Tier::current(751_000.0, Tier::ReadOnly), Tier::ReadWrite);
    }

    #[test]
    fn test_retry_after_targets_floor_plus_margin() {
        let state = BatteryState {
            remaining: 0.0,
            regen_per_min: 600.0,
            tier: Tier::ReadOnly,
        };
        // to sync floor: 751_000 / 10 per sec
        assert_eq!(state.retry_after_to(SYNC_FLOOR), 75_100);
        // to read-only exit: 1_100 / 10
        assert_eq!(state.retry_after_to(READONLY_FLOOR), 110);
    }
}
