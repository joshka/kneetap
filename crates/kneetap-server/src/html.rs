use std::fmt::{self, Write};

use kneetap_api::{BodyEncoding, CertificateSource, Report, TlsReport};

mod curl;
mod proxy;
mod time;

/// The page's styles, inlined so the report needs no further requests.
const STYLE: &str = include_str!("html/report.css");

/// Where the footer points readers who want to know what served the page.
const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");

/// The copy button's script. The page works without it, so the button starts hidden.
const SCRIPT: &str = include_str!("html/copy.js");

/// Renders a report as a self-contained page for a browser.
///
/// Every value comes from the network or the host, so each one is escaped before it's written.
/// `nonce` marks the page's own script, which [`headers`] then lets run. Without one, the page
/// leaves the script out and works without it.
pub fn render(report: &Report, nonce: Option<&str>) -> String {
    let mut page = String::new();
    write_page(&mut page, report, nonce).expect("writing to a String can't fail");
    page
}

/// Returns the headers the page is served with, which the page also lists for the reader.
///
/// The policy allows only inline styles and the page's own script, so a value that escaping
/// missed still can't run.
pub fn headers(nonce: Option<&str>) -> [(&'static str, String); 5] {
    let scripts = nonce.map_or_else(|| "'none'".to_owned(), |nonce| format!("'nonce-{nonce}'"));
    let policy = format!(
        "default-src 'none'; style-src 'unsafe-inline'; script-src {scripts}; \
         base-uri 'none'; form-action 'none'"
    );
    [
        ("content-type", "text/html; charset=utf-8".to_owned()),
        ("cache-control", "no-store".to_owned()),
        ("vary", "Accept".to_owned()),
        ("content-security-policy", policy),
        ("x-content-type-options", "nosniff".to_owned()),
    ]
}

/// Writes the sections in reading order, leaving out any the report has nothing for.
fn write_page(page: &mut String, report: &Report, nonce: Option<&str>) -> fmt::Result {
    let hostname = report.hostname.as_deref().unwrap_or("unavailable");
    let title = report.name.as_deref().unwrap_or(hostname);
    write!(
        page,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <meta name=\"color-scheme\" content=\"light dark\">\n\
         <title>{} · kneetap</title>\n<style>{STYLE}</style>\n</head>\n\
         <body style=\"--replica-hue: {}\">\n<main>\n",
        escape(title),
        replica_hue(title)
    )?;
    write_heading(page, report, hostname)?;
    write_status(page, report)?;
    write_request(page, report)?;
    page.push_str("<div class=\"pair\">\n");
    write_connection(page, report)?;
    write_server(page, report)?;
    page.push_str("</div>\n");
    write_environment(page, report)?;
    write_body(page, report)?;
    write_reproduce(page, report)?;
    write_response(page, nonce)?;
    write!(
        page,
        "<footer>Terminal clients such as <code>curl</code> get this report as text. \
         <a href=\"/api\">/api</a> returns it as JSON.<br>\
         <a href=\"{REPOSITORY}\">kneetap on GitHub</a></footer>\n</main>\n"
    )?;
    if let Some(nonce) = nonce {
        writeln!(page, "<script nonce=\"{nonce}\">{SCRIPT}</script>")?;
    }
    page.push_str("</body>\n</html>\n");
    Ok(())
}

/// Picks a hue from the instance's name, so replicas look different at a glance across reloads.
///
/// FNV-1a keeps the choice stable across builds and platforms, which `std`'s hasher doesn't.
fn replica_hue(title: &str) -> u32 {
    let hash = title.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    hash % 360
}

/// Names the instance that answered, which is what a reader comparing replicas looks for first.
fn write_heading(page: &mut String, report: &Report, hostname: &str) -> fmt::Result {
    page.push_str("<header>\n<div>\n<p class=\"eyebrow\">kneetap report</p>\n");
    let version = match &report.commit {
        Some(commit) => format!("kneetap {} ({commit})", report.version),
        None => format!("kneetap {}", report.version),
    };
    match &report.name {
        Some(name) => writeln!(
            page,
            "<h1>{}</h1>\n<p class=\"subtitle\">on <code>{}</code> · {}</p>",
            escape(name),
            escape(hostname),
            escape(&version)
        )?,
        None => writeln!(
            page,
            "<h1>{}</h1>\n<p class=\"subtitle\">{}</p>",
            escape(hostname),
            escape(&version)
        )?,
    }
    page.push_str("</div>\n<ul class=\"chips\">\n");
    write_chip(page, "", &report.http_version)?;
    match &report.tls {
        Some(tls) => write_chip(page, " secure", &tls.version)?,
        None if proxy::tls_ended_upstream(report) => {
            write_chip(page, "", "TLS ended at the proxy")?;
        }
        None => write_chip(page, " plain", "No TLS")?,
    }
    if let Some(alpn) = report.tls.as_ref().and_then(|tls| tls.alpn.as_ref()) {
        write_chip(page, "", &format!("ALPN {alpn}"))?;
    }
    if report
        .headers
        .keys()
        .any(|name| proxy::is_added_by_proxy(name))
    {
        write_chip(page, "", "Via a proxy")?;
    }
    page.push_str("</ul>\n</header>\n");
    Ok(())
}

fn write_chip(page: &mut String, class: &str, text: &str) -> fmt::Result {
    writeln!(page, "<li class=\"chip{class}\">{}</li>", escape(text))
}

/// Shows problems the server is working around, ahead of the request so they aren't missed.
fn write_status(page: &mut String, report: &Report) -> fmt::Result {
    let status = &report.server_status;
    if status.reload_error.is_none() && status.degradations.is_empty() {
        return Ok(());
    }
    page.push_str("<section class=\"warning\">\n<h2>Server status</h2>\n<ul>\n");
    if let Some(error) = &status.reload_error {
        writeln!(
            page,
            "<li>Certificate reload rejected: {}</li>",
            escape(error)
        )?;
    }
    for degradation in &status.degradations {
        writeln!(page, "<li><code>{}</code></li>", escape(degradation))?;
    }
    page.push_str("</ul>\n</section>\n");
    Ok(())
}

/// Shows the request line, query, and headers, which together are what reached this backend.
fn write_request(page: &mut String, report: &Report) -> fmt::Result {
    let (path, query) = match report.target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (report.target.as_str(), None),
    };
    write!(
        page,
        "<section>\n<h2>Request</h2>\n<p class=\"request-line\"><span class=\"method\">{}</span> \
         <span class=\"path\">{}</span>",
        escape(&report.method),
        escape(path)
    )?;
    if let Some(query) = query {
        write!(page, "<span class=\"query\">?{}</span>", escape(query))?;
    }
    page.push_str("</p>\n<dl class=\"meta\">\n");
    writeln!(
        page,
        "<div><dt>Request ID</dt><dd><span id=\"request-id\">{}</span> \
         <button type=\"button\" data-copy=\"request-id\" hidden>Copy</button></dd></div>",
        escape(&report.request_id)
    )?;
    write_time(page, "Received", &report.received_at, "")?;
    page.push_str("</dl>\n");
    write_absent(page, query, report)?;
    // An absolute-form target can name a different authority than the `Host` header.
    let received_host = report.headers.get("host");
    if received_host.is_some_and(|values| !values.contains(&report.host)) {
        writeln!(
            page,
            "<p class=\"note\">The URI names <code>{}</code>, which differs from the \
             <code>host</code> header.</p>",
            escape(&report.host)
        )?;
    }
    if let Some(query) = query {
        write_query(page, query)?;
    }
    page.push_str("<h3>Headers</h3>\n<table>\n<tbody>\n");
    // HTTP/2 carries the host in `:authority`, so it's missing from the header map.
    if received_host.is_none() && !report.host.is_empty() {
        write_row(page, ":authority", &report.host)?;
    }
    let (added, sent): (Vec<_>, Vec<_>) = report
        .headers
        .iter()
        .partition(|(name, _)| proxy::is_added_by_proxy(name));
    write_header_rows(page, &sent)?;
    page.push_str("</tbody>\n</table>\n");
    if !added.is_empty() {
        page.push_str("<h3>Added on the way</h3>\n<table>\n<tbody>\n");
        write_header_rows(page, &added)?;
        page.push_str("</tbody>\n</table>\n");
    }
    page.push_str("</section>\n");
    Ok(())
}

