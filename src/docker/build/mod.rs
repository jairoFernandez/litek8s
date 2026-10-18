//! `POST /build`: the classic (non-BuildKit) Docker builder.

pub mod dockerfile;

use std::collections::HashMap;
use std::sync::Arc;

use hyper::Request;
use hyper::body::Incoming;
use serde_json::{Value, json};

use super::api::Body;
use super::engine::Engine;
use super::error::{ApiError, ApiResult};

pub async fn handle(engine: Arc<Engine>, q: HashMap<String, String>, req: Request<Incoming>) -> ApiResult<hyper::Response<Body>> {
    let _ = (engine, q, req);
    Err(ApiError::new(501, "build is not implemented yet"))
}

pub fn prune(engine: &Engine) -> Value {
    let _ = engine;
    json!({"CachesDeleted": [], "SpaceReclaimed": 0})
}
