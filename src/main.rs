use axum::{
    body::Body,
    http::Request,
    middleware,

    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::{
    collections::HashMap,
    env,
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tracing::{error, info};
use uuid::Uuid;

use async_trait::async_trait;
use sqlx::{
    postgres::{PgPool, PgPoolOptions},
    Row,
};

static VERSION: &str = "0.1.0";

#[tokio::main]
async fn main() {
    init_tracing();

    let storage: Arc<dyn Storage> = match env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => {
            info!(event = "storage.init", backend = "postgres", "Using Postgres storage");
            Arc::new(PostgresStorage::connect(&url).await.expect("pg connect"))
        }
        _ => {
            info!(event = "storage.init", backend = "memory", "Using in-memory storage");
            Arc::new(InMemoryStorage::default())
        }
    };

    let api_key = env::var("AIRA_API_KEY").ok().filter(|v| !v.trim().is_empty());
    let allow_insecure_dev = env::var("AIRA_ALLOW_INSECURE_DEV")
        .ok()
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if api_key.is_none() {
        if allow_insecure_dev {
            info!(event = "auth.init", mode = "disabled", "API key auth disabled (dev mode)");
        } else {
            info!(event = "auth.init", mode = "missing", "AIRA_API_KEY not set; /v1 will reject requests");
        }
    } else {
        info!(event = "auth.init", mode = "api_key", "API key auth enabled for /v1");
    }

    let state = AppState {
        storage,
        api_key,
        allow_insecure_dev,
    };

    let v1 = Router::new()
        .route(
            "/entities/:id",
            put(put_entity).patch(patch_entity).get(get_entity),
        )
        .layer(middleware::from_fn_with_state(state.clone(), api_key_guard));

    let app = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .nest("/v1", v1)
        .with_state(state);

    let addr = "0.0.0.0:8080";
    info!(event = "startup", addr, version = VERSION, "AIRA Platform starting");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("info".parse().unwrap()),
        )
        .json()
        .init();
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn version() -> impl IntoResponse {
    (StatusCode::OK, VERSION)
}

// ----------------------------
// Domain models (v0 minimal)
// ----------------------------
#[derive(Clone, Debug, Serialize, Deserialize)]
struct EntityDoc {
    id: String,
    #[serde(rename = "type")]
    entity_type: String,

    // Keep raw JSON-LD fields as-is. This is last-known state store.
    #[serde(flatten)]
    extra: HashMap<String, JsonValue>,
}

#[derive(Clone, Debug)]
struct StoredEntity {
    doc: EntityDoc,
    version: u64,
    #[allow(dead_code)]
    updated_at_ms: i64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn etag_for(version: u64) -> String {
    format!("v{}", version)
}

// ----------------------------
// Errors (central handler)
// ----------------------------
#[derive(Error, Debug)]
enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("invalid json")]
    InvalidJson,
    #[error("entity not found")]
    NotFound { entity_id: String },
    #[error("etag mismatch")]
    EtagMismatch {
        entity_id: String,
        current: String,
        provided: String,
    },
    #[error("internal")]
    Internal,
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorInner,
}

#[derive(Serialize)]
struct ErrorInner {
    code: String,
    message: String,
    details: serde_json::Value,
    #[serde(rename = "traceId")]
    trace_id: String,
}

