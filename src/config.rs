use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// Admission line inside the combined budget.
const PLANE_WATERMARK_PCT: u64 = 90;
/// Post-eviction target inside the combined budget.
const PLANE_TARGET_PCT: u64 = 85;
/// A single blob may not exceed this share of the budget.
const MAX_SINGLE_BLOB_PCT: u64 = 1;
/// Default budget for events plus claimed blobs.
const DEFAULT_STORE_BYTES: u64 = 20 * 1024 * 1024 * 1024;
/// Maximum number of pinned owners honoured from `WHITELIST_PUBKEYS`.
pub const MAX_WHITELIST_PUBKEYS: usize = 64;

/// Node configuration, resolved once at boot.
///
/// Precedence: process environment first,
/// then the optional `/data/config.env` file, then built-in defaults.
/// Invalid values refuse to boot — never a silent fallback.
#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    /// Effective combined budget in bytes: events plus blobs claimed by a
    /// live event. Unclaimed blobs are outside it until their 48-hour expiry.
    pub store: u64,
    /// Unclamped value the operator asked for (diagnostics only).
    pub store_configured: u64,
    /// Whether the boot-time clamp to free disk space reduced the budget.
    pub storage_clamped: bool,
    /// Max accepted single blob upload = 1% of the effective budget.
    pub max_single_blob: u64,
    pub sse_subscriber_cap: usize,
    /// Owners whose events, and the blobs those events claim, are exempt from
    /// pressure eviction. TTL expiry still applies.
    pub whitelist: HashSet<[u8; 32]>,
}

impl Config {
    /// Admission line: new charged bytes are refused above this.
    pub fn watermark_bytes(&self) -> u64 {
        pct(self.store, PLANE_WATERMARK_PCT)
    }

    /// Post-eviction target.
    pub fn target_bytes(&self) -> u64 {
        pct(self.store, PLANE_TARGET_PCT)
    }

    /// Blobs are always available; the combined budget is always positive.
    pub fn blobs_enabled(&self) -> bool {
        self.store > 0
    }

    /// Free-space floor kept for SQLite/WAL and the operator: 5% of the
    /// budget, never less than 16 MiB.
    pub fn free_floor_bytes(&self) -> u64 {
        (self.store / 20).max(16 * 1024 * 1024)
    }
}

fn pct(n: u64, p: u64) -> u64 {
    n.saturating_mul(p) / 100
}

impl Config {
    pub fn load() -> Config {
        let data_dir = PathBuf::from(env_or("DATA_DIR", "/data"));
        let env_file_settings = read_env_file(data_dir.join("config.env"));

        let port: u16 = var_or_err("PORT", &env_file_settings)
            .map(|s| parse_port(&s))
            .unwrap_or(3232);

        let store_configured = var_or_err("STORAGE_LIMIT", &env_file_settings)
            .map(|s| {
                parse_size(&s)
                    .expect("STORAGE_LIMIT: invalid size value (examples: 20GB, 512MiB, 52428800)")
            })
            .unwrap_or(DEFAULT_STORE_BYTES);

        if store_configured == 0 {
            panic!("STORAGE_LIMIT must be greater than zero");
        }

        // Boot-time clamp: never promise more than 90% of free disk.
        let free = free_bytes(&data_dir).unwrap_or(u64::MAX);
        let ninety_pct_free = pct(free, 90);
        let (store, storage_clamped) = if free != u64::MAX && store_configured > ninety_pct_free {
            tracing::warn!(
                "STORAGE_LIMIT={} but only {} free on {}; clamping to {}",
                fmt_bytes(store_configured),
                fmt_bytes(free),
                data_dir.display(),
                fmt_bytes(ninety_pct_free)
            );
            (ninety_pct_free, true)
        } else {
            (store_configured, false)
        };

        if store == 0 {
            panic!("STORAGE_LIMIT clamped to zero: not enough free disk space");
        }

        let max_single_blob = pct(store, MAX_SINGLE_BLOB_PCT);

        let sse_subscriber_cap: usize = var_or_err("SSE_SUBSCRIBER_CAP", &env_file_settings)
            .map(|s| s.parse().expect("SSE_SUBSCRIBER_CAP: invalid integer"))
            .unwrap_or(256);

        let whitelist = resolve_whitelist(var_or_err("WHITELIST_PUBKEYS", &env_file_settings));

        Config {
            port,
            data_dir,
            store,
            store_configured,
            storage_clamped,
            max_single_blob,
            sse_subscriber_cap,
            whitelist,
        }
    }
}

fn var_or_err(name: &str, env_file: &HashMap<String, String>) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        return Some(v);
    }
    env_file.get(name).cloned()
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// `/data/config.env`: `KEY=VALUE` lines, `#` comments, blank lines ignored.
/// Never executed as a shell script; pure textual key/value parsing.
fn read_env_file(path: PathBuf) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(body) = std::fs::read_to_string(&path) else {
        return map;
    };
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        map.insert(k.trim().to_string(), v.trim().to_string());
    }
    if !map.is_empty() {
        tracing::info!("loaded {} settings from {}", map.len(), path.display());
    }
    map
}