/// Says what the request didn't carry, so a missing section reads as checked rather than lost.
fn write_absent(page: &mut String, query: Option<&str>, report: &Report) -> fmt::Result {
    let missing: Vec<_> = [
        query
            .is_none_or(str::is_empty)
            .then_some("No query string."),
        report.body.is_empty().then_some("No body."),
    ]
    .into_iter()
    .flatten()
    .collect();
    if missing.is_empty() {
        return Ok(());
    }
    writeln!(page, "<p class=\"absent\">{}</p>", missing.join(" "))
}

/// Writes a row per header value, in the order the values arrived.
fn write_header_rows(page: &mut String, headers: &[(&String, &Vec<String>)]) -> fmt::Result {
    for (name, values) in headers {
        for value in *values {
            write_row(page, name, value)?;
        }
    }
    Ok(())
}

/// Lists the query parameters decoded, because a long encoded target is hard to read.
fn write_query(page: &mut String, query: &str) -> fmt::Result {
    let parameters: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).collect();
    if parameters.is_empty() {
        return Ok(());
    }
    page.push_str("<h3>Query, decoded</h3>\n<table>\n<tbody>\n");
    for (name, value) in parameters {
        write_row(page, &name, &value)?;
    }
    page.push_str("</tbody>\n</table>\n");
    Ok(())
}

