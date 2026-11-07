//! OpenAPI 3.1 description of the public API.
//!
//! Paths are built from [`routes`](super::routes), so renaming a route changes
//! both the router and this document. Schemas describe the exact wire shapes
//! produced by the handlers.

use serde_json::{Map, Value, json};

use super::routes;

fn json_response(description: &str, schema: Value) -> Value {
    json!({
        "description": description,
        "content": {"application/json": {"schema": schema}},
    })
}

fn error_response(description: &str) -> Value {
    json_response(description, schema_ref("Error"))
}

fn schema_ref(name: &str) -> Value {
    json!({"$ref": format!("#/components/schemas/{name}")})
}

fn parameter_ref(name: &str) -> Value {
    json!({"$ref": format!("#/components/parameters/{name}")})
}

/// `GET /api/v1/openapi.json` — the generated document.
pub async fn serve() -> axum::Json<Value> {
    axum::Json(spec())
}

/// Build the OpenAPI document from the canonical route constants.
pub fn spec() -> Value {
    let mut paths = Map::new();

    paths.insert(
        routes::HEALTH.to_string(),
        json!({
            "get": {
                "tags": ["ops"],
                "operationId": "getHealth",
                "summary": "Liveness probe",
                "description": "Returns 200 when the process and embedded SQLite are usable.",
                "responses": {
                    "200": json_response("The node is alive.", schema_ref("Health"))
                }
            }
        }),
    );

    paths.insert(
        routes::STATS.to_string(),
        json!({
            "get": {
                "tags": ["ops"],
                "operationId": "getStats",
                "summary": "Storage and runtime snapshot",
                "description": "Live counts, the storage budget and pinned totals, reaper state, lifetime eviction counters, and what pressure eviction could reclaim right now.",
                "responses": {
                    "200": json_response("The current node snapshot.", schema_ref("Stats"))
                }
            }
        }),
    );

    paths.insert(
        routes::OPENAPI.to_string(),
        json!({
            "get": {
                "tags": ["meta"],
                "operationId": "getOpenApi",
                "summary": "OpenAPI description of this API",
                "description": "This document. Paths are relative to the `/api/v1` server URL.",
                "responses": {
                    "200": json_response(
                        "The OpenAPI 3.1 document.",
                        json!({"type": "object", "description": "An OpenAPI 3.1 document."})
                    )
                }
            }
        }),
    );

    paths.insert(
        routes::EVENTS.to_string(),
        json!({
            "post": {
                "tags": ["events"],
                "operationId": "publishEvent",
                "summary": "Publish a signed envelope",
                "description": "Validates the canonical envelope, its SHA-256 id, and its Ed25519 signature, then stores it. Republishing the same id is idempotent and adds no bytes.",
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/json": {"schema": schema_ref("EventEnvelope")}
                    }
                },
                "responses": {
                    "201": json_response("Event stored.", schema_ref("PublishResponse")),
                    "200": json_response("Event already stored; no bytes added.", schema_ref("PublishResponse")),
                    "400": error_response("Malformed JSON or an invalid field (`bad_json`, `bad_collection`, `bad_label`, `bad_bin`, `bad_owner`, `ttl_range`)."),
                    "401": error_response("The signature does not verify (`bad_sig`)."),
                    "413": error_response("The envelope exceeds 100,000 bytes (`too_large`)."),
                    "500": error_response("Storage failure (`internal`)."),
                    "507": error_response("Storage is at the admission watermark (`at_watermark`).")
                }
            },
            "get": {
                "tags": ["events"],
                "operationId": "listEvents",
                "summary": "List live events",
                "description": "Returns live, unexpired envelopes ordered by `created_at DESC, id DESC`. Repeated `label` filters use AND semantics.",
                "parameters": [
                    parameter_ref("Collection"),
                    parameter_ref("Label"),
                    parameter_ref("Owner"),
                    parameter_ref("Limit"),
                    parameter_ref("Cursor")
                ],
                "responses": {
                    "200": json_response("One page of matching envelopes.", schema_ref("EventList")),
                    "400": error_response("Invalid label, owner, or cursor (`bad_label`, `bad_owner`, `bad_json`)."),
                    "500": error_response("Storage failure (`internal`).")
                }
            }
        }),
    );

    paths.insert(
        routes::EVENT_STREAM.to_string(),
        json!({
            "get": {
                "tags": ["events"],
                "operationId": "streamEvents",
                "summary": "Stream live events over SSE",
                "description": "Server-Sent Events stream. Each frame uses `id: <internal-seq>`, `event: message`, and one compact canonical envelope on the `data:` line. A `: keepalive` comment is sent after 15 idle seconds. Send `Last-Event-ID` to replay matching rows stored after that sequence before switching to the live feed.",
                "parameters": [
                    parameter_ref("Collection"),
                    parameter_ref("Label"),
                    parameter_ref("Owner"),
                    parameter_ref("LastEventId")
                ],
                "responses": {
                    "200": {
                        "description": "The event stream.",
                        "headers": {
                            "Cache-Control": {
                                "description": "Always `no-cache`.",
                                "schema": {"type": "string", "const": "no-cache"}
                            }
                        },
                        "content": {
                            "text/event-stream": {
                                "schema": {"type": "string"},
                                "example": "id: 42\nevent: message\ndata: {\"collection\":\"chat/room-42\",\"id\":\"8772dfa8...\"}\n\n"
                            }
                        }
                    },
                    "400": error_response("Invalid label or owner (`bad_label`, `bad_owner`)."),
                    "503": error_response("The SSE subscriber cap is full (`subscriber_cap`).")
                }
            }
        }),
    );

    paths.insert(
        routes::EVENT_DELETE.to_string(),
        json!({
            "post": {
                "tags": ["events"],
                "operationId": "deleteEvents",
                "summary": "Delete stored events with an owner signature",
                "description": "Deletes up to 100 event ids owned by the signing key in one transaction. The digest input is canonical `{action:\"delete\",created_at,owner,targets}`; owner and targets are lowercased before hashing. Missing or expired targets are reported in `not_found`; a target owned by another key fails the whole request with 403. Live SSE subscribers receive an `event: delete` frame; resuming does not replay deletions.",
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/json": {"schema": schema_ref("DeleteRequest")}
                    }
                },
                "responses": {
                    "200": json_response("Deletion result. Already-missing ids appear in `not_found`.", schema_ref("DeleteResponse")),
                    "400": error_response("Malformed JSON, owner, or targets (`bad_json`, `bad_owner`, `bad_targets`)."),
                    "401": error_response("The signature does not verify (`bad_sig`)."),
                    "403": error_response("At least one target is owned by another key (`forbidden`); nothing was deleted."),
                    "413": error_response("The request exceeds 16,384 bytes (`too_large`)."),
                    "500": error_response("Storage failure (`internal`).")
                }
            }
        }),
    );

    paths.insert(
        routes::EVENT_BY_ID.to_string(),
        json!({
            "get": {
                "tags": ["events"],
                "operationId": "getEvent",
                "summary": "Fetch one stored envelope",
                "description": "Returns the exact stored canonical envelope. Invalid, unknown, and expired ids all return 404.",
                "parameters": [
                    parameter_ref("EventIdPath"),
                    parameter_ref("IfNoneMatch")
                ],
                "responses": {
                    "200": {
                        "description": "The stored canonical envelope.",
                        "headers": {
                            "ETag": {
                                "description": "Quoted event id.",
                                "schema": {"type": "string"}
                            },
                            "X-Content-Type-Options": {
                                "description": "Always `nosniff`.",
                                "schema": {"type": "string", "const": "nosniff"}
                            }
                        },
                        "content": {
                            "application/json": {"schema": schema_ref("EventEnvelope")}
                        }
                    },
                    "304": {
                        "description": "The client's ETag matches; no body.",
                        "headers": {
                            "ETag": {
                                "description": "Quoted event id.",
                                "schema": {"type": "string"}
                            }
                        }
                    },
                    "404": error_response("Invalid, unknown, or expired event id (`not_found`).")
                }
            }
        }),
    );

    paths.insert(
        routes::EVENT_SIZE.to_string(),
        json!({
            "get": {
                "tags": ["events"],
                "operationId": "getEventSize",
                "summary": "Event size metadata",
                "description": "The stored envelope size plus the sizes of the live blobs named in `bin`. Dangling and repeated references add nothing.",
                "parameters": [parameter_ref("EventIdPath")],
                "responses": {
                    "200": json_response("Envelope and total attached size.", schema_ref("EventSize")),
                    "404": error_response("Invalid, unknown, or expired event id (`not_found`).")
                }
            }
        }),
    );

    paths.insert(
        routes::BLOBS.to_string(),
        json!({
            "put": {
                "tags": ["blobs"],
                "operationId": "uploadBlob",
                "summary": "Upload raw blob bytes",
                "description": "Streams the raw body to a temp file while computing SHA-256, then installs it at a content-addressed path. There is no filename, extension, or multipart form. Re-uploading the same bytes is deduplicated.",
                "requestBody": {
                    "required": true,
                    "content": {
                        "application/octet-stream": {
                            "schema": {"type": "string", "format": "binary"}
                        }
                    }
                },
                "responses": {
                    "201": {
                        "description": "Blob stored.",
                        "headers": {
                            "Location": {
                                "description": "Versioned download path for this blob.",
                                "schema": {"type": "string"}
                            }
                        },
                        "content": {
                            "application/json": {"schema": schema_ref("BlobUpload")}
                        }
                    },
                    "200": {
                        "description": "The blob already existed; access metadata was refreshed.",
                        "content": {
                            "application/json": {"schema": schema_ref("BlobUpload")}
                        }
                    },
                    "400": error_response("Empty body or a failed body stream (`empty_body`, `bad_body`)."),
                    "404": error_response("The blob plane is disabled (`blobs_disabled`)."),
                    "408": error_response("The body stalled for 30 seconds (`timeout`)."),
                    "413": error_response("The blob exceeds 1% of `STORAGE_LIMIT` (`too_large`)."),
                    "429": error_response("No upload slot or temp-file budget is available (`busy`)."),
                    "500": error_response("Storage failure (`internal`)."),
                    "507": error_response("Storage is at the watermark or below the free-space floor (`at_watermark`, `disk_full`).")
                }
            }
        }),
    );

    paths.insert(
        routes::BLOB_BY_HASH.to_string(),
        json!({
            "get": {
                "tags": ["blobs"],
                "operationId": "downloadBlob",
                "summary": "Download a blob",
                "description": "Streams the blob with ETag, cache, and range support. A successful read records access but never extends the blob deadline.",
                "parameters": [
                    parameter_ref("BlobHashPath"),
                    parameter_ref("Range"),
                    parameter_ref("IfNoneMatch")
                ],
                "responses": {
                    "200": {
                        "description": "The full blob, or the full blob when a range is unsatisfiable.",
                        "headers": {
                            "Content-Type": {
                                "description": "Always `application/octet-stream`.",
                                "schema": {"type": "string", "const": "application/octet-stream"}
                            },
                            "ETag": {
                                "description": "Quoted blob hash.",
                                "schema": {"type": "string"}
                            },
                            "Cache-Control": {
                                "description": "Always `public, max-age=3600`.",
                                "schema": {"type": "string", "const": "public, max-age=3600"}
                            },
                            "Accept-Ranges": {
                                "description": "Always `bytes`.",
                                "schema": {"type": "string", "const": "bytes"}
                            },
                            "X-Content-Type-Options": {
                                "description": "Always `nosniff`.",
                                "schema": {"type": "string", "const": "nosniff"}
                            },
                            "Content-Length": {
                                "description": "Blob size in bytes.",
                                "schema": {"type": "integer", "format": "int64", "minimum": 0}
                            }
                        },
                        "content": {
                            "application/octet-stream": {
                                "schema": {"type": "string", "format": "binary"}
                            }
                        }
                    },
                    "206": {
                        "description": "The requested byte range.",
                        "headers": {
                            "Content-Range": {
                                "description": "The served byte range and full size.",
                                "schema": {"type": "string"}
                            },
                            "Content-Length": {
                                "description": "Range length in bytes.",
                                "schema": {"type": "integer", "format": "int64", "minimum": 0}
                            }
                        },
                        "content": {
                            "application/octet-stream": {
                                "schema": {"type": "string", "format": "binary"}
                            }
                        }
                    },
                    "304": {
                        "description": "The client's ETag matches; no body.",
                        "headers": {
                            "ETag": {
                                "description": "Quoted blob hash.",
                                "schema": {"type": "string"}
                            },
                            "Cache-Control": {
                                "description": "Always `public, max-age=3600`.",
                                "schema": {"type": "string", "const": "public, max-age=3600"}
                            }
                        }
                    },
                    "404": error_response("Unknown, expired, or malformed hash, or the blob plane is disabled (`not_found`, `blobs_disabled`)."),
                    "500": error_response("Storage failure (`internal`).")
                }
            },
            "head": {
                "tags": ["blobs"],
                "operationId": "headBlob",
                "summary": "Blob metadata",
                "description": "Same headers and miss behavior as GET, with no body. A hit records access.",
                "parameters": [
                    parameter_ref("BlobHashPath"),
                    parameter_ref("IfNoneMatch")
                ],
                "responses": {
                    "200": {
                        "description": "The blob exists; headers describe it.",
                        "headers": {
                            "Content-Type": {
                                "description": "Always `application/octet-stream`.",
                                "schema": {"type": "string", "const": "application/octet-stream"}
                            },
                            "ETag": {
                                "description": "Quoted blob hash.",
                                "schema": {"type": "string"}
                            },
                            "Cache-Control": {
                                "description": "Always `public, max-age=3600`.",
                                "schema": {"type": "string", "const": "public, max-age=3600"}
                            },
                            "Accept-Ranges": {
                                "description": "Always `bytes`.",
                                "schema": {"type": "string", "const": "bytes"}
                            },
                            "Content-Length": {
                                "description": "Blob size in bytes.",
                                "schema": {"type": "integer", "format": "int64", "minimum": 0}
                            }
                        }
                    },
                    "304": {
                        "description": "The client's ETag matches; no body.",
                        "headers": {
                            "ETag": {
                                "description": "Quoted blob hash.",
                                "schema": {"type": "string"}
                            },
                            "Cache-Control": {
                                "description": "Always `public, max-age=3600`.",
                                "schema": {"type": "string", "const": "public, max-age=3600"}
                            }
                        }
                    },
                    "404": error_response("Unknown, expired, or malformed hash, or the blob plane is disabled (`not_found`, `blobs_disabled`)."),
                    "500": error_response("Storage failure (`internal`).")
                }
            }
        }),
    );

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Rabta API",
            "version": routes::API_VERSION,
            "description": "Signed-event relay with local content-addressed blobs. Clients canonicalize, hash, and sign envelopes; the node verifies signatures, stores opaque JSON, streams changes over SSE, and expires data on a TTL. Every failure uses the Error schema."
        },
        "servers": [
            {
                "url": routes::PREFIX,
                "description": "This node."
            }
        ],
        "tags": [
            {"name": "ops", "description": "Liveness and monitoring."},
            {"name": "events", "description": "Signed event envelopes and the live SSE stream."},
            {"name": "blobs", "description": "Raw content-addressed blob upload and download."},
            {"name": "meta", "description": "Machine-readable API description."}
        ],
        "security": [],
        "paths": Value::Object(paths),
        "components": {
            "parameters": {
                "EventIdPath": {
                    "name": "id",
                    "in": "path",
                    "required": true,
                    "description": "64-hex event id.",
                    "schema": schema_ref("EventId")
                },
                "BlobHashPath": {
                    "name": "hash",
                    "in": "path",
                    "required": true,
                    "description": "64-hex SHA-256 of the raw blob body.",
                    "schema": schema_ref("BlobHash")
                },
                "Collection": {
                    "name": "collection",
                    "in": "query",
                    "required": false,
                    "description": "Exact collection match.",
                    "schema": {
                        "type": "string",
                        "pattern": "^[a-z0-9/_-]{1,32}$"
                    }
                },
                "Label": {
                    "name": "label",
                    "in": "query",
                    "required": false,
                    "description": "Repeatable. All supplied labels must be present (AND).",
                    "style": "form",
                    "explode": true,
                    "schema": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "pattern": "^[A-Za-z0-9:_\\.\\-\\+/]{1,64}$"
                        }
                    }
                },
                "Owner": {
                    "name": "owner",
                    "in": "query",
                    "required": false,
                    "description": "Canonical owner only; bare hex is rejected.",
                    "schema": schema_ref("Owner")
                },
                "Limit": {
                    "name": "limit",
                    "in": "query",
                    "required": false,
                    "description": "Page size. Values outside the range are clamped.",
                    "schema": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "default": 20
                    }
                },
                "Cursor": {
                    "name": "cursor",
                    "in": "query",
                    "required": false,
                    "description": "Opaque cursor from `next_cursor`.",
                    "schema": {"type": "string"}
                },
                "LastEventId": {
                    "name": "Last-Event-ID",
                    "in": "header",
                    "required": false,
                    "description": "Resume after this internal event sequence number.",
                    "schema": {"type": "integer", "format": "int64", "minimum": 0}
                },
                "IfNoneMatch": {
                    "name": "If-None-Match",
                    "in": "header",
                    "required": false,
                    "description": "A quoted ETag, the bare value, or `*`. A match returns 304.",
                    "schema": {"type": "string"}
                },
                "Range": {
                    "name": "Range",
                    "in": "header",
                    "required": false,
                    "description": "One `bytes=from-to` range. Malformed or unsatisfiable ranges return the full blob.",
                    "schema": {"type": "string"},
                    "example": "bytes=0-1023"
                }
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    "required": ["error", "code"],
                    "properties": {
                        "error": {
                            "type": "string",
                            "description": "Human-readable message."
                        },
                        "code": {
                            "type": "string",
                            "description": "Stable machine-readable code, such as `not_found` or `at_watermark`."
                        }
                    },
                    "additionalProperties": false
                },
                "Health": {
                    "type": "object",
                    "required": ["status", "live_events", "live_blobs"],
                    "properties": {
                        "status": {"type": "string", "const": "ok"},
                        "live_events": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "live_blobs": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        }
                    },
                    "additionalProperties": false
                },
                "EventId": {
                    "type": "string",
                    "description": "Lowercase hex SHA-256 of the canonical envelope digest.",
                    "pattern": "^[0-9a-f]{64}$"
                },
                "BlobHash": {
                    "type": "string",
                    "description": "Lowercase hex SHA-256 of the raw blob body.",
                    "pattern": "^[0-9a-f]{64}$"
                },
                "Owner": {
                    "type": "string",
                    "description": "Ed25519 public key as `ed25519:<64 hex>`. The server stores it lowercase.",
                    "pattern": "^ed25519:[0-9a-fA-F]{64}$"
                },
                "EventEnvelope": {
                    "type": "object",
                    "required": [
                        "collection",
                        "created_at",
                        "expires_at",
                        "owner",
                        "labels",
                        "data",
                        "id",
                        "sig"
                    ],
                    "properties": {
                        "collection": {
                            "type": "string",
                            "pattern": "^[a-z0-9/_-]{1,32}$"
                        },
                        "created_at": {
                            "type": "integer",
                            "format": "int64",
                            "description": "Unix seconds. Must not be in the server's future."
                        },
                        "expires_at": {
                            "type": "integer",
                            "format": "int64",
                            "description": "Unix seconds. Must be in the future and at most 10 years after `created_at`."
                        },
                        "owner": schema_ref("Owner"),
                        "labels": {
                            "type": "array",
                            "maxItems": 10,
                            "items": {
                                "type": "string",
                                "pattern": "^[A-Za-z0-9:_\\.\\-\\+/]{1,64}$"
                            }
                        },
                        "bin": {
                            "type": "array",
                            "description": "Blob hashes this event claims. References only; bytes never travel in the envelope.",
                            "minItems": 1,
                            "maxItems": 8,
                            "uniqueItems": true,
                            "items": schema_ref("BlobHash")
                        },
                        "data": {
                            "type": "object",
                            "description": "Opaque JSON object owned by the client.",
                            "additionalProperties": true
                        },
                        "id": schema_ref("EventId"),
                        "sig": {
                            "type": "string",
                            "description": "Ed25519 signature over the 32-byte digest.",
                            "pattern": "^[0-9a-f]{128}$"
                        }
                    },
                    "additionalProperties": true,
                    "description": "Canonical signed envelope. On publish, unknown fields are accepted and discarded; `id` must match the digest and `sig` must verify."
                },
                "PublishResponse": {
                    "type": "object",
                    "required": ["status", "id"],
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["stored", "duplicate"]
                        },
                        "id": schema_ref("EventId")
                    },
                    "additionalProperties": false
                },
                "DeleteRequest": {
                    "type": "object",
                    "required": ["owner", "targets", "created_at", "sig"],
                    "properties": {
                        "owner": schema_ref("Owner"),
                        "targets": {
                            "type": "array",
                            "description": "Event ids to delete. The signer must own every target.",
                            "minItems": 1,
                            "maxItems": 100,
                            "uniqueItems": true,
                            "items": schema_ref("EventId")
                        },
                        "created_at": {
                            "type": "integer",
                            "format": "int64",
                            "description": "Unix seconds. Must not be in the server's future."
                        },
                        "sig": {
                            "type": "string",
                            "description": "Ed25519 signature over SHA-256 of canonical `{action:\"delete\",created_at,owner,targets}`.",
                            "pattern": "^[0-9a-fA-F]{128}$"
                        }
                    },
                    "additionalProperties": true
                },
                "DeleteResponse": {
                    "type": "object",
                    "required": ["deleted", "not_found"],
                    "properties": {
                        "deleted": {
                            "type": "array",
                            "items": schema_ref("EventId")
                        },
                        "not_found": {
                            "type": "array",
                            "description": "Targets that were already missing or expired.",
                            "items": schema_ref("EventId")
                        }
                    },
                    "additionalProperties": false
                },
                "EventList": {
                    "type": "object",
                    "required": ["count", "events", "next_cursor"],
                    "properties": {
                        "count": {
                            "type": "integer",
                            "minimum": 0
                        },
                        "events": {
                            "type": "array",
                            "items": schema_ref("EventEnvelope")
                        },
                        "next_cursor": {
                            "type": ["string", "null"],
                            "description": "Opaque cursor for the next page, or null when the page was not full."
                        }
                    },
                    "additionalProperties": false
                },
                "EventSize": {
                    "type": "object",
                    "required": ["id", "envelope_size", "total_size"],
                    "properties": {
                        "id": schema_ref("EventId"),
                        "envelope_size": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0,
                            "description": "Stored canonical envelope size in bytes."
                        },
                        "total_size": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0,
                            "description": "Envelope size plus live attached blob bytes."
                        }
                    },
                    "additionalProperties": false
                },
                "BlobUpload": {
                    "type": "object",
                    "required": ["hash", "size", "url", "expires_at"],
                    "properties": {
                        "hash": schema_ref("BlobHash"),
                        "size": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "url": {
                            "type": "string",
                            "format": "uri-reference",
                            "description": "Versioned download path for this blob."
                        },
                        "expires_at": {
                            "type": "integer",
                            "format": "int64",
                            "description": "Unix seconds when the blob expires unless a live event extends it."
                        }
                    },
                    "additionalProperties": false
                },
                "Stats": {
                    "type": "object",
                    "required": [
                        "events",
                        "blobs",
                        "storage",
                        "reaper",
                        "eviction",
                        "upload",
                        "sse",
                        "uptime_s",
                        "version"
                    ],
                    "properties": {
                        "events": schema_ref("StatsEvents"),
                        "blobs": schema_ref("StatsBlobs"),
                        "storage": schema_ref("StatsStorage"),
                        "reaper": schema_ref("StatsReaper"),
                        "eviction": schema_ref("StatsEviction"),
                        "upload": schema_ref("StatsUpload"),
                        "sse": schema_ref("StatsSse"),
                        "uptime_s": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "version": {"type": "string"}
                    },
                    "additionalProperties": false
                },
                "StatsEvents": {
                    "type": "object",
                    "required": ["live", "bytes"],
                    "properties": {
                        "live": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0,
                            "description": "Charged live envelope bytes."
                        }
                    },
                    "additionalProperties": false
                },
                "StatsBlobs": {
                    "type": "object",
                    "required": [
                        "enabled",
                        "live",
                        "bytes",
                        "claimed_bytes",
                        "unclaimed_bytes",
                        "max_single_bytes"
                    ],
                    "properties": {
                        "enabled": {"type": "boolean"},
                        "live": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "claimed_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "unclaimed_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "max_single_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        }
                    },
                    "additionalProperties": false
                },
                "StatsStorage": {
                    "type": "object",
                    "required": [
                        "budget_bytes",
                        "target_bytes",
                        "used_bytes",
                        "store_bytes",
                        "configured_bytes",
                        "clamped_to_free",
                        "free_bytes",
                        "data_dir",
                        "whitelist_keys",
                        "whitelist_event_bytes",
                        "whitelist_blob_bytes"
                    ],
                    "properties": {
                        "budget_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "description": "Admission watermark: 90% of the effective budget."
                        },
                        "target_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "description": "Eviction target: 85% of the effective budget."
                        },
                        "used_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "description": "Charged bytes: live envelopes plus claimed blobs."
                        },
                        "store_bytes": {
                            "type": "integer",
                            "format": "int64"
                        },
                        "configured_bytes": {
                            "type": "integer",
                            "format": "int64"
                        },
                        "clamped_to_free": {"type": "boolean"},
                        "free_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "data_dir": {"type": "string"},
                        "whitelist_keys": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "whitelist_event_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "whitelist_blob_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        }
                    },
                    "additionalProperties": false
                },
                "StatsReaper": {
                    "type": "object",
                    "required": ["interval_s", "last_sweep_at", "last_sweep_age_s"],
                    "properties": {
                        "interval_s": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "last_sweep_at": {
                            "type": ["integer", "null"],
                            "description": "Unix seconds of the last completed sweep, or null before the first one."
                        },
                        "last_sweep_age_s": {
                            "type": ["integer", "null"],
                            "description": "Seconds since the last sweep, or null before the first one."
                        }
                    },
                    "additionalProperties": false
                },
                "StatsEviction": {
                    "type": "object",
                    "required": [
                        "events_evicted",
                        "blobs_evicted",
                        "bytes_reclaimed",
                        "candidates"
                    ],
                    "properties": {
                        "events_evicted": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "blobs_evicted": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "bytes_reclaimed": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "candidates": schema_ref("StatsEvictionCandidates")
                    },
                    "additionalProperties": false
                },
                "StatsEvictionCandidates": {
                    "type": "object",
                    "required": [
                        "events",
                        "envelope_bytes",
                        "blob_bytes",
                        "reclaimable_bytes",
                        "largest_bytes"
                    ],
                    "properties": {
                        "events": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0,
                            "description": "Live events pressure eviction could delete now."
                        },
                        "envelope_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "blob_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0,
                            "description": "Blob bytes only the candidate events claim."
                        },
                        "reclaimable_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "largest_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        }
                    },
                    "additionalProperties": false
                },
                "StatsUpload": {
                    "type": "object",
                    "required": ["inflight_bytes", "max_single_bytes"],
                    "properties": {
                        "inflight_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "max_single_bytes": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        }
                    },
                    "additionalProperties": false
                },
                "StatsSse": {
                    "type": "object",
                    "required": ["subscribers", "cap"],
                    "properties": {
                        "subscribers": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        },
                        "cap": {
                            "type": "integer",
                            "format": "int64",
                            "minimum": 0
                        }
                    },
                    "additionalProperties": false
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_refs<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "$ref"
                        && let Some(reference) = child.as_str()
                    {
                        out.push(reference);
                    }
                    collect_refs(child, out);
                }
            }
            Value::Array(items) => {
                for child in items {
                    collect_refs(child, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn spec_covers_every_public_route_and_resolves_refs() {
        let document = spec();
        assert_eq!(document["openapi"], "3.1.0");
        assert_eq!(routes::PREFIX, format!("/api/{}", routes::API_VERSION));
        assert_eq!(document["info"]["version"], routes::API_VERSION);
        assert_eq!(document["servers"][0]["url"], routes::PREFIX);

        let paths = document["paths"].as_object().expect("paths object");
        for route in [
            routes::HEALTH,
            routes::STATS,
            routes::EVENTS,
            routes::EVENT_STREAM,
            routes::EVENT_DELETE,
            routes::EVENT_BY_ID,
            routes::EVENT_SIZE,
            routes::BLOBS,
            routes::BLOB_BY_HASH,
            routes::OPENAPI,
        ] {
            let item = paths
                .get(route)
                .unwrap_or_else(|| panic!("missing path {route}"));
            assert!(
                item.as_object()
                    .is_some_and(|operations| !operations.is_empty()),
                "{route} has no operations"
            );
        }

        let mut refs = Vec::new();
        collect_refs(&document, &mut refs);
        assert!(!refs.is_empty());
        for reference in refs {
            let pointer = reference
                .strip_prefix("#/")
                .unwrap_or_else(|| panic!("non-local reference {reference}"));
            let mut node = &document;
            for part in pointer.split('/') {
                node = node
                    .get(part)
                    .unwrap_or_else(|| panic!("unresolved reference {reference}"));
            }
        }
    }
}