impl ApiError {
    fn code(&self) -> (&'static str, StatusCode) {
        match self {
            ApiError::Unauthorized => ("UNAUTHORIZED", StatusCode::UNAUTHORIZED),
            ApiError::Forbidden => ("FORBIDDEN", StatusCode::FORBIDDEN),
            ApiError::InvalidJson => ("INVALID_JSON", StatusCode::BAD_REQUEST),
            ApiError::NotFound { .. } => ("ENTITY_NOT_FOUND", StatusCode::NOT_FOUND),
            ApiError::EtagMismatch { .. } => {
                ("ENTITY_ETAG_MISMATCH", StatusCode::PRECONDITION_FAILED)
            }
            ApiError::Internal => ("INTERNAL", StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

fn trace_id_from(headers: &HeaderMap) -> String {
    headers
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn api_error_response(err: ApiError, trace_id: String) -> Response {
    let (code, status) = err.code();

    let (message, details) = match &err {
        ApiError::Unauthorized => ("Missing API key".to_string(), serde_json::json!({})),
        ApiError::Forbidden => ("Invalid API key".to_string(), serde_json::json!({})),
        ApiError::InvalidJson => ("Invalid JSON body".to_string(), serde_json::json!({})),
        ApiError::NotFound { entity_id } => (
            "Entity not found".to_string(),
            serde_json::json!({ "entityId": entity_id }),
        ),
        ApiError::EtagMismatch {
            entity_id,
            current,
            provided,
        } => (
            "ETag does not match current version".to_string(),
            serde_json::json!({
                "entityId": entity_id,
                "currentEtag": current,
                "providedIfMatch": provided
            }),
        ),
        ApiError::Internal => ("Internal error".to_string(), serde_json::json!({})),
    };

    // Log (LLM-friendly JSON)
    error!(
        event = "http.response",
        result = "fail",
        status = %status.as_u16(),
        errorCode = %code,
        traceId = %trace_id,
        message = %message
    );

    let body = ErrorBody {
        error: ErrorInner {
            code: code.to_string(),
            message,
            details,
            trace_id: trace_id.clone(),
        },
    };

    let mut resp = (status, Json(body)).into_response();
    resp.headers_mut().insert(
        "x-trace-id",
        HeaderValue::from_str(&trace_id)
            .unwrap_or_else(|_| HeaderValue::from_static("invalid-trace-id")),
    );
    resp
}

// ----------------------------
// Storage trait
// ----------------------------
#[async_trait]
trait Storage: Send + Sync + 'static {
    async fn get(&self, id: &str) -> Result<Option<StoredEntity>, ApiError>;
    async fn upsert(
        &self,
        id: &str,
        doc: EntityDoc,
        if_match: Option<&str>,
    ) -> Result<StoredEntity, ApiError>;
    async fn patch(
        &self,
        id: &str,
        patch_doc: JsonValue,
        if_match: Option<&str>,
    ) -> Result<StoredEntity, ApiError>;
}

// ----------------------------
// In-memory storage
// ----------------------------
#[derive(Default)]
struct InMemoryStorage {
    map: RwLock<HashMap<String, StoredEntity>>,
}

#[async_trait]
impl Storage for InMemoryStorage {
    async fn get(&self, id: &str) -> Result<Option<StoredEntity>, ApiError> {
        Ok(self.map.read().unwrap().get(id).cloned())
    }

    async fn upsert(
        &self,
        id: &str,
        mut doc: EntityDoc,
        if_match: Option<&str>,
    ) -> Result<StoredEntity, ApiError> {
        // Enforce id from path
        doc.id = id.to_string();

        let mut map = self.map.write().unwrap();
        let existing = map.get(id).cloned();

        if let Some(ex) = existing.clone() {
            // If-Match check
            if let Some(provided) = if_match {
                let current = etag_for(ex.version);
                if provided != current {
                    return Err(ApiError::EtagMismatch {
                        entity_id: id.to_string(),
                        current,
                        provided: provided.to_string(),
                    });
                }
            }

            let stored = StoredEntity {
                doc,
                version: ex.version + 1,
                updated_at_ms: now_ms(),
            };
            map.insert(id.to_string(), stored.clone());
            Ok(stored)
        } else {
            // Creating new entity: if If-Match present, treat as mismatch (v0 behavior)
            if let Some(provided) = if_match {
                return Err(ApiError::EtagMismatch {
                    entity_id: id.to_string(),
                    current: "v0".to_string(),
                    provided: provided.to_string(),
                });
            }

            let stored = StoredEntity {
                doc,
                version: 1,
                updated_at_ms: now_ms(),
            };
            map.insert(id.to_string(), stored.clone());
            Ok(stored)
        }
    }

    async fn patch(
        &self,
        id: &str,
        patch_doc: JsonValue,
        if_match: Option<&str>,
    ) -> Result<StoredEntity, ApiError> {
        let mut map = self.map.write().unwrap();

        let existing = map.get(id).cloned().ok_or_else(|| ApiError::NotFound {
            entity_id: id.to_string(),
        })?;

        // If-Match check
        if let Some(provided) = if_match {
            let current = etag_for(existing.version);
            if provided != current {
                return Err(ApiError::EtagMismatch {
                    entity_id: id.to_string(),
                    current,
                    provided: provided.to_string(),
                });
            }
        }

        // Rebuild as JSON object for JSON Merge Patch:
        // { id, type, ...extra }
        let mut base = serde_json::Map::<String, JsonValue>::new();
        base.insert("id".to_string(), JsonValue::String(existing.doc.id.clone()));
        base.insert(
            "type".to_string(),
            JsonValue::String(existing.doc.entity_type.clone()),
        );
        for (k, v) in existing.doc.extra.iter() {
            base.insert(k.clone(), v.clone());
        }
        let mut base_val = JsonValue::Object(base);

        // Apply JSON Merge Patch (RFC 7396)
        json_patch::merge(&mut base_val, &patch_doc);

        // Validate required fields after merge
        let obj = base_val.as_object().ok_or(ApiError::InvalidJson)?;
        let new_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or(ApiError::InvalidJson)?;

        // Re-split into EntityDoc (enforce id from path)
        let mut extra: HashMap<String, JsonValue> = HashMap::new();
        for (k, v) in obj.iter() {
            if k != "id" && k != "type" {
                extra.insert(k.clone(), v.clone());
            }
        }

        let new_doc = EntityDoc {
            id: id.to_string(),
            entity_type: new_type.to_string(),
            extra,
        };

        let stored = StoredEntity {
            doc: new_doc,
            version: existing.version + 1,
            updated_at_ms: now_ms(),
        };

        map.insert(id.to_string(), stored.clone());
        Ok(stored)
    }
}

// ----------------------------
// Postgres storage
// ----------------------------
struct PostgresStorage {
    pool: PgPool,
}

impl PostgresStorage {
    async fn connect(database_url: &str) -> Result<Self, ApiError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await
            .map_err(|e| {
                error!(event = "pg.connect", result = "fail", err = %e);
                ApiError::Internal
            })?;

        // Minimal bootstrap (safe to run multiple times)
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS entities (
              id TEXT PRIMARY KEY,
              entity_type TEXT NOT NULL,
              doc JSONB NOT NULL,
              version BIGINT NOT NULL,
              updated_at_ms BIGINT NOT NULL
            )
            "#,
        )
        .execute(&pool)
        .await
        .map_err(|e| {
            error!(event = "pg.migrate", result = "fail", err = %e);
            ApiError::Internal
        })?;

        sqlx::query("CREATE INDEX IF NOT EXISTS entities_type_idx ON entities (entity_type)")
            .execute(&pool)
            .await
            .map_err(|e| {
                error!(event = "pg.migrate", result = "fail", err = %e);
                ApiError::Internal
            })?;

        Ok(Self { pool })
    }
}

#[async_trait]
impl Storage for PostgresStorage {
    async fn get(&self, id: &str) -> Result<Option<StoredEntity>, ApiError> {
        let row = sqlx::query("SELECT doc, version, updated_at_ms FROM entities WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| {
                error!(event = "pg.get", result = "fail", err = %e, entityId = %id);
                ApiError::Internal
            })?;

        let Some(row) = row else {
            return Ok(None);
        };

        let doc_val: JsonValue = row.try_get("doc").map_err(|_| ApiError::Internal)?;
        let version: i64 = row.try_get("version").map_err(|_| ApiError::Internal)?;
        let updated_at_ms: i64 = row.try_get("updated_at_ms").map_err(|_| ApiError::Internal)?;

        let doc: EntityDoc = serde_json::from_value(doc_val).map_err(|_| ApiError::InvalidJson)?;
        Ok(Some(StoredEntity {
            doc,
            version: version as u64,
            updated_at_ms,
        }))
    }

    async fn upsert(
        &self,
        id: &str,
        mut doc: EntityDoc,
        if_match: Option<&str>,
    ) -> Result<StoredEntity, ApiError> {
        // Enforce id from path
        doc.id = id.to_string();

        let now = now_ms();
        let doc_val = serde_json::to_value(&doc).map_err(|_| ApiError::InvalidJson)?;

        let mut tx = self.pool.begin().await.map_err(|e| {
            error!(event = "pg.tx.begin", result = "fail", err = %e);
            ApiError::Internal
        })?;

        let current = sqlx::query("SELECT version FROM entities WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                error!(event = "pg.upsert.select", result = "fail", err = %e, entityId = %id);
                ApiError::Internal
            })?;

        let stored = if let Some(row) = current {
            let cur_version: i64 = row.try_get("version").map_err(|_| ApiError::Internal)?;
            if let Some(provided) = if_match {
                let cur_etag = etag_for(cur_version as u64);
                if provided != cur_etag {
                    return Err(ApiError::EtagMismatch {
                        entity_id: id.to_string(),
                        current: cur_etag,
                        provided: provided.to_string(),
                    });
                }
            }

            let row = sqlx::query(
                "UPDATE entities SET entity_type = $2, doc = $3, version = version + 1, updated_at_ms = $4 WHERE id = $1 RETURNING doc, version, updated_at_ms",
            )
            .bind(id)
            .bind(&doc.entity_type)
            .bind(&doc_val)
            .bind(now)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                error!(event = "pg.upsert.update", result = "fail", err = %e, entityId = %id);
                ApiError::Internal
            })?;