fn parse_port(s: &str) -> u16 {
    s.parse::<u16>().expect("PORT: invalid port number")
}

/// Resolve `WHITELIST_PUBKEYS` at the environment boundary: absent, empty, or
/// whitespace-only means nothing is pinned. Any other value is parsed strictly
/// and refuses boot when malformed.
fn resolve_whitelist(raw: Option<String>) -> HashSet<[u8; 32]> {
    match raw {
        None => HashSet::new(),
        Some(s) if s.trim().is_empty() => HashSet::new(),
        Some(s) => parse_whitelist(&s).unwrap_or_else(|e| panic!("WHITELIST_PUBKEYS: {e}")),
    }
}

/// `WHITELIST_PUBKEYS` parsing: comma-separated Ed25519 public keys, each bare
/// 64-hex or `ed25519:<64-hex>`, case-insensitive, duplicates collapsed, at
/// most 64 unique keys.
pub fn parse_whitelist(s: &str) -> Result<HashSet<[u8; 32]>, String> {
    let mut keys = HashSet::new();
    for raw in s.split(',') {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let hex_part = raw
            .strip_prefix("ed25519:")
            .or_else(|| raw.strip_prefix("ED25519:"))
            .unwrap_or(raw);
        if hex_part.len() != 64 || !hex_part.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "bad entry `{raw}`: expected 64 hex characters, optionally prefixed with `ed25519:`"
            ));
        }
        let bytes = hex::decode(hex_part).map_err(|e| format!("bad entry `{raw}`: {e}"))?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| format!("bad entry `{raw}`: expected 32 bytes"))?;
        ed25519_dalek::VerifyingKey::from_bytes(&key)
            .map_err(|_| format!("bad entry `{raw}`: not a valid Ed25519 public key"))?;
        keys.insert(key);
        if keys.len() > MAX_WHITELIST_PUBKEYS {
            return Err(format!(
                "more than {MAX_WHITELIST_PUBKEYS} unique keys listed"
            ));
        }
    }
    Ok(keys)
}

/// Human-readable size parsing: `20MB`, `20GB`, `20TB`, `512MiB`, plain bytes,
/// lowercase, `KB`/`m/G` variants. `M`/`G` without `i` = binary (2^20 / 2^30).
pub fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_uppercase();
    let (num_part, mult) = if let Some(rest) = t.strip_suffix("KIB") {
        (rest, 1024u64)
    } else if let Some(rest) = t.strip_suffix("MIB") {
        (rest, 1024u64 * 1024)
    } else if let Some(rest) = t.strip_suffix("GIB") {
        (rest, 1024u64 * 1024 * 1024)
    } else if let Some(rest) = t.strip_suffix("TIB") {
        (rest, 1024u64 * 1024 * 1024 * 1024)
    } else if let Some(rest) = t.strip_suffix("KB") {
        (rest, 1024)
    } else if let Some(rest) = t.strip_suffix("MB") {
        (rest, 1024 * 1024)
    } else if let Some(rest) = t.strip_suffix("GB") {
        (rest, 1024 * 1024 * 1024)
    } else if let Some(rest) = t.strip_suffix("TB") {
        (rest, 1024 * 1024 * 1024 * 1024)
    } else if let Some(rest) = t.strip_suffix('K') {
        (rest, 1024)
    } else if let Some(rest) = t.strip_suffix('M') {
        (rest, 1024 * 1024)
    } else if let Some(rest) = t.strip_suffix('G') {
        (rest, 1024 * 1024 * 1024)
    } else if let Some(rest) = t.strip_suffix('T') {
        (rest, 1024 * 1024 * 1024 * 1024)
    } else {
        (t.as_str(), 1u64)
    };
    let raw = num_part.trim();
    if raw.is_empty() {
        return Err(format!("no number in `{s}`"));
    }
    let f: f64 = raw.parse().map_err(|_| format!("not a number: `{raw}`"))?;
    if !f.is_finite() || f < 0.0 || f > 1.0e18 {
        return Err(format!("size out of range: `{s}`"));
    }
    let scaled = (f * mult as f64).floor();
    // f64::MAX and NaN would both saturate on cast; reject anything that does
    // not fit u64 instead of silently storing a bogus budget.
    if !scaled.is_finite() || scaled >= u64::MAX as f64 {
        return Err(format!("size overflows u64: `{s}`"));
    }
    Ok(scaled as u64)
}

/// Available bytes on the device holding `dir`. Public wrapper reused by /api/v1/stats.
pub fn free_bytes_public(dir: &std::path::Path) -> u64 {
    free_bytes(dir).unwrap_or(0)
}

