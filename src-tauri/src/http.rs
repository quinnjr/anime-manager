//! Shared HTTP retry/backoff and body helpers used by the torrent and Nyaa
//! clients: one retry ladder, one snippet truncator, one capped body reader.

use std::time::Duration;

use crate::error::{AppError, Result};

/// Backoff before retry `attempt` (1-based): 200ms, then 800ms, plus a
/// sub-100ms jitter so concurrent hunts do not march in step.
pub fn retry_wait(attempt: u32) -> Duration {
    let base = if attempt <= 1 { 200 } else { 800 };
    let jitter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.subsec_nanos() % 100) as u64)
        .unwrap_or(0);
    Duration::from_millis(base + jitter)
}

/// Read at most `max` characters of a response body for an error message.
pub async fn snippet(resp: reqwest::Response, max: usize) -> String {
    resp.text().await.unwrap_or_default().chars().take(max).collect()
}

/// Run one request, retrying 429/5xx up to `attempts` times with the
/// `retry_wait` ladder and honouring `Retry-After` (capped at 30s).
///
/// `retry_on_timeout` also retries a transport timeout — correct for idempotent
/// GETs, wrong for a create (POST) that the server may have already accepted.
pub async fn send_with_retry<F, Fut>(
    attempts: u32,
    retry_on_timeout: bool,
    mk: F,
) -> Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<reqwest::Response, reqwest::Error>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let resp = match mk().await {
            Ok(resp) => resp,
            Err(e) if e.is_timeout() && retry_on_timeout && attempt < attempts => {
                tokio::time::sleep(retry_wait(attempt)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let status = resp.status();
        if (status.as_u16() == 429 || status.is_server_error()) && attempt < attempts {
            let asked = resp
                .headers()
                .get("retry-after")
                .and_then(|h| h.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.min(30)));
            tokio::time::sleep(asked.unwrap_or_else(|| retry_wait(attempt))).await;
            continue;
        }
        return Ok(resp);
    }
}

/// Buffer a response body, refusing one larger than `max` bytes. The declared
/// `Content-Length` is checked first, then the stream is aborted mid-read once
/// the running total crosses `max`, so an oversized or lying server cannot
/// exhaust memory.
pub async fn read_capped(mut resp: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if let Some(len) = resp.content_length()
        && len > max as u64
    {
        return Err(AppError::Network("response body too large".into()));
    }
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() + chunk.len() > max {
            return Err(AppError::Network("response body too large".into()));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}
