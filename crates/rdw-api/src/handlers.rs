//! The `GET /api/v1/fuel` handler: validate, rate-limit, guard concurrency,
//! fetch/merge/widen, assemble CSV or ZIP, and respond only once the whole
//! export has succeeded. Any failure along the way returns an error and
//! sends no CSV bytes at all.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, RawQuery, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::errors::{render_to_response, ApiError};
use crate::extractors::{extract_api_key, parse_brands, parse_limit};
use crate::pipeline::{fetch_and_widen, PipelineError};
use crate::state::AppState;
use rdw_core::{Assembled, FuelFailureSummary, RateLimitOutcome, RowWidener};

/// Response header naming the fuel-failure count and affected vehicle count
/// on a partial (degraded) export. A browser download never surfaces
/// response headers to the user, so this is a supplementary machine-readable
/// signal; the filename (see `build_response`) is what a human sees.
const EXPORT_WARNINGS_HEADER: &str = "x-export-warnings";

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_raw_query(raw: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    if let Some(raw) = raw {
        for (k, v) in url::form_urlencoded::parse(raw.as_bytes()) {
            map.insert(k.into_owned(), v.into_owned());
        }
    }
    map
}

pub async fn fuel_handler(
    State(state): State<Arc<AppState>>,
    // Client IP is accepted for a future anonymous-request rate-limit
    // fallback, but this endpoint always requires a validated API key, so
    // it is not currently read.
    ConnectInfo(_addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
) -> Response {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let params = parse_raw_query(raw_query.as_deref());

    let result = run(&state, &headers, &params).await;
    match result {
        Ok(response) => response,
        Err(err) => render_to_response(&err, accept.as_deref()),
    }
}

async fn run(
    state: &AppState,
    headers: &HeaderMap,
    params: &std::collections::HashMap<String, String>,
) -> Result<Response, ApiError> {
    // 1. API key: header takes precedence over query param.
    let header_key = headers.get("X-Api-Key").and_then(|v| v.to_str().ok());
    let query_key = params.get("api_key").map(String::as_str);
    let api_key = extract_api_key(header_key, query_key).ok_or(ApiError::Unauthorized)?;
    if !state.valid_api_keys.contains(&api_key) {
        return Err(ApiError::Unauthorized);
    }

    // 2. Query validation.
    let brands = parse_brands(params.get("brands").map(String::as_str))
        .map_err(|e| ApiError::BadRequest(e.0))?;
    let limit = parse_limit(params.get("limit").map(String::as_str))
        .map_err(|e| ApiError::BadRequest(e.0))?;

    // 3. Rate limit: fixed window, keyed by API key.
    let now = now_unix();
    state.rate_limiter.evict_stale(now);
    let rate_key = format!("key:{api_key}");
    match state.rate_limiter.check_and_record(&rate_key, now) {
        RateLimitOutcome::DayExceeded => {
            return Err(ApiError::RateLimited {
                retry_after_secs: seconds_until_next_day(now),
            })
        }
        RateLimitOutcome::WeekExceeded => {
            return Err(ApiError::RateLimited {
                retry_after_secs: seconds_until_next_week(now),
            })
        }
        RateLimitOutcome::Allowed => {}
    }

    // 4. Concurrency guard: only one export at a time.
    let _permit = match state.export_lock.try_acquire() {
        Ok(permit) => permit,
        Err(_) => return Err(ApiError::Busy),
    };

    // 5. Fetch, merge-join, widen. Rows are staged to a temp file one page
    // at a time via `assembler` so the whole export is never held in
    // memory; the assembler's `finish()` is only ever called after every
    // page has succeeded (step 6), preserving the never-partial guarantee.
    let widener = RowWidener::new(
        state.metadata.vehicle_columns.clone(),
        state.metadata.fuel_columns.clone(),
    );
    let mut assembler = rdw_core::Assembler::new(widener.header());
    let summary = match fetch_and_widen(
        &state.client,
        &brands,
        limit,
        &widener,
        &mut assembler,
        &state.failure_config,
    )
    .await
    {
        Ok(summary) => summary,
        Err(err) => {
            assembler.abort();
            let api_err = pipeline_error_to_api_error(&err);
            if matches!(
                api_err,
                ApiError::BadGateway(_) | ApiError::GatewayTimeout(_)
            ) {
                // A 502/504 upstream failure (vehicle-page failure, or the
                // fuel-failure threshold exceeded) must not consume quota.
                state.rate_limiter.release(&rate_key, now);
            }
            return Err(api_err);
        }
    };

    // 6. Close staging. A fuel-range failure below the abort threshold still
    // produces a valid, deliberately degraded CSV/ZIP (status column,
    // filename, and header all mark it); only a vehicle-page failure or an
    // above-threshold fuel-failure rate reaches step 5's abort path above.
    let assembled = assembler.finish_with_report(&summary).map_err(|e| {
        state.rate_limiter.release(&rate_key, now);
        ApiError::BadGateway(format!("failed to assemble export: {e}"))
    })?;

    // Reading the staged file back can still fail, and that is a 502 like any
    // other upstream failure, so it must release quota on the same terms
    // rather than charging the caller for an export they never received.
    let response = build_response(&assembled, &summary).inspect_err(|_| {
        state.rate_limiter.release(&rate_key, now);
    });
    rdw_core::cleanup(&assembled);
    response
}

fn pipeline_error_to_api_error(err: &PipelineError) -> ApiError {
    match err {
        PipelineError::Timeout => ApiError::GatewayTimeout("RDW request timed out".to_string()),
        PipelineError::Upstream(msg) => ApiError::BadGateway(msg.clone()),
        PipelineError::DataIntegrity(msg) => {
            ApiError::BadGateway(format!("data integrity check failed: {msg}"))
        }
        PipelineError::Assembly(msg) => {
            ApiError::BadGateway(format!("failed to assemble export: {msg}"))
        }
        PipelineError::FuelFailureThresholdExceeded(msg) => {
            ApiError::BadGateway(format!("too many fuel range fetches failed: {msg}"))
        }
    }
}

/// Build the export response. On any fuel failure the filename is marked
/// `-PARTIAL` (a browser download surfaces the filename, never response
/// headers, so this is the layer a human actually sees) and the
/// `X-Export-Warnings` header names the failure and affected-vehicle
/// counts, for API clients that inspect headers rather than the CSV's
/// `export_status` column.
fn build_response(
    assembled: &Assembled,
    summary: &FuelFailureSummary,
) -> Result<Response, ApiError> {
    let bytes = std::fs::read(assembled.path())
        .map_err(|e| ApiError::BadGateway(format!("failed to read export file: {e}")))?;

    let (content_type, extension) = match assembled {
        Assembled::Csv { .. } => ("text/csv", "csv"),
        Assembled::Zip { .. } => ("application/zip", "zip"),
    };
    let partial = summary.has_failures();
    let filename = if partial {
        format!("fuel-export-PARTIAL.{extension}")
    } else {
        format!("fuel-export.{extension}")
    };

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from(bytes.len() as u64),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    if partial {
        let value = format!(
            "fuel_failures={} vehicles_affected={}",
            summary.failures, summary.vehicles_affected
        );
        if let Ok(hv) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static(EXPORT_WARNINGS_HEADER), hv);
        }
    }

    Ok((StatusCode::OK, headers, bytes).into_response())
}

fn seconds_until_next_day(now_unix: i64) -> u64 {
    const DAY: i64 = 86_400;
    let next_day = (now_unix.div_euclid(DAY) + 1) * DAY;
    (next_day - now_unix).max(0) as u64
}

fn seconds_until_next_week(now_unix: i64) -> u64 {
    const DAY: i64 = 86_400;
    // Weeks are Monday-aligned; epoch day 0 (Thursday) is day-index 3 of
    // its week, matching `rate_limit::week_key`'s +3 shift.
    let day_index = now_unix.div_euclid(DAY);
    let week_start_day = ((day_index + 3).div_euclid(7)) * 7 - 3;
    let next_week_start = (week_start_day + 7) * DAY;
    (next_week_start - now_unix).max(0) as u64
}