/// Writes one name and value, folding a long value.
fn write_row(page: &mut String, name: &str, value: &str) -> fmt::Result {
    write!(page, "<tr><th scope=\"row\">{}</th><td>", escape(name))?;
    write_value(page, value)?;
    page.push_str("</td></tr>\n");
    Ok(())
}

/// Values longer than this start folded, so one cookie or token can't push the rest off screen.
const FOLD_AFTER: usize = 160;

/// Writes a value, folding a long one behind a preview that expands to the full text.
fn write_value(page: &mut String, value: &str) -> fmt::Result {
    let length = value.chars().count();
    if length <= FOLD_AFTER {
        page.push_str(&escape(value));
        return Ok(());
    }
    let preview: String = value.chars().take(FOLD_AFTER).collect();
    write!(
        page,
        "<details class=\"fold\"><summary><span class=\"preview\">{}…</span> \
         <span class=\"more\">show all {length} characters</span>\
         <span class=\"less\">show less</span></summary>{}</details>",
        escape(&preview),
        escape(value)
    )
}

/// Shows who connected and how, which tells a proxy's address apart from the client's.
fn write_connection(page: &mut String, report: &Report) -> fmt::Result {
    page.push_str("<section>\n<h2>Connection</h2>\n<dl>\n");
    write_term(page, "Remote address", &report.remote_addr)?;
    if let Some(client) = proxy::forwarded_client(report) {
        write_term(page, "Forwarded client", client)?;
    }
    let connection = &report.connection;
    write_term(page, "Connection", &connection.id)?;
    let number = connection.request_number.to_string();
    write_term(page, "Request number", &number)?;
    write_time(page, "Opened", &connection.opened_at, "")?;
    match &report.tls {
        Some(tls) => write_tls(page, tls)?,
        None if proxy::tls_ended_upstream(report) => write_term(page, "TLS", "ended at the proxy")?,
        None => write_term(page, "TLS", "none")?,
    }
    page.push_str("</dl>\n</section>\n");
    Ok(())
}

/// Describes the TLS session in words, so a reader needn't know the report's enum values.
fn write_tls(page: &mut String, tls: &TlsReport) -> fmt::Result {
    write_term(page, "TLS", &format!("{} {}", tls.version, tls.cipher))?;
    let source = match tls.certificate_source {
        CertificateSource::File => "supplied by the operator",
        CertificateSource::Generated => "generated, self-signed",
    };
    write_term(page, "Server certificate", source)?;
    let verification = tls.client_verification.to_string();
    write_term(page, "Client certificate", &verification)?;
    for subject in &tls.client_subjects {
        write_term(page, "Client subject", subject)?;
    }
    Ok(())
}

/// Shows what's true of the server across requests, so it sits after the request itself.
fn write_server(page: &mut String, report: &Report) -> fmt::Result {
    page.push_str("<section>\n<h2>Server</h2>\n<dl>\n");
    write_term(page, "Version", &report.version)?;
    if let Some(commit) = &report.commit {
        write_term(page, "Commit", commit)?;
    }
    let uptime = time::uptime(&report.started_at, &report.received_at)
        .map(|uptime| format!(", up {uptime}"))
        .unwrap_or_default();
    write_time(page, "Started", &report.started_at, &uptime)?;
    if !report.addresses.is_empty() {
        write_term(page, "Addresses", &report.addresses.join("\n"))?;
    }
    page.push_str("</dl>\n</section>\n");
    Ok(())
}

