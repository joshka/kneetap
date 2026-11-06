use std::convert::Infallible;
use std::ops::Range;

use axum::body::{Body, Bytes};
use axum::extract::Query;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use axum::response::Response;
use futures_util::stream;
use itertools::Itertools;
use serde::Deserialize;

use crate::routes::Rejection;

/// Bytes generated at a time, which bounds memory however large the response is.
const CHUNK_SIZE: u64 = 64 * 1024;

/// Bytes in one line of the pattern: 15 hexadecimal digits and a newline.
const LINE_SIZE: u64 = 16;

/// The parameters `/data` owns, kept as text so [`parse_size`] can explain what's wrong with them.
#[derive(Debug, Deserialize)]
struct DataQuery {
    size: Option<String>,
    unit: Option<String>,

    #[serde(default)]
    attachment: bool,
}

/// Serves generated data of the requested size, without storing it.
///
/// Responses support validators and single ranges, so this can test resumable downloads. A
/// `Range` the server doesn't support is ignored, which HTTP allows.
pub fn respond(parts: &Parts, maximum: u64) -> Result<Response, Rejection> {
    let query = Query::<DataQuery>::try_from_uri(&parts.uri);
    let Query(query) = query.map_err(|_| (StatusCode::BAD_REQUEST, "invalid query\n"))?;
    let size = requested_size(&query, maximum)?;
    let etag = format!("\"kneetap-{size}\"");
    let (status, selected) = select(parts, &etag, size);

    let mut response = Response::new(body(status, &selected));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    insert_representation_headers(headers, &etag);
    insert_length_headers(headers, status, &selected, size);
    if query.attachment {
        let disposition = HeaderValue::from_static("attachment; filename=\"data.bin\"");
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    Ok(response)
}

/// Reads the size in bytes from the query, which is 1 byte if the client doesn't say.
fn requested_size(query: &DataQuery, maximum: u64) -> Result<u64, Rejection> {
    let size = query.size.as_deref().unwrap_or("1");
    let unit = query.unit.as_deref().unwrap_or("b");
    let size = parse_size(size, unit, maximum);
    size.map_err(|message| (StatusCode::BAD_REQUEST, message))
}

/// Streams the selected bytes for a status that carries them, and is empty otherwise.
fn body(status: StatusCode, selected: &Range<u64>) -> Body {
    if has_content(status) {
        stream_body(selected.clone())
    } else {
        Body::empty()
    }
}

/// Returns whether a status carries the selected bytes, rather than only explaining itself.
fn has_content(status: StatusCode) -> bool {
    matches!(status, StatusCode::OK | StatusCode::PARTIAL_CONTENT)
}

/// Inserts the headers that describe the data whatever the outcome of the request.
fn insert_representation_headers(headers: &mut HeaderMap, etag: &str) {
    let content_type = HeaderValue::from_static("application/octet-stream");
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::ETAG, etag.parse().expect("a digit-only etag"));
}

/// Inserts `Content-Length` and `Content-Range` for a status from [`select`].
///
/// A 304 states the length a 200 would have, rather than its own empty body.
fn insert_length_headers(
    headers: &mut HeaderMap,
    status: StatusCode,
    selected: &Range<u64>,
    size: u64,
) {
    if has_content(status) || status == StatusCode::NOT_MODIFIED {
        let length = selected.end - selected.start;
        headers.insert(header::CONTENT_LENGTH, length.into());
    }
    let content_range = match status {
        StatusCode::PARTIAL_CONTENT => content_range(selected, size),
        StatusCode::RANGE_NOT_SATISFIABLE => format!("bytes */{size}"),
        _ => return,
    };
    let content_range = content_range.parse().expect("a digit-only range");
    headers.insert(header::CONTENT_RANGE, content_range);
}

/// Formats a `Content-Range` value for a non-empty selection.
fn content_range(selected: &Range<u64>, size: u64) -> String {
    let Range { start, end } = selected;
    format!("bytes {start}-{}/{size}", end - 1)
}

/// Applies conditional and range headers, in the order HTTP defines.
fn select(parts: &Parts, etag: &str, size: u64) -> (StatusCode, Range<u64>) {
    let everything = 0..size;
    if let Some(status) = failed_precondition(parts, etag) {
        return (status, everything);
    }
    if !range_applies(parts, etag) {
        return (StatusCode::OK, everything);
    }
    let range = header_value(parts, &header::RANGE);
    match range.map_or(ByteRange::Ignored, |value| parse_range(value, size)) {
        ByteRange::Ignored => (StatusCode::OK, everything),
        ByteRange::Selected(range) => (StatusCode::PARTIAL_CONTENT, range),
        ByteRange::Unsatisfiable => (StatusCode::RANGE_NOT_SATISFIABLE, everything),
    }
}

