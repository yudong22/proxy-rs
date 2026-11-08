//! Request path for all three client protocols.
//!
//! This is the module root. The protocol handlers and the shared upstream
//! send/retry path were all in one 2,400-line file; they are now one file per
//! stage of the request, with the pre-existing clusters left where they were.
//!
//! ## Layout
//!
//! | file | owns |
//! |---|---|
//! | [`flavor`] | `ApiFlavor` — which wire protocol a request arrived on |
//! | [`handlers`] | the three axum handlers (Anthropic Messages, Responses, Chat) |
//! | [`request`] | request-side plumbing: body extraction, rejection, session resolution, stats, credential refresh |
//! | [`forward`] | the upstream send/retry path — one long function, see its own note |
//! | [`response`] | buffering the stream, non-streaming aggregation, streaming passthrough |
//! | [`safety_classifier`] | recognising Claude Code's auto-mode classifier, and the local `allow` verdict |
//! | [`models`] | `GET /v1/models` and its config-driven variants |
//! | [`auth_resolvers`] | which API key the caller presented |
//! | [`auth_headers`] | upstream identity + CLI fingerprint headers |
//! | [`failover`] | when to relocate a session off the configured default |
//! | [`upstream_errors`] | classifying, describing and repairing upstream failures |
//! | [`sse`] | SSE framing, the request-log ledger and the TTFT/speed tracker |
//!
//! Everything is re-exported below under its original name, so every
//! pre-existing `crate::proxy::X` path (and the tests' `super::X`) keeps
//! resolving unchanged — the split is a file-layout change, not an API change.

mod auth_headers;
mod auth_resolvers;
mod failover;
mod flavor;
mod forward;
mod handlers;
mod models;
mod request;
mod response;
mod safety_classifier;
mod sse;
mod upstream_errors;

#[cfg(test)]
mod tests;

// Bring the split-out items back into this module's namespace under their
// historical names, so every existing `crate::proxy::X` path and the tests'
// `super::X` keep resolving. `pub(crate) use` is required, not cosmetic: the test
// module is a *sibling* of these files (`proxy/tests.rs`), so `super::X` resolves
// through `crate::proxy` and a private import would not be visible to it.
pub(crate) use auth_headers::*;
pub(crate) use auth_resolvers::*;
pub(crate) use failover::*;
pub(crate) use flavor::*;
pub(crate) use forward::*;
pub(crate) use handlers::*;
pub(crate) use models::*;
pub(crate) use request::*;
pub(crate) use response::*;
pub(crate) use safety_classifier::*;
pub(crate) use sse::*;
pub(crate) use upstream_errors::*;

use crate::config::{Config, ModelsFlavor};
use crate::error::{ProxyError, ProxyResult};
use crate::metrics;
use crate::models::{anthropic, openai, responses};
use crate::service;
use crate::session::{self, ClientKind, SessionInfo};
use crate::stats::{RequestOutcome, StatsDb, TokenRecord, UpstreamTiming};
use crate::translate::{pipeline, responses as responses_pipeline, stream};
use crate::util::{self, format_headers, truncate};
use axum::{
    body::Body,
    extract::{FromRequest, Request},
    http::{HeaderMap, HeaderName, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use reqwest::Client;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
fn json_request<T: serde::Serialize>(value: &T) -> Request {
    Request::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(value).expect("test body serializes"),
        ))
        .expect("test request builds")
}