/// Writes a timestamp as its time of day, keeping the full timestamp on hover.
fn write_time(page: &mut String, term: &str, timestamp: &str, suffix: &str) -> fmt::Result {
    let Some(short) = time::time_of_day(timestamp) else {
        return write_term(page, term, &format!("{timestamp}{suffix}"));
    };
    let timestamp = escape(timestamp);
    writeln!(
        page,
        "<div><dt>{}</dt><dd><time datetime=\"{timestamp}\" title=\"{timestamp}\">{}</time>{}</dd></div>",
        escape(term),
        escape(&short),
        escape(suffix)
    )
}

fn write_term(page: &mut String, term: &str, value: &str) -> fmt::Result {
    writeln!(
        page,
        "<div><dt>{}</dt><dd>{}</dd></div>",
        escape(term),
        escape(value)
    )
}

/// Lists the environment only when the client asked for it and the operator allowed it.
fn write_environment(page: &mut String, report: &Report) -> fmt::Result {
    let Some(environment) = &report.environment else {
        return Ok(());
    };
    page.push_str("<section>\n<h2>Environment</h2>\n<pre>");
    for line in environment {
        writeln!(page, "{}", escape(line))?;
    }
    page.push_str("</pre>\n</section>\n");
    Ok(())
}

/// Shows the body when there is one, saying so when it's base64 because it wasn't UTF-8.
fn write_body(page: &mut String, report: &Report) -> fmt::Result {
    if report.body.is_empty() {
        return Ok(());
    }
    let label = match report.body_encoding {
        BodyEncoding::Text => "Body",
        BodyEncoding::Base64 => "Body <span class=\"tag\">base64, not valid UTF-8</span>",
    };
    writeln!(
        page,
        "<section>\n<h2>{label}</h2>\n<pre>{}</pre>\n</section>",
        escape(&report.body)
    )
}

/// Offers a curl command that repeats the request, to try it from a terminal or another host.
fn write_reproduce(page: &mut String, report: &Report) -> fmt::Result {
    write!(
        page,
        "<section>\n<div class=\"section-heading\"><h2>Repeat with curl</h2>\
         <button type=\"button\" data-copy=\"curl\" hidden>Copy</button></div>\n\
         <pre id=\"curl\">{}</pre>\n",
        escape(&curl::command(report))
    )?;
    if report.headers.contains_key("cookie") {
        page.push_str(
            "<p class=\"note\">The command leaves out <code>cookie</code>, which on a shared host \
             such as localhost often carries other sites' cookies. It's in the headers above.</p>\n",
        );
    }
    page.push_str("</section>\n");
    Ok(())
}

/// Lists what kneetap sent back, folded, for debugging how a proxy caches or rewrites the page.
fn write_response(page: &mut String, nonce: Option<&str>) -> fmt::Result {
    page.push_str(
        "<section>\n<details class=\"response\"><summary><h2>Response</h2></summary>\n\
         <p class=\"request-line\"><span class=\"status\">200 OK</span></p>\n<table>\n<tbody>\n",
    );
    for (name, value) in headers(nonce) {
        write_row(page, name, &value)?;
    }
    page.push_str("</tbody>\n</table>\n</details>\n</section>\n");
    Ok(())
}