/// Returns the status that ends the request early if a validator says it should.
fn failed_precondition(parts: &Parts, etag: &str) -> Option<StatusCode> {
    let if_match = header_list(parts, &header::IF_MATCH);
    if if_match.is_some_and(|list| !etag_listed(&list, etag)) {
        return Some(StatusCode::PRECONDITION_FAILED);
    }
    let if_none_match = header_list(parts, &header::IF_NONE_MATCH);
    let unchanged = if_none_match.is_some_and(|list| weak_etag_listed(&list, etag));
    unchanged.then_some(StatusCode::NOT_MODIFIED)
}

/// Checks that `Range` can apply: only to GET, and `If-Range` falls back to the whole data.
fn range_applies(parts: &Parts, etag: &str) -> bool {
    let if_range = header_value(parts, &header::IF_RANGE);
    parts.method != Method::HEAD && if_range.is_none_or(|tag| tag == etag)
}

/// Reads a header that holds a single value, treating anything unreadable as absent.
fn header_value<'a>(parts: &'a Parts, name: &HeaderName) -> Option<&'a str> {
    parts.headers.get(name)?.to_str().ok()
}

/// Joins a header's field lines, because repeated lines mean one list (RFC 9110 section 5.3).
fn header_list(parts: &Parts, name: &HeaderName) -> Option<String> {
    let lines = parts.headers.get_all(name).iter();
    let list = lines.filter_map(|line| line.to_str().ok()).join(",");
    (!list.is_empty()).then_some(list)
}

/// Checks an `If-Match` list, which only strong tags can satisfy.
fn etag_listed(list: &str, etag: &str) -> bool {
    entity_tags(list).any(|item| item == "*" || item == etag)
}

/// Checks an `If-None-Match` list, which also compares weak tags.
fn weak_etag_listed(list: &str, etag: &str) -> bool {
    entity_tags(list).any(|item| {
        let item = item.strip_prefix("W/").unwrap_or(item);
        item == "*" || item == etag
    })
}

/// Splits a list of entity tags at its commas, leaving any inside a quoted tag alone.
fn entity_tags(list: &str) -> impl Iterator<Item = &str> {
    let mut quoted = false;
    let separator = move |character| {
        quoted ^= character == '"';
        character == ',' && !quoted
    };
    list.split(separator).map(str::trim)
}

/// Generates the response lazily, because a request can ask for far more than fits in memory.
fn stream_body(range: Range<u64>) -> Body {
    Body::from_stream(stream::unfold(range, |range| async { next_chunk(range) }))
}

/// Splits the next chunk off the remaining range, ending the stream when none is left.
fn next_chunk(range: Range<u64>) -> Option<(Result<Bytes, Infallible>, Range<u64>)> {
    if range.is_empty() {
        return None;
    }
    let end = range.start.saturating_add(CHUNK_SIZE).min(range.end);
    let chunk = Bytes::from(pattern(range.start..end));
    Some((Ok(chunk), end..range.end))
}

/// Returns part of the data, in which every line states its own offset in hexadecimal.
///
/// Any slice of a download can therefore be checked by eye or by script, which makes misplaced
/// bytes from a proxy or a resumed transfer easy to spot.
fn pattern(range: Range<u64>) -> Vec<u8> {
    let first_line = range.start / LINE_SIZE;
    let last_line = range.end.div_ceil(LINE_SIZE);
    let lines: String = (first_line..last_line).map(pattern_line).collect();
    let skip = usize::try_from(range.start % LINE_SIZE).expect("a line offset fits in usize");
    let length = usize::try_from(range.end - range.start).expect("a chunk fits in usize");
    lines.as_bytes()[skip..skip + length].to_vec()
}

/// Formats one line of the pattern, wrapping offsets that need more than its 15 digits.
fn pattern_line(line: u64) -> String {
    let offset = (line * LINE_SIZE) % (1 << 60);
    format!("{offset:015x}\n")
}