            let doc_val: JsonValue = row.try_get("doc").map_err(|_| ApiError::Internal)?;
            let version: i64 = row.try_get("version").map_err(|_| ApiError::Internal)?;
            let updated_at_ms: i64 = row.try_get("updated_at_ms").map_err(|_| ApiError::Internal)?;
            let doc: EntityDoc = serde_json::from_value(doc_val).map_err(|_| ApiError::InvalidJson)?;

            StoredEntity {
                doc,
                version: version as u64,
                updated_at_ms,
            }
        } else {
            if let Some(provided) = if_match {
                return Err(ApiError::EtagMismatch {
                    entity_id: id.to_string(),
                    current: "v0".to_string(),
                    provided: provided.to_string(),
                });
            }

            let row = sqlx::query(
                "INSERT INTO entities (id, entity_type, doc, version, updated_at_ms) VALUES ($1,$2,$3,1,$4) RETURNING doc, version, updated_at_ms",
            )
            .bind(id)
            .bind(&doc.entity_type)
            .bind(&doc_val)
            .bind(now)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| {
                error!(event = "pg.upsert.insert", result = "fail", err = %e, entityId = %id);
                ApiError::Internal
            })?;

            let doc_val: JsonValue = row.try_get("doc").map_err(|_| ApiError::Internal)?;
            let version: i64 = row.try_get("version").map_err(|_| ApiError::Internal)?;
            let updated_at_ms: i64 = row.try_get("updated_at_ms").map_err(|_| ApiError::Internal)?;
            let doc: EntityDoc = serde_json::from_value(doc_val).map_err(|_| ApiError::InvalidJson)?;

            StoredEntity {
                doc,
                version: version as u64,
                updated_at_ms,
            }
        };

        tx.commit().await.map_err(|e| {
            error!(event = "pg.tx.commit", result = "fail", err = %e);
            ApiError::Internal
        })?;

        Ok(stored)
    }

    async fn patch(
        &self,
        id: &str,
        patch_doc: JsonValue,
        if_match: Option<&str>,
    ) -> Result<StoredEntity, ApiError> {
        let now = now_ms();

        let mut tx = self.pool.begin().await.map_err(|e| {
            error!(event = "pg.tx.begin", result = "fail", err = %e);
            ApiError::Internal
        })?;

        let row = sqlx::query("SELECT doc, version FROM entities WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| {
                error!(event = "pg.patch.select", result = "fail", err = %e, entityId = %id);
                ApiError::Internal
            })?
            .ok_or_else(|| ApiError::NotFound {
                entity_id: id.to_string(),
            })?;

        let mut base_val: JsonValue = row.try_get("doc").map_err(|_| ApiError::Internal)?;
        let cur_version: i64 = row.try_get("version").map_err(|_| ApiError::Internal)?;

        if let Some(provided) = if_match {
            let cur_etag = etag_for(cur_version as u64);
            if provided != cur_etag {
                return Err(ApiError::EtagMismatch {
                    entity_id: id.to_string(),
                    current: cur_etag,
                    provided: provided.to_string(),
                });
            }
        }

        // Apply JSON Merge Patch (RFC 7396)
        json_patch::merge(&mut base_val, &patch_doc);

        // Validate required fields after merge
        let obj = base_val.as_object().ok_or(ApiError::InvalidJson)?;
        let new_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or(ApiError::InvalidJson)?;

        // Rebuild EntityDoc (enforce id from path)
        let mut extra: HashMap<String, JsonValue> = HashMap::new();
        for (k, v) in obj.iter() {
            if k != "id" && k != "type" {
                extra.insert(k.clone(), v.clone());
            }
        }

        let new_doc = EntityDoc {
            id: id.to_string(),
            entity_type: new_type.to_string(),
            extra,
        };

        let doc_val = serde_json::to_value(&new_doc).map_err(|_| ApiError::InvalidJson)?;

        let row = sqlx::query(
            "UPDATE entities SET entity_type = $2, doc = $3, version = version + 1, updated_at_ms = $4 WHERE id = $1 RETURNING doc, version, updated_at_ms",
        )
        .bind(id)
        .bind(&new_doc.entity_type)
        .bind(&doc_val)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| {
            error!(event = "pg.patch.update", result = "fail", err = %e, entityId = %id);
            ApiError::Internal
        })?;

        tx.commit().await.map_err(|e| {
            error!(event = "pg.tx.commit", result = "fail", err = %e);
            ApiError::Internal
        })?;

        let doc_val: JsonValue = row.try_get("doc").map_err(|_| ApiError::Internal)?;
        let version: i64 = row.try_get("version").map_err(|_| ApiError::Internal)?;
        let updated_at_ms: i64 = row.try_get("updated_at_ms").map_err(|_| ApiError::Internal)?;
        let doc: EntityDoc = serde_json::from_value(doc_val).map_err(|_| ApiError::InvalidJson)?;

        Ok(StoredEntity {
            doc,
            version: version as u64,
            updated_at_ms,
        })
    }
}

