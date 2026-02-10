use axum::{
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
    sync::{Arc, RwLock},
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tracing::{error, info};
use uuid::Uuid;

static VERSION: &str = "0.1.0";

#[tokio::main]
async fn main() {
    init_tracing();

    let state = AppState {
        storage: Arc::new(InMemoryStorage::default()),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/version", get(version))
        .route("/v1/entities/:id", put(put_entity).get(get_entity))
        .with_state(state);

    let addr = "0.0.0.0:8080";
    info!(event="startup", addr, version=VERSION, "AIRA Platform starting");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("info".parse().unwrap()))
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
    #[allow(dead_code)]
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
            ApiError::InvalidJson => ("INVALID_JSON", StatusCode::BAD_REQUEST),
            ApiError::NotFound { .. } => ("ENTITY_NOT_FOUND", StatusCode::NOT_FOUND),
            ApiError::EtagMismatch { .. } => ("ENTITY_ETAG_MISMATCH", StatusCode::PRECONDITION_FAILED),
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
        event="http.response",
        result="fail",
        status=%status.as_u16(),
        errorCode=%code,
        traceId=%trace_id,
        message=%message
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
        HeaderValue::from_str(&trace_id).unwrap_or(HeaderValue::from_static("invalid-trace-id")),
    );
    resp
}

// ----------------------------
// Storage trait + in-memory impl
// ----------------------------
trait Storage: Send + Sync + 'static {
    fn get(&self, id: &str) -> Result<Option<StoredEntity>, ApiError>;
    fn upsert(&self, id: &str, doc: EntityDoc, if_match: Option<&str>) -> Result<StoredEntity, ApiError>;
}

#[derive(Default)]
struct InMemoryStorage {
    // id -> StoredEntity
    map: RwLock<HashMap<String, StoredEntity>>,
}

impl Storage for InMemoryStorage {
    fn get(&self, id: &str) -> Result<Option<StoredEntity>, ApiError> {
        Ok(self.map.read().unwrap().get(id).cloned())
    }

    fn upsert(&self, id: &str, mut doc: EntityDoc, if_match: Option<&str>) -> Result<StoredEntity, ApiError> {
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
            let new_version = ex.version + 1;
            let stored = StoredEntity {
                doc,
                version: new_version,
                updated_at_ms: now_ms(),
            };
            map.insert(id.to_string(), stored.clone());
            Ok(stored)
        } else {
            // Creating new entity: if If-Match present, we can treat as mismatch
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
                #[allow(dead_code)]
                updated_at_ms: now_ms(),
            };
            map.insert(id.to_string(), stored.clone());
            Ok(stored)
        }
    }
}

// ----------------------------
// App State
// ----------------------------
#[derive(Clone)]
struct AppState {
    storage: Arc<dyn Storage>,
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

    match state.storage.get(&id) {
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
    let entity_type = payload.get("type").and_then(|v| v.as_str()).map(|s| s.to_string());
    if entity_type.is_none() {
        return api_error_response(ApiError::InvalidJson, trace_id);
    }

    let mut extra = HashMap::new();
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

    match state.storage.upsert(&id, doc, if_match.as_deref()) {
        Ok(stored) => {
            info!(
                event="entity.upsert",
                result="ok",
                traceId=%trace_id,
                entityId=%id,
                version=%stored.version
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
