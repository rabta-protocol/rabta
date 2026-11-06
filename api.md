# Rabta API reference

**Protocol version:** 1 · **API style:** HTTP + JSON + SSE · **Authentication:** none

This is the detailed wire reference for Rabta: every route, the event envelope format, the signing rules, and local blob storage.

New to Rabta? Start with the [README](README.md) for the one-container quick start. For a machine-readable route index, call `GET /` on a running node.

## Contents

- [Relay model, not Nostr](#relay-model-not-nostr)
- [Conventions](#conventions)
- [Errors](#errors)
- [Cryptography and the event envelope](#cryptography-and-the-event-envelope)
- [Public routes](#public-routes)
  - [`GET /`](#get-)
  - [`GET /api/v1/openapi.json`](#get-apiv1openapijson)
  - [`GET /api/v1/health`](#get-apiv1health)
  - [`GET /api/v1/stats`](#get-apiv1stats)
  - [`POST /api/v1/events`](#post-apiv1events)
  - [`GET /api/v1/events`](#get-apiv1events)
  - [`GET /api/v1/events/{id}`](#get-apiv1eventsid)
  - [`GET /api/v1/events/{id}/size`](#get-apiv1eventsidsize)
  - [`GET /api/v1/events/stream`](#get-apiv1eventsstream)
  - [`PUT /api/v1/blobs`](#put-apiv1blobs)
  - [`GET /api/v1/blobs/{hash}`](#get-apiv1blobshash)
  - [`HEAD /api/v1/blobs/{hash}`](#head-apiv1blobshash)
- [Blob plane](#blob-plane)
- [Limits and runtime configuration](#limits-and-runtime-configuration)
- [Client examples](#client-examples)

## Relay model, not Nostr

Rabta has the shape of a relay — signed immutable events, a pubkey identity, an append-only sequence, live delivery — but it is not a Nostr relay and does not implement NIPs.

- **Transport:** HTTP + JSON + SSE, not WebSocket `REQ` / `EVENT` / `EOSE`.
- **Signatures:** Ed25519 over a canonical Rabta object, not secp256k1 Schnorr over a Nostr event array.
- **Model:** no `kind`, no `tags`, no replaceable or ephemeral events, no NIP-09 deletion, no NIP-42 auth, no NIP-50 search.
- **Attachments:** raw uploads are stored on the node's own disk, addressed by SHA-256. An unclaimed blob lives 48 hours; a blob named in a live event's `bin` expires with that event.

A Nostr client cannot talk to Rabta without an adapter. See [Rabta vs. Nostr relays](README.md#rabta-vs-nostr-relays) for the side-by-side comparison.

## Conventions

| Topic | Rule |
| --- | --- |
| Base URL | `http://<host>:3232` by default. All routes are on one port. |
| Request/response bodies | JSON, except raw blob uploads and the SSE stream. |
| Timestamps | Unix seconds, integers. |
| Event IDs | 64 lowercase hex characters = SHA-256 of the canonical envelope fields. |
| Blob identity | 64 lowercase hex characters = SHA-256 of the raw body. |
| Hex input | Accepted case-insensitively; responses and stored IDs are lowercase. |
| Authentication | None. Every route is unauthenticated. |
| CORS | Permissive (`Access-Control-Allow-Origin: *`, all methods/headers). |
| Versioning | No URL version prefix. |
| Relay compatibility | Not Nostr-compatible; no NIPs are implemented. See [Relay model, not Nostr](#relay-model-not-nostr). |
| HTML | None. A UI is a separate static app that calls the node. |

Because there is no authentication, anyone who can reach the node can publish events and upload blobs. The `owner` field is a cryptographic identity, not an access-control check. Put the node behind TLS and an authenticating reverse proxy if it is exposed to the internet.

## Errors

Every handled failure returns the same JSON shape:

```json
{
  "error": "human-readable message",
  "code": "machine_readable_code"
}
```

The HTTP status and `code` identify the class of failure:

| `code` | Status | Meaning |
| --- | --- | --- |
| `bad_json` | 400 | Malformed JSON, missing/invalid body shape, or a bad cursor. |
| `bad_sig` | 401 | Missing/invalid `id`, `sig`, or `owner`; digest mismatch; signature verification failed. |
| `bad_collection` | 400 | `collection` does not match `^[a-z0-9/_-]{1,32}$`. |
| `bad_label` | 400 | Invalid envelope label, or empty/duplicate label filter. |
| `bad_bin` | 400 | `bin` is not an array of 1–8 unique 64-char lowercase hex hashes. |
| `bad_owner` | 400 | `owner` filter is not `ed25519:<64 hex>`. |
| `ttl_range` | 400 | Timestamp or TTL outside the allowed window. |
| `too_large` | 413 | Event body exceeds 100,000 bytes, or a blob exceeds 1% of `STORAGE_LIMIT`. |
| `empty_body` | 400 | Blob upload body is empty. |
| `bad_body` | 400 | The blob upload stream failed partway through the body. |
| `at_watermark` | 507 | Storage admission watermark reached; write refused. |
| `disk_full` | 507 | Free space is below the safety floor. |
| `busy` | 429 | Upload slots or temp-file budget are exhausted; retry shortly. |
| `timeout` | 408 | Upload body stalled between chunks. |
| `subscriber_cap` | 503 | SSE subscriber limit reached. |
| `not_found` | 404 | Unknown, expired, or malformed resource ID. |
| `internal` | 500 | Storage or filesystem failure. |

## Cryptography and the event envelope

### The short version

1. Clients own encryption and signing.
2. The node verifies **Ed25519** signatures and stores the envelope opaquely.
3. Event identity is a **SHA-256** hash; blob identity is the **SHA-256 of the raw bytes**.
4. The signed digest is computed over **canonical JSON** with sorted keys and compact separators.
5. Clients may encrypt `data` and blob bytes before upload. The server never needs plaintext.

> **Not a Nostr event.** There is no `kind`, `tags`, or `content`, and the signature is Ed25519 over the canonical object below, not secp256k1 Schnorr over a Nostr array. Rabta does not implement NIP-01, NIP-09, NIP-40, NIP-42, or NIP-50. See [Relay model, not Nostr](#relay-model-not-nostr).

### What the server can see

Rabta is zero-knowledge in the practical sense: it has no accounts, no application schema, and never decrypts payloads. It does see envelope metadata — public key, collection, labels, timestamps, sizes, blob-reference hashes, and access timing. It also stores blob bytes on its own disk, under a content hash. Encrypt client-side when that metadata matters.

The server does **not**:

- generate or hold user keys;
- encrypt or decrypt `data` or blobs;
- sign events on behalf of clients;
- enforce per-owner read/write permissions.

Authenticity comes from the client signature. Confidentiality, if you need it, comes from client-side encryption of `data` and blob bodies.

### Envelope fields

```json
{
  "collection": "chat/room-42",
  "created_at": 1793283000,
  "expires_at": 1824732600,
  "owner": "ed25519:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
  "labels": ["demo", "docs"],
  "data": {
    "ciphertext": "base64-or-any-client-format",
    "nonce": "..."
  },
  "id": "8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a",
  "sig": "e5bc0571cf814a9c0bdafa595532e9c4793b0b1343e563ec9951b6be4d7a078a373c7f2728b47f32c03ee6d0235a20a315487777e17956314b0b3e415ed1f903"
}
```

| Field | Required | Type | Rules |
| --- | --- | --- | --- |
| `collection` | yes | string | `^[a-z0-9/_-]{1,32}$`. Lowercase only. |
| `created_at` | yes | integer | Unix seconds. Must not be in the server's future. |
| `expires_at` | yes | integer | Unix seconds. Must be in the future and no more than 10 years after `created_at`. |
| `owner` | yes | string | `ed25519:` + 64 hex chars (32-byte public key). |
| `labels` | yes | array of strings | At most 10. Each matches `^[A-Za-z0-9:_\.\-\+\/]{1,64}$`. Order is preserved. |
| `bin` | no | array of strings | 1–8 blob hashes, each exactly 64 lowercase hex chars. Duplicates rejected. Order is preserved. Reference only: see [Blob references](#blob-references). |
| `data` | yes | JSON object | Opaque to the server. Any JSON object. |
| `id` | yes | string | 64 hex chars. Must equal the computed digest. |
| `sig` | yes | string | 128 hex chars. Ed25519 signature over the 32-byte digest. |

Unknown fields are accepted, ignored, and not persisted.

The stored envelope is the canonical object containing the digest fields plus lowercase `id` and `sig`. Comments, whitespace, key order, and extra fields from the upload are not preserved.

### Canonical JSON

The digest input is exactly the object:

```text
{bin?, collection, created_at, data, expires_at, labels, owner}
```

`bin` is present only when the envelope carried it; events without `bin` digest exactly as before. `id` and `sig` are never part of the digest input. Blobs are a separate plane: an event never uploads or embeds blob bytes.

Canonicalization rules:

- object keys sorted lexicographically, recursively;
- compact separators: `,` and `:`, no spaces;
- UTF-8 bytes;
- arrays keep their order (including `labels` and `bin`);
- numbers must be integers in the digest fields (`created_at`, `expires_at`).

A Rust-compatible canonical encoding can be written as:

```js
function canonical(value) {
  if (Array.isArray(value)) {
    return `[${value.map(canonical).join(',')}]`;
  }
  if (value && typeof value === 'object') {
    return `{${Object.keys(value)
      .sort()
      .map((k) => `${JSON.stringify(k)}:${canonical(value[k])}`)
      .join(',')}}`;
  }
  return JSON.stringify(value);
}
```

**Number warning:** avoid floating-point numbers in `data` if clients are written in different languages. JavaScript prints `1.0` as `1`, while Rust may print `1.0`, producing different canonical bytes and a different digest. Use integers and strings for values that must round-trip exactly.

### Identity and signature

```text
canonical_bytes = canonical_json({bin?, collection, created_at, data, expires_at, labels, owner})
digest          = SHA-256(canonical_bytes)          // 32 bytes
id              = lowercase_hex(digest)             // 64 chars
sig             = Ed25519_sign(owner_private_key, digest)  // 64 bytes
```

Important details:

- The signature is over the **32-byte SHA-256 digest**, not over the canonical JSON bytes directly.
- The public key is the raw 32-byte Ed25519 verifying key, hex-encoded after `ed25519:`.
- The owner string used in the digest is normalized to lowercase hex.
- `id` and `sig` are hex-encoded and stored lowercase.
- Ed25519 is deterministic: the same key and digest always produce the same signature.

Verification order inside the node: body cap → JSON shape → TTL window → canonical digest → `id` cross-check → signature verification → transactional insert.

### Blob references

`bin` lets an event advertise the blobs it uses. Event envelopes and claimed blobs share one storage budget:

- Each entry is a SHA-256 hash of a blob uploaded through `PUT /api/v1/blobs`.
- The server validates shape only. It never checks existence.
- A blob referenced by a live event expires with the **latest** expiry among its live referencing events. Multiple events can claim the same blob; the longest-lived one wins.
- A blob with no live references is unclaimed and expires **48 hours after its first upload**. Reads and re-uploads do not extend that window.
- Only claimed blobs count toward `STORAGE_LIMIT`. An unclaimed upload is outside the budget until an event references it.
- A dangling reference is valid. Storage pressure can still evict a claimed blob early, exactly as it can evict an unexpired event; events are evicted before claimed blobs. The exception is a blob claimed by a live event from a `WHITELIST_PUBKEYS` owner, which is never a pressure victim.
- `bin` is not indexed or filterable; there is no `GET /api/v1/events?bin=...`. The server does keep an internal refs table to calculate retention.

Upload the blob first, then put its hash in the event's `bin` array.

### Worked example

Digest input, byte-for-byte:

```text
{"collection":"docs/example","created_at":1793283000,"data":{"ciphertext":"base64-or-any-client-format","nonce":"..."},"expires_at":1824732600,"labels":["demo","docs"],"owner":"ed25519:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c"}
```

SHA-256 of those bytes:

```text
8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a
```

Signature over that 32-byte digest with the example key:

```text
e5bc0571cf814a9c0bdafa595532e9c4793b0b1343e563ec9951b6be4d7a078a373c7f2728b47f32c03ee6d0235a20a315487777e17956314b0b3e415ed1f903
```

Full envelope as POSTed (pretty-printed for readability; the node accepts any key order):

```json
{
  "collection": "docs/example",
  "created_at": 1793283000,
  "expires_at": 1824732600,
  "owner": "ed25519:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
  "labels": ["demo", "docs"],
  "data": {
    "ciphertext": "base64-or-any-client-format",
    "nonce": "..."
  },
  "id": "8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a",
  "sig": "e5bc0571cf814a9c0bdafa595532e9c4793b0b1343e563ec9951b6be4d7a078a373c7f2728b47f32c03ee6d0235a20a315487777e17956314b0b3e415ed1f903"
}
```

`POST /api/v1/events` returns `201`:

```json
{"status":"stored","id":"8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a"}
```

`GET /api/v1/events/{id}` returns the stored canonical envelope:

```text
{"collection":"docs/example","created_at":1793283000,"data":{"ciphertext":"base64-or-any-client-format","nonce":"..."},"expires_at":1824732600,"id":"8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a","labels":["demo","docs"],"owner":"ed25519:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c","sig":"e5bc0571cf814a9c0bdafa595532e9c4793b0b1343e563ec9951b6be4d7a078a373c7f2728b47f32c03ee6d0235a20a315487777e17956314b0b3e415ed1f903"}
```

## Public routes

### `GET /`

Minimal JSON route index. Includes the service version, API version, base path, description, and a map of route → purpose.

```json
{
  "service": "rabta",
  "version": "0.0.1-dev",
  "api_version": "v1",
  "base_path": "/api/v1",
  "description": "signed-event relay with local content-addressed blobs",
  "routes": {
    "GET /api/v1/health": "liveness probe",
    "GET /api/v1/stats": "storage + eviction counters",
    "GET /api/v1/openapi.json": "OpenAPI 3.1 description of this API",
    "POST /api/v1/events": "publish a signed envelope (application/json)",
    "GET /api/v1/events": "list (collection, label*, owner, limit, cursor)",
    "GET /api/v1/events/stream": "SSE (Last-Event-ID resume)",
    "GET /api/v1/events/{id}": "fetch one (ETag / If-None-Match)",
    "GET /api/v1/events/{id}/size": "envelope size + attached blob bytes",
    "PUT /api/v1/blobs": "raw blob upload; identity is the sha256 of the body",
    "GET /api/v1/blobs/{hash}": "download a blob (Range / ETag)",
    "HEAD /api/v1/blobs/{hash}": "blob metadata"
  }
}
```

Every route except this index lives under `/api/v1`; clients should treat the prefix as the API version marker.

### `GET /api/v1/openapi.json`

OpenAPI 3.1 document describing every public route, parameter, request body, and response schema. Paths are built from the same route constants as the router, so the document cannot drift from the served endpoints. The `servers` entry is `/api/v1`, and paths inside the document are relative to that prefix.

```bash
curl -s http://localhost:3232/api/v1/openapi.json | jq '.paths | keys'
```

### `GET /api/v1/health`

Liveness probe.

```json
{"status":"ok","live_events":6,"live_blobs":3}
```

`live_events` and `live_blobs` count rows currently marked live. Expired rows disappear from reads immediately, but the counts include them until the next reaper sweep (every 5 minutes).

### `GET /api/v1/stats`

JSON monitoring snapshot. No dashboard.

```json
{
  "events": {"live": 6, "bytes": 2185},
  "blobs": {
    "enabled": true,
    "live": 3,
    "bytes": 4194304,
    "claimed_bytes": 1048576,
    "unclaimed_bytes": 3145728,
    "max_single_bytes": 214748364
  },
  "storage": {
    "budget_bytes": 19327352832,
    "target_bytes": 18253611008,
    "used_bytes": 1050761,
    "store_bytes": 21474836480,
    "configured_bytes": 21474836480,
    "clamped_to_free": false,
    "free_bytes": 457677926400,
    "data_dir": "/data",
    "whitelist_keys": 0,
    "whitelist_event_bytes": 0,
    "whitelist_blob_bytes": 0
  },
  "reaper": {
    "interval_s": 300,
    "last_sweep_at": 1793283600,
    "last_sweep_age_s": 12
  },
  "eviction": {
    "events_evicted": 0,
    "blobs_evicted": 0,
    "bytes_reclaimed": 0,
    "candidates": {
      "events": 6,
      "envelope_bytes": 2185,
      "blob_bytes": 1048576,
      "reclaimable_bytes": 1050761,
      "largest_bytes": 1048596
    }
  },
  "upload": {
    "inflight_bytes": 0,
    "max_single_bytes": 214748364
  },
  "sse": {"subscribers": 1, "cap": 256},
  "uptime_s": 3600,
  "version": "0.0.1-dev"
}
```

The `storage` block is the one to read when a write is refused:

| Field | Meaning |
| --- | --- |
| `budget_bytes` | Admission watermark: 90% of the effective budget. |
| `target_bytes` | Post-eviction target: 85% of the effective budget. |
| `used_bytes` | Charged bytes: live envelopes plus blobs claimed by a live event. |
| `store_bytes` | Effective budget after the boot-time free-disk clamp. |
| `configured_bytes` | Budget as configured, before the clamp. |
| `clamped_to_free` | Whether the boot-time clamp changed the budget. |
| `free_bytes` | Free space on the data volume at the time of the call. |
| `data_dir` | Data directory in use. |
| `whitelist_keys` | Pinned owners loaded from `WHITELIST_PUBKEYS`. |
| `whitelist_event_bytes` | Charged bytes of live envelopes owned by a pinned key. |
| `whitelist_blob_bytes` | Size of live blobs claimed by at least one pinned event, counted once. |

`eviction` counts cover the life of the process. `eviction.candidates` is a live snapshot: the events pressure eviction could delete right now (whitelisted owners excluded), the envelope plus sole-claimed blob bytes they hold, and the footprint of the single largest candidate. It is computed on each `/api/v1/stats` call, not accumulated. `upload.inflight_bytes` is the temp-file reservation currently held by active uploads. `reaper.last_sweep_at` and `last_sweep_age_s` are `null` until the first sweep completes.

### `POST /api/v1/events`

Publish one signed envelope.

```bash
curl -X POST http://localhost:3232/api/v1/events \
  -H 'content-type: application/json' \
  --data-binary @event.json
```

**Request**

- Body: one envelope JSON object, at most **100,000 bytes**.
- `Content-Type` is not enforced, but send `application/json`.

**Response**

- `201 Created` when the event is new:

  ```json
  {"status":"stored","id":"8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a"}
  ```

- `200 OK` when the same `id` is already stored:

  ```json
  {"status":"duplicate","id":"8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a"}
  ```

Duplicate deliveries are idempotent. Blobs are uploaded separately; an event can reference them through `bin`, but blob bytes never travel inside the event body.

### `GET /api/v1/events`

List live, unexpired events with filters and cursor pagination.

| Query parameter | Meaning |
| --- | --- |
| `collection` | Exact collection match. |
| `label` | Repeatable. All supplied labels must be present (AND). |
| `owner` | `ed25519:<64 hex>` only. Bare hex is rejected. |
| `limit` | Page size, clamped to `1..100`. Default `20`. |
| `cursor` | Opaque cursor from `next_cursor`. |

```bash
curl 'http://localhost:3232/api/v1/events?collection=chat/room-42&label=team&limit=20'
```

**Response**

```json
{
  "count": 2,
  "events": [
    {
      "collection": "chat/room-42",
      "created_at": 1793283000,
      "data": {"ciphertext": "..."},
      "expires_at": 1824732600,
      "id": "8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a",
      "labels": ["demo", "docs"],
      "owner": "ed25519:ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c",
      "sig": "e5bc0571..."
    }
  ],
  "next_cursor": "AAAAAGrjU7iHct-oZmR5c1u2Rf7wWDJ_YsEjeJkftl8wntHQkpuCSg"
}
```

- Results are ordered by `created_at DESC, id DESC`.
- `next_cursor` is `null` when fewer than `limit` rows were returned; otherwise it is a base64url-encoded opaque cursor for the next page.
- The cursor encodes the last row's `(created_at, id)`. Treat it as opaque.
- Expired events and events being evicted are never returned.
- Empty or duplicate `label` filters return `400 bad_label`.
- A non-canonical `owner` returns `400 bad_owner`.
- A malformed `cursor` returns `400 bad_json`.

### `GET /api/v1/events/{id}`

Fetch one stored envelope by event ID.

```bash
curl -i http://localhost:3232/api/v1/events/8772dfa8666479735bb645fef058327f62c12378991fb65f309ed1d0929b824a
```

- `200` with `Content-Type: application/json`.
- `ETag: "<id>"`.
- `If-None-Match: "<id>"`, the bare ID, or `*` returns `304 Not Modified`.
- Invalid, unknown, or expired IDs return `404 not_found`.
- The body is the exact stored canonical envelope, not the original upload bytes.

### `GET /api/v1/events/{id}/size`

Return the stored envelope size and the true size including its attached blobs.

```json
{
  "id": "2f134cda189a7ba816061f1c1bf3975ff6e5d7cbb3c86d2b5a60a7518904079f",
  "envelope_size": 398,
  "total_size": 12582952
}
```

- `envelope_size` is the stored canonical envelope in bytes.
- `total_size` is `envelope_size` plus the sizes of the live blob rows named in `bin`.
- Dangling references, expired blobs, and repeated hashes add nothing.
- Missing/expired events return `404 not_found`.

### `GET /api/v1/events/stream`

Server-Sent Events stream of newly stored events.

```bash
curl -N 'http://localhost:3232/api/v1/events/stream?collection=chat/room-42&label=team'
```

**Query parameters**

Same filters as `GET /api/v1/events`: `collection`, repeated `label` (AND), and canonical `owner`. `limit` and `cursor` are not used; all matching live events are streamed.

**Resume**

Send `Last-Event-ID: <seq>` to resume after a sequence number. Without the header, the stream starts at the current high-water mark and emits only events stored after the connection opens.

The `id:` field is an internal per-node event sequence number, **not** the event's SHA-256 ID. It is stable only on the same node/database and exists for resume.

**Wire format**

Each event is one frame:

```text
id: 42
event: message
data: {"collection":"chat/room-42","created_at":1793283000,"data":{"ciphertext":"..."},"expires_at":1824732600,"id":"8772dfa8...","labels":["team"],"owner":"ed25519:ea4a...","sig":"e5bc05..."}

```

- `data` is the compact canonical envelope on one line.
- The blank line terminates the frame.
- Every 15 seconds an idle stream receives a comment keepalive:

  ```text
  : keepalive

  ```

- On reconnect, the node replays matching rows after `Last-Event-ID` from SQLite, then switches to the live feed. Missed rows are not lost as long as they are still live and unexpired.
- The default subscriber cap is 256; when full, the request fails with `503 subscriber_cap` before the stream starts.
- Disconnects release the subscriber slot automatically.

### `PUT /api/v1/blobs`

Upload raw bytes. The node derives the identity from the body; there is no filename, extension, or multipart form.

```bash
curl -X PUT --data-binary @file.bin http://localhost:3232/api/v1/blobs
```

- No `STORAGE_LIMIT` toggle disables blobs; the routes are always present.
- The body is streamed to a temp file while SHA-256 is computed. Nothing is buffered in memory.
- An empty body returns `400 empty_body`.
- A body larger than **1% of `STORAGE_LIMIT`** returns `413 too_large`; when `Content-Length` already exceeds that cap, the request is rejected before reading.
- A stalled body (no chunk for 30 seconds) returns `408 timeout`.
- No upload slot or temp-file budget available returns `429 busy`.
- Free space below the safety floor returns `507 disk_full`.
- A blob hash already referenced by a live event is charged to `STORAGE_LIMIT`, so uploading it returns `507 at_watermark` if it cannot fit. An unclaimed upload skips budget admission and only has to clear the free-space floor.

**Response** — `201 Created` for new bytes (with `Location: /api/v1/blobs/{hash}`), `200 OK` when the hash already exists:

```json
{
  "hash": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
  "size": 12,
  "url": "/api/v1/blobs/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
  "expires_at": 1824732600
}
```

Identity is the lowercase hex SHA-256 of the raw body. Re-uploading the same bytes deduplicates: no bytes are added, access metadata is refreshed, and the response is `200 OK`. Re-uploading never extends an unclaimed blob past 48 hours.

### `GET /api/v1/blobs/{hash}`

Download a blob. `hash` is 64 hex characters; a malformed hash returns `404 not_found`.

```bash
curl -o file.bin http://localhost:3232/api/v1/blobs/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08
```

Response headers:

| Header | Value |
| --- | --- |
| `Content-Type` | `application/octet-stream` |
| `ETag` | `"<hash>"` |
| `Cache-Control` | `public, max-age=3600` |
| `Accept-Ranges` | `bytes` |
| `X-Content-Type-Options` | `nosniff` |

- Supports one `Range: bytes=from-to` range with `206 Partial Content` and `Content-Range`. An unsatisfiable range falls back to the full `200`.
- `If-None-Match` with the blob's ETag returns `304 Not Modified`.
- A successful `200`, `206`, or `304` records the access. It never extends the blob's deadline; access tracking is write-throttled to at most once per hour per blob.

A blob's deadline is derived, not stored as a sliding lease. An unclaimed blob expires **48 hours after its first upload**; a blob named in one or more live events expires when the latest of those events expires. Reads never change the deadline.

### `HEAD /api/v1/blobs/{hash}`

Same headers and miss behavior as `GET`, with no body. A hit records the access.

## Blob plane

Blobs are stored locally:

| Event | Effect |
| --- | --- |
| `PUT /api/v1/blobs` | Streams the body to a temp file, hashes it, installs it at `/data/blobs/ab/<hash>`, and returns the hash. |
| Same bytes uploaded again | Deduplicated; no new bytes; access metadata refreshed; deadline unchanged. |
| Read | Serves bytes with Range/ETag and records the access (throttled). Deadline unchanged. |
| Disk pressure | Events leave first, largest total footprint first (envelope plus blobs only that event claims), which may unclaim their blobs; then claimed blobs, down to the 85% target. Unclaimed blobs are not pressure-evicted. |
| Pinned claim | A blob referenced by any live event from a `WHITELIST_PUBKEYS` owner is never a pressure victim, even if an unprotected event also references it. |
| Retention | Unclaimed: 48 hours from upload. Claimed: latest expiry of the live events listing it. Reclaimed by the reaper. |

`STORAGE_LIMIT` is a single byte budget with an admission line at 90% and an eviction target at 85%. It counts event envelopes plus blobs claimed by a live event; unclaimed blobs are outside it until an event references them.

Eviction removes events one at a time, largest total footprint first — the envelope plus any blobs only that event claims — until charged usage is at or below the 85% target. A blob shared with another live event counts for neither claimant, because deleting one of them frees no charged bytes. Ties go to the older event, then the lower id.

Whitelisting changes only the pressure path: pinned events and their claimed blobs are skipped by eviction, while the TTL sweep still deletes them at `expires_at`. If pinned data alone exceeds the admission watermark, new writes are refused with `507 at_watermark`, and `/api/v1/stats` shows the pinned totals under `storage`.

If the configured budget exceeds 90% of free disk at boot, it is clamped to 90%. A single blob may be at most 1% of the effective `STORAGE_LIMIT`.

## Limits and runtime configuration

### Hard limits

| Limit | Value |
| --- | --- |
| Event envelope | 100,000 bytes |
| Event TTL | > 60 seconds and ≤ 10 years (3650 days) |
| Labels per event | 10 |
| Public event list page | 1–100 (default 20) |
| Single blob | 1% of effective `STORAGE_LIMIT` |
| Concurrent blob uploads | 8 |
| Upload stall timeout | 30 seconds between chunks |
| Blob retention | Unclaimed: 48 hours from upload. Claimed: latest referencing event expiry. Reads never extend it. |
| SSE subscribers | 256 by default |
| Reaper interval | 300 seconds |
| Whitelisted owners | 64 keys; exempt from pressure eviction, still TTL-expired |

### Environment variables

Set variables in the process environment or in `/data/config.env` as `KEY=VALUE` lines. The process environment wins. Invalid values refuse to boot rather than silently falling back.

The bundled [`compose.yml`](compose.yml) sets `STORAGE_LIMIT` to `20GB` and passes `WHITELIST_PUBKEYS` through from the stack environment. `SSE_SUBSCRIBER_CAP` and `RUST_LOG` keep their binary defaults.

| Variable | Default | Meaning |
| --- | --- | --- |
| `PORT` | `3232` | HTTP port. |
| `DATA_DIR` | `/data` | SQLite database and blob directory. |
| `STORAGE_LIMIT` | `20GB` | One byte budget for event envelopes plus blobs claimed by a live event. Must be greater than 0. |
| `SSE_SUBSCRIBER_CAP` | `256` | Maximum concurrent SSE clients. |
| `WHITELIST_PUBKEYS` | empty | Comma-separated owner keys, up to 64, bare or `ed25519:`-prefixed. Their events and claimed blobs are exempt from pressure eviction; TTL still applies. |
| `RUST_LOG` | `info` | Log filter, for example `debug` or `rabta=debug`. |

`WHITELIST_PUBKEYS` accepts bare 64-hex keys or `ed25519:<64 hex>`. Hex is case-insensitive, duplicates collapse, empty entries are ignored, and more than 64 unique keys refuses boot. A malformed key also refuses boot; the node never silently pins a partial list.

Sizes accept `20GB`, `512MiB`, `52428800`, and similar. Derived values:

- The combined budget has an admission watermark at 90% and an eviction target at 85%.
- If `STORAGE_LIMIT` exceeds 90% of free disk at boot, it is clamped to 90%.
- `STORAGE_LIMIT=0` refuses to boot.
- Unclaimed blob uploads do not count toward `STORAGE_LIMIT` until a live event references them; they expire 48 hours after upload.
- A single blob may be at most 1% of the effective `STORAGE_LIMIT`.
- There is no `STORAGE_MAX` or `IPFS` variable.
- Pinned owners above the watermark cause new writes to be refused (`507 at_watermark`) rather than evicting pinned rows. `/api/v1/stats` reports the pinned byte totals.

## Client examples

### Sign and publish with Node.js

```js
import { createHash, generateKeyPairSync, sign } from 'node:crypto';

function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(',')}]`;
  if (value && typeof value === 'object') {
    return `{${Object.keys(value).sort()
      .map((k) => `${JSON.stringify(k)}:${canonical(value[k])}`)
      .join(',')}}`;
  }
  return JSON.stringify(value);
}

const { privateKey, publicKey } = generateKeyPairSync('ed25519');
const rawPublicKey = publicKey.export({ format: 'der', type: 'spki' }).subarray(-32);
const owner = `ed25519:${rawPublicKey.toString('hex')}`;

const now = Math.floor(Date.now() / 1000);
const fields = {
  collection: 'chat/room-42',
  created_at: now,
  expires_at: now + 24 * 60 * 60,
  owner,
  labels: ['team'],
  data: { ciphertext: '<client-encrypted payload>' },
};

const digest = createHash('sha256').update(canonical(fields)).digest();
const envelope = {
  ...fields,
  id: digest.toString('hex'),
  sig: sign(null, digest, privateKey).toString('hex'),
};

const response = await fetch('http://localhost:3232/api/v1/events', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify(envelope),
});
console.log(await response.json());
```

### Upload and fetch a blob

```bash
curl -X PUT --data-binary @file.bin \
  "http://localhost:3232/api/v1/blobs"

# {"hash":"9f86d0...","size":12,"url":"/api/v1/blobs/9f86d0...","expires_at":1824732600}

# Fetch it back; Range and ETag are supported.
curl -o file.bin \
  "http://localhost:3232/api/v1/blobs/9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
```

In a browser, hash the bytes first if you want to verify the response, or just read the `hash` from the upload response:

```js
const response = await fetch('http://localhost:3232/api/v1/blobs', {
  method: 'PUT',
  body: file,
});
const { hash } = await response.json();
const bytes = await fetch(`http://localhost:3232/api/v1/blobs/${hash}`).then((r) => r.blob());
```

### List, fetch, and stream

```bash
# List the newest 20 events in a collection.
curl 'http://localhost:3232/api/v1/events?collection=chat/room-42&limit=20'

# Fetch one event by ID.
curl http://localhost:3232/api/v1/events/<64-hex-id>

# Check the stored envelope size.
curl http://localhost:3232/api/v1/events/<64-hex-id>/size

# Stream live updates, resuming after sequence 42.
curl -N -H 'Last-Event-ID: 42' \
  'http://localhost:3232/api/v1/events/stream?collection=chat/room-42'
```

---

Rabta is under active development. The public API is intentionally small. If a wire detail here disagrees with the code, the code in `src/api/` is authoritative.