// ----------------------------
// App State
// ----------------------------
#[derive(Clone)]
struct AppState {
    storage: Arc<dyn Storage>,
    api_key: Option<String>,
    allow_insecure_dev: bool,
}

// ----------------------------
// Handlers
// ----------------------------
async fn get_entity(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let trace_id = trace_id_from(&headers);

    match state.storage.get(&id).await {
        Ok(Some(stored)) => {
            let mut resp = Json(stored.doc).into_response();
            resp.headers_mut().insert(
                "etag",
                HeaderValue::from_str(&etag_for(stored.version)).unwrap(),
            );
            resp.headers_mut().insert(
                "x-trace-id",
                HeaderValue::from_str(&trace_id).unwrap(),
            );
            resp
        }
        Ok(None) => api_error_response(ApiError::NotFound { entity_id: id }, trace_id),
        Err(e) => api_error_response(e, trace_id),
    }
}

async fn put_entity(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(payload): Json<JsonValue>,
) -> Response {
    let trace_id = trace_id_from(&headers);

    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().trim_matches('"').to_string());

    // Validate minimal required fields
    let entity_type = payload
        .get("type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    if entity_type.is_none() {
        return api_error_response(ApiError::InvalidJson, trace_id);
    }

    let mut extra: HashMap<String, JsonValue> = HashMap::new();
    if let Some(obj) = payload.as_object() {
        for (k, v) in obj.iter() {
            if k != "id" && k != "type" {
                extra.insert(k.clone(), v.clone());
            }
        }
    } else {
        return api_error_response(ApiError::InvalidJson, trace_id);
    }

    let doc = EntityDoc {
        id: id.clone(),
        entity_type: entity_type.unwrap(),
        extra,
    };

    match state
        .storage
        .upsert(&id, doc, if_match.as_deref())
        .await
    {
        Ok(stored) => {
            info!(
                event = "entity.upsert",
                result = "ok",
                traceId = %trace_id,
                entityId = %id,
                version = %stored.version
            );
            let mut resp = StatusCode::OK.into_response();
            resp.headers_mut().insert(
                "etag",
                HeaderValue::from_str(&etag_for(stored.version)).unwrap(),
            );
            resp.headers_mut().insert(
                "x-trace-id",
                HeaderValue::from_str(&trace_id).unwrap(),
            );
            resp
        }
        Err(e) => api_error_response(e, trace_id),
    }
}