/// Converts a size and unit to bytes, with `KB` meaning 1000 and `KiB` meaning 1024.
fn parse_size(value: &str, unit: &str, maximum: u64) -> Result<u64, &'static str> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("size must be an unsigned decimal integer\n");
    }
    let multiplier = unit_multiplier(unit).ok_or("unknown size unit\n")?;
    value
        .parse::<u64>()
        .ok()
        .and_then(|size| size.checked_mul(multiplier))
        .filter(|&size| size <= maximum)
        .ok_or("size exceeds configured maximum\n")
}

/// Returns the bytes in a unit, ignoring case.
fn unit_multiplier(unit: &str) -> Option<u64> {
    let multiplier = match unit.to_ascii_lowercase().as_str() {
        "b" | "bytes" => 1,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        _ => return None,
    };
    Some(multiplier)
}

/// The outcome of reading a `Range` header.
#[derive(Debug, Eq, PartialEq)]
enum ByteRange {
    /// The header is invalid or uses a form this server doesn't support, such as multiple ranges.
    Ignored,

    /// The bytes to send, which are never empty.
    Selected(Range<u64>),

    /// The header is valid but selects nothing in the data.
    Unsatisfiable,
}

/// Parses a `Range` header for a body of `size` bytes.
///
/// RFC 9110 section 14.2 lets a server ignore a range it doesn't support, so anything other than a
/// single byte range serves the whole body rather than failing.
fn parse_range(value: &str, size: u64) -> ByteRange {
    let Some((start, end)) = single_range(value) else {
        return ByteRange::Ignored;
    };
    match (decimal(start), decimal(end)) {
        (None, Some(suffix)) if start.is_empty() => final_bytes(suffix, size),
        (Some(start), None) if end.is_empty() => bounded(start, u64::MAX, size),
        (Some(start), Some(end)) => bounded(start, end, size),
        _ => ByteRange::Ignored,
    }
}

/// Splits a `bytes=start-end` value into its bounds, which may be empty.
fn single_range(value: &str) -> Option<(&str, &str)> {
    let (unit, spec) = value.split_once('=')?;
    let supported = unit.eq_ignore_ascii_case("bytes") && !spec.contains(',');
    supported.then(|| spec.split_once('-')).flatten()
}

/// Selects the last `suffix` bytes, or all of the data if it's shorter than that.
fn final_bytes(suffix: u64, size: u64) -> ByteRange {
    if suffix == 0 {
        ByteRange::Unsatisfiable
    } else if size == 0 {
        // HTTP calls this satisfiable, but `Content-Range` can't describe an empty slice.
        ByteRange::Ignored
    } else {
        ByteRange::Selected(size.saturating_sub(suffix)..size)
    }
}

/// Selects an inclusive range, clamping its end to the data as HTTP requires.
///
/// An end before the start is an invalid range rather than an unsatisfiable one, and RFC 9110 says
/// to ignore those.
fn bounded(start: u64, end: u64, size: u64) -> ByteRange {
    if end < start {
        ByteRange::Ignored
    } else if start >= size {
        ByteRange::Unsatisfiable
    } else {
        ByteRange::Selected(start..end.min(size - 1) + 1)
    }
}