/// Escapes the characters that could end an element or attribute, so text stays text.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use kneetap_api::{ConnectionReport, ServerStatus};

    use super::*;

    pub fn report() -> Report {
        Report {
            name: None,
            hostname: Some("web-1".to_owned()),
            addresses: vec!["10.0.0.5".to_owned()],
            version: "1.2.3".to_owned(),
            commit: None,
            started_at: "2026-11-07T08:00:00.000Z".to_owned(),
            request_id: "Hq7mW2pL".to_owned(),
            received_at: "2026-11-07T09:00:00.250Z".to_owned(),
            remote_addr: "10.0.0.9:4000".to_owned(),
            connection: ConnectionReport {
                id: "aZ3kQ9xB".to_owned(),
                request_number: 2,
                opened_at: "2026-11-07T08:59:00.000Z".to_owned(),
            },
            environment: None,
            method: "GET".to_owned(),
            target: "/a?b=c".to_owned(),
            host: "example.test".to_owned(),
            http_version: "HTTP/1.1".to_owned(),
            headers: BTreeMap::from([("host".to_owned(), vec!["example.test".to_owned()])]),
            body: String::new(),
            body_encoding: BodyEncoding::Text,
            tls: None,
            server_status: ServerStatus::default(),
        }
    }

    #[test]
    fn request_values_cannot_add_markup() {
        let report = Report {
            target: "/<script>alert(1)</script>?q=%3Cb%3E".to_owned(),
            headers: BTreeMap::from([("x-a".to_owned(), vec!["\"><img src=x>".to_owned()])]),
            body: "</pre><b>".to_owned(),
            ..report()
        };

        let page = render(&report, Some("n"));

        let leaked: Vec<_> = ["<script>", "<img", "<b>"]
            .into_iter()
            .filter(|tag| page.contains(tag))
            .collect();
        assert_eq!(leaked, Vec::<&str>::new());
    }

    #[test]
    fn only_the_page_script_carries_the_nonce() {
        let page = render(&report(), Some("abc"));

        assert_eq!(page.matches("nonce=\"abc\"").count(), 1);
    }

    #[test]
    fn without_a_nonce_the_page_has_no_script_and_forbids_them() {
        let page = render(&report(), None);
        let policy = &headers(None)[3].1;

        assert_eq!(
            (
                page.contains("<script"),
                policy.contains("script-src 'none'")
            ),
            (false, true)
        );
    }

    #[test]
    fn missing_query_and_body_are_stated() {
        let report = Report {
            target: "/a".to_owned(),
            ..report()
        };

        let page = render(&report, None);

        assert_eq!(
            page.lines()
                .find(|line| line.starts_with("<p class=\"absent\">")),
            Some("<p class=\"absent\">No query string. No body.</p>")
        );
    }

    #[test]
    fn escape_replaces_every_markup_character() {
        assert_eq!(
            escape(r#"<a href="x">'&'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }

    #[test]
    fn the_http2_authority_is_shown_when_the_host_header_is_missing() {
        let report = Report {
            headers: BTreeMap::new(),
            ..report()
        };

        let page = render(&report, Some("n"));

        assert_eq!(
            page.lines().find(|line| line.contains(":authority")),
            Some("<tr><th scope=\"row\">:authority</th><td>example.test</td></tr>")
        );
    }

    #[test]
    fn the_name_leads_and_the_hostname_and_version_follow() {
        let report = Report {
            name: Some("api-a".to_owned()),
            commit: Some("abc1234".to_owned()),
            ..report()
        };

        let page = render(&report, Some("n"));
        let heading: Vec<_> = page
            .lines()
            .filter(|line| line.starts_with("<h1>") || line.starts_with("<p class=\"subtitle\">"))
            .collect();

        assert_eq!(
            heading,
            [
                "<h1>api-a</h1>",
                "<p class=\"subtitle\">on <code>web-1</code> · kneetap 1.2.3 (abc1234)</p>"
            ]
        );
    }

    #[test]
    fn query_parameters_are_listed_decoded() {
        let mut page = String::new();

        write_query(&mut page, "q=a%20b&tag=x&tag=y").unwrap();

        insta::assert_snapshot!(page, @r#"
        <h3>Query, decoded</h3>
        <table>
        <tbody>
        <tr><th scope="row">q</th><td>a b</td></tr>
        <tr><th scope="row">tag</th><td>x</td></tr>
        <tr><th scope="row">tag</th><td>y</td></tr>
        </tbody>
        </table>
        "#);
    }

    #[test]
    fn long_values_fold_behind_a_preview() {
        let mut page = String::new();
        let value = format!("{}<", "a".repeat(FOLD_AFTER));

        write_value(&mut page, &value).unwrap();

        let preview = "a".repeat(FOLD_AFTER);
        assert_eq!(
            page,
            format!(
                "<details class=\"fold\"><summary><span class=\"preview\">{preview}…</span> \
                 <span class=\"more\">show all 161 characters</span>\
                 <span class=\"less\">show less</span></summary>{preview}&lt;</details>"
            )
        );
    }

    #[test]
    fn proxy_headers_are_listed_apart_from_the_client_ones() {
        let mut report = report();
        report
            .headers
            .insert("via".to_owned(), vec!["1.1 edge".to_owned()]);

        let page = render(&report, Some("n"));
        let added = page.split("<h3>Added on the way</h3>").nth(1);

        assert_eq!(
            added.and_then(|table| table.lines().find(|line| line.starts_with("<tr>"))),
            Some("<tr><th scope=\"row\">via</th><td>1.1 edge</td></tr>")
        );
    }

    #[test]
    fn the_replica_hue_is_stable_and_differs_between_names() {
        assert_eq!([replica_hue("api-a"), replica_hue("api-b")], [245, 68]);
    }
}