/// Available bytes on the device holding `dir`, or `None` when it cannot be
/// determined. Used by blob uploads to fail closed on the disk floor.
pub(crate) fn free_bytes_opt(dir: &std::path::Path) -> Option<u64> {
    free_bytes(dir)
}

/// Available bytes on the device holding `dir`. Computed on demand.
fn free_bytes(dir: &std::path::Path) -> Option<u64> {
    // Make sure the path exists (data dir is created before config load in main).
    std::fs::create_dir_all(dir).ok()?;
    let cdir = std::ffi::CString::new(dir.to_str()?).ok()?;
    unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(cdir.as_ptr(), &mut st) != 0 {
            return None;
        }
        let avail = st.f_bavail as u64;
        let bsize = st.f_bsize as u64;
        Some(avail.saturating_mul(bsize))
    }
}

pub fn fmt_bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n}B")
    } else {
        format!("{v:.1}{}", units[i])
    }
}

#[cfg(test)]
impl Config {
    /// Build a deterministic config for unit tests.
    pub fn test_cfg(data_dir: impl Into<PathBuf>, store: u64) -> Config {
        Config {
            port: 0,
            data_dir: data_dir.into(),
            store,
            store_configured: store,
            storage_clamped: false,
            max_single_blob: pct(store, MAX_SINGLE_BLOB_PCT),
            sse_subscriber_cap: 1,
            whitelist: HashSet::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(parse_size("20GB").unwrap(), 20 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("20gb").unwrap(), 20 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("512MiB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size("52428800").unwrap(), 52_428_800);
        assert_eq!(parse_size("2k").unwrap(), 2048);
        assert_eq!(parse_size("1.5MB").unwrap(), 1_572_864); // floored
        assert_eq!(parse_size("20MB").unwrap(), 20 * 1024 * 1024);
        assert!(parse_size("20").is_ok());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("").is_err());
        assert!(parse_size("NaN").is_err());
        assert!(parse_size("inf").is_err());
        assert!(parse_size("1e309").is_err());
        assert!(parse_size("18446744073709551616").is_err());
        assert!(parse_size("1e18TB").is_err());
    }

    #[test]
    fn combined_budget_watermarks() {
        let cfg = Config::test_cfg("/tmp/rabta-config-test", 100);
        assert_eq!(cfg.watermark_bytes(), 90);
        assert_eq!(cfg.target_bytes(), 85);
        assert_eq!(cfg.max_single_blob, 1);
        assert_eq!(cfg.store, 100);
    }

    #[test]
    fn proportional_clamp_preserves_the_ratio() {
        assert_eq!(pct(1000, 90), 900);
        assert_eq!(pct(0, 90), 0);
    }

    fn key_hex(seed: u8) -> String {
        let sk = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
        hex::encode(sk.verifying_key().to_bytes())
    }

    #[test]
    fn blank_whitelist_pins_nothing() {
        assert!(resolve_whitelist(None).is_empty());
        assert!(resolve_whitelist(Some(String::new())).is_empty());
        assert!(resolve_whitelist(Some("   ".into())).is_empty());
        assert!(parse_whitelist("").unwrap().is_empty());
    }

    #[test]
    fn whitelist_accepts_bare_and_prefixed_keys() {
        let bare = key_hex(1);
        let prefixed = format!("ed25519:{}", key_hex(2).to_ascii_uppercase());
        let keys = parse_whitelist(&format!(" {bare} , {prefixed} ")).unwrap();
        assert_eq!(keys.len(), 2);
        let expected: [u8; 32] = hex::decode(&bare).unwrap().try_into().unwrap();
        assert!(keys.contains(&expected));
    }

    #[test]
    fn whitelist_collapses_duplicates() {
        let k = key_hex(3);
        let keys = parse_whitelist(&format!("{k},{},ed25519:{k}", k.to_uppercase())).unwrap();
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn whitelist_rejects_malformed_entries() {
        assert!(parse_whitelist("abc").is_err());
        assert!(parse_whitelist(&"z".repeat(64)).is_err());
        assert!(parse_whitelist("ed25519:").is_err());
        // 32 bytes with no valid compressed-Ed25519 decompression.
        let invalid = (0u8..=255)
            .map(|b| [b; 32])
            .find(|k| ed25519_dalek::VerifyingKey::from_bytes(k).is_err())
            .expect("some byte pattern must be an invalid point");
        assert!(parse_whitelist(&hex::encode(invalid)).is_err());
    }

    #[test]
    fn whitelist_is_capped_at_64_keys() {
        let ok = (0..64u8).map(key_hex).collect::<Vec<_>>().join(",");
        assert_eq!(parse_whitelist(&ok).unwrap().len(), 64);

        let too_many = (0..65u8).map(key_hex).collect::<Vec<_>>().join(",");
        assert!(parse_whitelist(&too_many).is_err());
    }

    #[test]
    #[should_panic(expected = "WHITELIST_PUBKEYS")]
    fn malformed_whitelist_refuses_boot() {
        resolve_whitelist(Some("not-a-key".into()));
    }
}