/// Parses digits only, since `u64::from_str` also accepts a leading `+`.
fn decimal(value: &str) -> Option<u64> {
    let digits = !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    digits.then(|| value.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use axum::body;
    use axum::http::Request;
    use proptest::prelude::*;
    use rstest::rstest;

    use super::*;

    const MAXIMUM: u64 = 1000;

    fn get(target: &str, headers: &[(&str, &str)]) -> Parts {
        let mut builder = Request::get(target);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(()).unwrap().into_parts().0
    }

    async fn bytes(response: Response) -> Vec<u8> {
        let body = response.into_body();
        body::to_bytes(body, usize::MAX).await.unwrap().to_vec()
    }

    #[test]
    fn lines_state_their_own_offset() {
        assert_eq!(pattern(0..32), b"000000000000000\n000000000000010\n");
    }

    #[rstest]
    #[case(0)]
    #[case(1)]
    #[case(40)]
    #[case(513)]
    #[tokio::test]
    async fn responses_contain_exactly_the_requested_size(#[case] size: u64) {
        let response = respond(&get(&format!("/data?size={size}"), &[]), MAXIMUM).unwrap();

        assert_eq!(response.headers()[header::CONTENT_LENGTH], size.to_string());
        assert_eq!(bytes(response).await, pattern(0..size));
    }

    #[tokio::test]
    async fn attachments_are_named_downloads_of_the_same_size() {
        let response = respond(&get("/data?size=40&attachment=true", &[]), MAXIMUM).unwrap();

        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"data.bin\""
        );
        assert_eq!(bytes(response).await, pattern(0..40));
    }

    #[rstest]
    #[case("bytes=3-7", 3..8, "bytes 3-7/100")]
    #[case("bytes=90-", 90..100, "bytes 90-99/100")]
    #[case("bytes=-4", 96..100, "bytes 96-99/100")]
    #[tokio::test]
    async fn ranges_return_that_slice_of_the_data(
        #[case] range: &str,
        #[case] slice: Range<u64>,
        #[case] content_range: &str,
    ) {
        let response = respond(&get("/data?size=100", &[("range", range)]), MAXIMUM).unwrap();

        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], content_range);
        assert_eq!(bytes(response).await, pattern(slice));
    }

    #[rstest]
    #[case(&[("range", "bytes=100-")], StatusCode::RANGE_NOT_SATISFIABLE)]
    #[case(&[("range", "bytes=0-1,3-4")], StatusCode::OK)]
    #[case(&[("range", "bytes=7-3")], StatusCode::OK)]
    #[case(&[("if-none-match", "\"kneetap-100\"")], StatusCode::NOT_MODIFIED)]
    #[case(&[("if-none-match", "W/\"kneetap-100\"")], StatusCode::NOT_MODIFIED)]
    #[case(
        &[("if-none-match", "\"a\""), ("if-none-match", "\"kneetap-100\"")],
        StatusCode::NOT_MODIFIED
    )]
    #[case(&[("if-none-match", "\"a,*,b\"")], StatusCode::OK)]
    #[case(&[("if-match", "\"other\"")], StatusCode::PRECONDITION_FAILED)]
    #[case(&[("if-match", "\"a,*,b\"")], StatusCode::PRECONDITION_FAILED)]
    #[case(&[("if-match", "\"other\""), ("if-match", "\"kneetap-100\"")], StatusCode::OK)]
    #[case(&[("if-range", "\"other\""), ("range", "bytes=1-2")], StatusCode::OK)]
    #[case(
        &[("if-range", "\"kneetap-100\""), ("range", "bytes=1-2")],
        StatusCode::PARTIAL_CONTENT
    )]
    fn request_headers_select_the_status(
        #[case] headers: &[(&str, &str)],
        #[case] status: StatusCode,
    ) {
        let response = respond(&get("/data?size=100", headers), MAXIMUM).unwrap();

        assert_eq!(response.status(), status);
    }

    #[tokio::test]
    async fn suffix_ranges_of_empty_data_return_all_of_it() {
        let response = respond(&get("/data?size=0", &[("range", "bytes=-5")]), MAXIMUM).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bytes(response).await, b"");
    }

    #[test]
    fn head_reports_the_full_length_and_ignores_ranges() {
        let parts = Request::head("/data?size=100")
            .header("range", "bytes=1-2")
            .body(())
            .unwrap()
            .into_parts()
            .0;

        let response = respond(&parts, MAXIMUM).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "100");
    }

    #[rstest]
    #[case("1", "KB", Ok(1000))]
    #[case("1", "KiB", Ok(1024))]
    #[case("2", "b", Ok(2))]
    #[case("2", "", Err("unknown size unit\n"))]
    #[case("-1", "b", Err("size must be an unsigned decimal integer\n"))]
    #[case("1", "wat", Err("unknown size unit\n"))]
    #[case("2", "KB", Err("size exceeds configured maximum\n"))]
    #[case("18446744073709551615", "TB", Err("size exceeds configured maximum\n"))]
    fn sizes_are_checked_before_use(
        #[case] size: &str,
        #[case] unit: &str,
        #[case] expected: Result<u64, &str>,
    ) {
        assert_eq!(parse_size(size, unit, 1024), expected);
    }

    proptest! {
        #[test]
        fn any_slice_matches_the_full_data(size in 1u64..4096, a in 0u64..4096, b in 0u64..4096) {
            let (start, end) = (a.min(b).min(size), a.max(b).min(size));
            let full = pattern(0..size);
            let slice = &full[usize::try_from(start).unwrap()..usize::try_from(end).unwrap()];
            prop_assert_eq!(pattern(start..end), slice);
        }
    }
}
