// Apr 29, 2026 — axum HTTP streaming endpoint for the librqbit-backed torrent
// engine. Replaces webtorrent's separate `:8888` HTTP server with a route on
// spela's existing axum router (`:7890`). ffmpeg is the only consumer; it
// issues `Range: bytes=N-` requests as it transcodes, and librqbit
// re-prioritizes pieces around the requested offset.
//
// Phase 1 (this commit): module compiles, Range parser unit-tested, response
// builder unit-tested. Wiring into the axum router lives in Phase 2 — see the
// `torrent_engine.rs` header for the migration plan.
//
// Why a separate module rather than another `server.rs` handler:
// `server.rs` is already 4000 lines. The torrent streaming handler has a
// distinct testable concern (HTTP Range semantics) and is small enough to
// keep self-contained.

use std::io::SeekFrom;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::torrent_engine::TorrentEngine;

/// Parsed Range request, both endpoints inclusive (RFC 7233 § 2.1).
/// `start <= end < total` always holds for a valid request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

impl ByteRange {
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    /// Whether this range covers the entire resource. When true, the response
    /// status is 200; otherwise 206 (partial content).
    pub fn is_full(&self, total: u64) -> bool {
        self.start == 0 && total > 0 && self.end == total - 1
    }
}

/// Parse an HTTP `Range:` header against a known total length. Returns the
/// effective byte range to serve. Behavior:
///
/// - Missing or malformed header: returns the full resource (`0..=total-1`)
/// - `bytes=N-`:    suffix open-ended → `N..=total-1`
/// - `bytes=N-M`:   bounded → `N..=min(M, total-1)`
/// - `bytes=-N`:    suffix length → last N bytes (`total-N..=total-1`)
/// - `bytes=N-` where N >= total: returns Err (caller should respond 416)
/// - Multi-range (`bytes=0-100,200-300`): only first range honored — multi-part
///   responses aren't supported by ffmpeg or Chromecast in our pipeline.
///
/// `total` MUST be > 0; a zero-length file would never reach this code path.
pub fn parse_range_header(
    raw: Option<&HeaderValue>,
    total: u64,
) -> Result<ByteRange, RangeError> {
    if total == 0 {
        return Err(RangeError::EmptyResource);
    }
    let raw = match raw {
        Some(h) => h,
        None => {
            return Ok(ByteRange {
                start: 0,
                end: total - 1,
            })
        }
    };
    let s = raw.to_str().map_err(|_| RangeError::Malformed)?;
    let s = s.strip_prefix("bytes=").ok_or(RangeError::Malformed)?;
    // Multi-range: take first only.
    let first = s.split(',').next().unwrap_or(s).trim();
    let (start_part, end_part) = first
        .split_once('-')
        .ok_or(RangeError::Malformed)?;

    let last = total - 1;
    if start_part.is_empty() {
        // Suffix range: `bytes=-N` -> last N bytes.
        let suffix: u64 = end_part.parse().map_err(|_| RangeError::Malformed)?;
        if suffix == 0 {
            return Err(RangeError::Unsatisfiable);
        }
        let suffix = suffix.min(total);
        return Ok(ByteRange {
            start: total - suffix,
            end: last,
        });
    }
    let start: u64 = start_part.parse().map_err(|_| RangeError::Malformed)?;
    if start >= total {
        return Err(RangeError::Unsatisfiable);
    }
    let end = if end_part.is_empty() {
        last
    } else {
        let parsed: u64 = end_part.parse().map_err(|_| RangeError::Malformed)?;
        parsed.min(last)
    };
    if end < start {
        return Err(RangeError::Malformed);
    }
    Ok(ByteRange { start, end })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeError {
    /// Header was present but not a valid `bytes=...` form.
    Malformed,
    /// Range was syntactically valid but cannot be satisfied (e.g. start past
    /// EOF). Maps to HTTP 416 Range Not Satisfiable.
    Unsatisfiable,
    /// File has zero length — a programming error in spela given that we only
    /// stream resolved torrents.
    EmptyResource,
}

impl RangeError {
    pub fn http_status(&self) -> StatusCode {
        match self {
            RangeError::Malformed => StatusCode::BAD_REQUEST,
            RangeError::Unsatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
            RangeError::EmptyResource => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// Build the axum Response for a torrent stream. Pulled out as a separate
/// async function so the axum-handler-facing entry point is a thin one-liner
/// and so this can be tested via integration tests against a real torrent
/// engine. Pure helper — does not depend on shared state.
pub async fn serve_torrent_stream(
    engine: &TorrentEngine,
    id: u32,
    file_idx: usize,
    headers: &HeaderMap,
) -> Result<Response, StatusCode> {
    let handle = engine.handle(id).ok_or_else(|| {
        tracing::warn!("torrent_stream: torrent {} not found in session", id);
        StatusCode::NOT_FOUND
    })?;
    // `handle.stream(file_idx)` returns `librqbit::FileStream`, whose concrete
    // type can't be named outside the librqbit crate (the `torrent_state`
    // module that owns it is private at the crate root). We use it through
    // `AsyncRead + AsyncSeek + .len()` only, which is fine — the value never
    // crosses our function boundary.
    //
    // May 1, 2026 (Wilderpeople movie-night fifth bug): retry on
    // "initializing" state. librqbit's `start()` returns when the torrent
    // is *added to the session*, but the storage/file backing isn't ready
    // until initial-checksum-validation completes (1-3s for cached files,
    // longer for fresh downloads). spela's do_play kicks off ffmpeg
    // immediately after start() returns; if ffmpeg's first HTTP GET
    // arrives during the init window, librqbit returns
    // `with_storage_and_file: invalid state: initializing`, which this
    // function previously translated to 404. ffmpeg treats 404 as fatal
    // (no -reconnect retry on HTTP error codes), so the transcode
    // crashed and HLS pre-buffer timed out at 60s with zero segments —
    // exactly the original Wilderpeople movie-night failure mode after
    // a fresh spela restart. Fix: poll librqbit every 250ms for up to
    // 30s while it's in the initializing state, then return 503 (so
    // ffmpeg's `-reconnect` IS triggered as a last resort if init takes
    // even longer).
    let mut stream = {
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(30);
        loop {
            match handle.clone().stream(file_idx) {
                Ok(s) => break s,
                Err(err) => {
                    let msg = err.to_string();
                    if msg.contains("initializing")
                        && tokio::time::Instant::now() < deadline
                    {
                        tokio::time::sleep(
                            tokio::time::Duration::from_millis(250),
                        )
                        .await;
                        continue;
                    }
                    tracing::warn!(
                        "torrent_stream: handle.stream({}, {}) failed: {}",
                        id,
                        file_idx,
                        err
                    );
                    let status = if msg.contains("initializing") {
                        StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        StatusCode::NOT_FOUND
                    };
                    return Err(status);
                }
            }
        }
    };

    let total = stream.len();
    let range = parse_range_header(headers.get(header::RANGE), total)
        .map_err(|err| err.http_status())?;

    stream
        .seek(SeekFrom::Start(range.start))
        .await
        .map_err(|err| {
            tracing::warn!("torrent_stream: seek({}) failed: {}", range.start, err);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let len = range.len();
    let limited = stream.take(len);
    let body = Body::from_stream(ReaderStream::new(limited));

    let status = if range.is_full(total) {
        StatusCode::OK
    } else {
        StatusCode::PARTIAL_CONTENT
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len.to_string())
        // Chromecast's Default Media Receiver doesn't actually fetch this
        // endpoint directly — ffmpeg does, then transcodes to HLS. ffmpeg
        // sniffs container format from bytes, so the Content-Type is
        // hint-only. Matroska is the dominant container in our torrent set.
        .header(header::CONTENT_TYPE, "video/x-matroska");
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{}", range.start, range.end, total),
        );
    }
    builder
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hv(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn no_header_returns_full_range() {
        let r = parse_range_header(None, 1_000).unwrap();
        assert_eq!(r, ByteRange { start: 0, end: 999 });
        assert!(r.is_full(1_000));
    }

    #[test]
    fn bytes_eq_n_dash_open_ended() {
        let r = parse_range_header(Some(&hv("bytes=500-")), 1_000).unwrap();
        assert_eq!(r, ByteRange { start: 500, end: 999 });
        assert!(!r.is_full(1_000));
    }

    #[test]
    fn bytes_eq_n_dash_m_bounded() {
        let r = parse_range_header(Some(&hv("bytes=100-200")), 1_000).unwrap();
        assert_eq!(r, ByteRange { start: 100, end: 200 });
        assert_eq!(r.len(), 101);
    }

    #[test]
    fn bytes_eq_n_dash_m_clamps_end_to_resource() {
        let r = parse_range_header(Some(&hv("bytes=900-9999")), 1_000).unwrap();
        assert_eq!(r, ByteRange { start: 900, end: 999 });
    }

    #[test]
    fn suffix_range_returns_last_n() {
        let r = parse_range_header(Some(&hv("bytes=-100")), 1_000).unwrap();
        assert_eq!(r, ByteRange { start: 900, end: 999 });
    }

    #[test]
    fn suffix_range_larger_than_total_clamps() {
        let r = parse_range_header(Some(&hv("bytes=-99999")), 1_000).unwrap();
        // When suffix > total, return entire resource.
        assert_eq!(r, ByteRange { start: 0, end: 999 });
    }

    #[test]
    fn start_at_zero_explicit() {
        let r = parse_range_header(Some(&hv("bytes=0-")), 1_000).unwrap();
        assert_eq!(r, ByteRange { start: 0, end: 999 });
        // ffmpeg often issues `bytes=0-` to test Range support; treat as full.
        assert!(r.is_full(1_000));
    }

    #[test]
    fn malformed_header_no_bytes_prefix() {
        let err = parse_range_header(Some(&hv("items=0-100")), 1_000).unwrap_err();
        assert_eq!(err, RangeError::Malformed);
    }

    #[test]
    fn malformed_header_no_dash() {
        let err = parse_range_header(Some(&hv("bytes=100")), 1_000).unwrap_err();
        assert_eq!(err, RangeError::Malformed);
    }

    #[test]
    fn malformed_header_garbage_numbers() {
        let err = parse_range_header(Some(&hv("bytes=abc-def")), 1_000).unwrap_err();
        assert_eq!(err, RangeError::Malformed);
    }

    #[test]
    fn unsatisfiable_start_past_eof() {
        let err = parse_range_header(Some(&hv("bytes=99999-")), 1_000).unwrap_err();
        assert_eq!(err, RangeError::Unsatisfiable);
        assert_eq!(err.http_status(), StatusCode::RANGE_NOT_SATISFIABLE);
    }

    #[test]
    fn unsatisfiable_zero_suffix() {
        let err = parse_range_header(Some(&hv("bytes=-0")), 1_000).unwrap_err();
        assert_eq!(err, RangeError::Unsatisfiable);
    }

    #[test]
    fn malformed_end_before_start() {
        let err = parse_range_header(Some(&hv("bytes=500-100")), 1_000).unwrap_err();
        assert_eq!(err, RangeError::Malformed);
    }

    #[test]
    fn multi_range_takes_first() {
        // RFC 7233 allows multipart responses, but we only honor the first.
        // ffmpeg never sends multi-range; chromecast doesn't either.
        let r = parse_range_header(Some(&hv("bytes=0-100,200-300")), 1_000).unwrap();
        assert_eq!(r, ByteRange { start: 0, end: 100 });
    }

    #[test]
    fn whitespace_around_range_tolerated() {
        let r = parse_range_header(Some(&hv("bytes= 0-100 ")), 1_000).unwrap();
        assert_eq!(r, ByteRange { start: 0, end: 100 });
    }

    #[test]
    fn empty_resource_is_programming_error() {
        let err = parse_range_header(None, 0).unwrap_err();
        assert_eq!(err, RangeError::EmptyResource);
        assert_eq!(err.http_status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn is_full_distinguishes_partial() {
        let total = 1_000_u64;
        assert!(ByteRange { start: 0, end: 999 }.is_full(total));
        assert!(!ByteRange { start: 0, end: 998 }.is_full(total));
        assert!(!ByteRange { start: 1, end: 999 }.is_full(total));
        assert!(!ByteRange { start: 100, end: 200 }.is_full(total));
    }

    #[test]
    fn typical_ffmpeg_initial_probe() {
        // ffmpeg's first read is usually a small head probe — bytes=0-...
        // for ~8KB so it can identify the container. Make sure we serve 206
        // for a partial range starting at zero.
        let r = parse_range_header(Some(&hv("bytes=0-8191")), 4_500_000_000).unwrap();
        assert_eq!(r.start, 0);
        assert_eq!(r.end, 8191);
        assert!(!r.is_full(4_500_000_000));
        assert_eq!(r.len(), 8192);
    }

    #[test]
    fn typical_ffmpeg_resume_seek() {
        // ffmpeg resuming a transcode after smart-resume passes -ss N to
        // ffmpeg, which then issues `bytes=START-` where START maps to the
        // approximate byte offset. Range::len here matches what we'd serve.
        let total = 4_500_000_000_u64;
        let r = parse_range_header(Some(&hv("bytes=2250000000-")), total).unwrap();
        assert_eq!(r.start, 2_250_000_000);
        assert_eq!(r.end, total - 1);
        assert_eq!(r.len(), total - 2_250_000_000);
    }
}