async fn patch_entity(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(patch_doc): Json<JsonValue>,
) -> Response {
    let trace_id = trace_id_from(&headers);

    let if_match = headers
        .get("if-match")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().trim_matches('"').to_string());

    match state
        .storage
        .patch(&id, patch_doc, if_match.as_deref())
        .await
    {
        Ok(stored) => {
            info!(
                event = "entity.patch",
                result = "ok",
                traceId = %trace_id,
                entityId = %id,
                version = %stored.version
            );
            let mut resp = StatusCode::OK.into_response();
            resp.headers_mut().insert(
                "etag",
                HeaderValue::from_str(&etag_for(stored.version)).unwrap(),
            );
            resp.headers_mut().insert(
                "x-trace-id",
                HeaderValue::from_str(&trace_id).unwrap(),
            );
            resp
        }
        Err(e) => api_error_response(e, trace_id),
    }
}

// ----------------------------
// Middleware: API key guard (protects /v1)
// ----------------------------
async fn api_key_guard(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: Request<Body>,
    next: middleware::Next,
) -> Response {
    let trace_id = trace_id_from(&headers);

    let Some(expected) = state.api_key.as_deref() else {
        if state.allow_insecure_dev {
            return next.run(req).await;
        }
        return api_error_response(ApiError::Unauthorized, trace_id);
    };

    // Accept either `x-api-key: ...` or `authorization: ApiKey ...`
    let provided = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.trim().to_string())
                .and_then(|s| s.strip_prefix("ApiKey ").map(|v| v.trim().to_string()))
        });

    let Some(provided) = provided else {
        return api_error_response(ApiError::Unauthorized, trace_id);
    };

    if provided != expected {
        return api_error_response(ApiError::Forbidden, trace_id);
    }

    next.run(req).await
}
