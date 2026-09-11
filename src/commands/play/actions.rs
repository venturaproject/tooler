//! Standalone check/fetch leaves with no task-dispatch bookkeeping of their own:
//! `check_url`, the `http:` request/pagination/download engine, `scrape:`, `check_port`,
//! `env_check`, and `run_with_timeout` (the local-subprocess-with-a-deadline primitive
//! `run:` uses).
use super::*;
use anyhow::{Result, bail};
use colored::Colorize;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Spawns `cmd`, optionally capturing stdout (draining it on a concurrent reader thread
/// so a chatty child can't deadlock against an undrained pipe while the caller is only
/// polling `try_wait()`), and kills it if `timeout` elapses first. `timeout: None` means
/// wait indefinitely, same as the plain `.status()`/`.output()` this replaces.
pub(crate) fn run_with_timeout(
    mut cmd: std::process::Command,
    capture: bool,
    timeout: Option<u64>,
) -> Result<(bool, Option<i32>, Option<String>)> {
    crate::db::isolate_process_group(&mut cmd);
    if capture {
        cmd.stdout(std::process::Stdio::piped());
    }
    let mut child = cmd.spawn()?;
    let reader = capture.then(|| {
        let mut out = child.stdout.take().expect("stdout was piped");
        std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = String::new();
            let _ = out.read_to_string(&mut buf);
            buf
        })
    });

    let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs(secs));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if let Some(dl) = deadline
            && Instant::now() >= dl
        {
            crate::db::kill_process_group(&mut child);
            let _ = child.wait();
            bail!("command timed out after {}s", timeout.unwrap());
        }
        std::thread::sleep(Duration::from_millis(100));
    };

    let captured = reader.map(|r| r.join().unwrap_or_default().trim().to_string());
    Ok((status.success(), status.code(), captured))
}

// ── Actions ───────────────────────────────────────────────────────────────────

pub(crate) fn check_url(url: &str, quiet: bool) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    match client.get(url).send() {
        Ok(r) if r.status().is_success() => {
            if !quiet {
                println!(
                    "  {} {} ({})",
                    "✓".green().bold(),
                    url,
                    r.status().as_u16().to_string().green()
                );
            }
            Ok(())
        }
        Ok(r) => bail!("HTTP {}", r.status().as_u16()),
        Err(e) => bail!("{e}"),
    }
}

/// Executes an `http:` task's request: renders headers/body against `vars`, sends, and
/// returns the response's (body text, status code) — mirrors `check_url`'s
/// client-builder pattern. Bails on a network error, or (unless `spec.ignore_status`) a
/// non-2xx status, with the response body (truncated) in the error message.
/// Builds the request (method, headers, body — all rendered) shared by `http_request`
/// and `http_download`. Neither `.send()`s it nor decides how to read the response body,
/// since that differs between the two (text vs. binary-safe bytes).
pub(crate) fn build_http_request(
    spec: &HttpSpec,
    url: &str,
    vars: &HashMap<String, String>,
) -> Result<reqwest::blocking::RequestBuilder> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(spec.timeout))
        .build()?;
    let method = reqwest::Method::from_bytes(spec.method.to_uppercase().as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid http method: {}", spec.method))?;
    let mut req = client.request(method, url);
    for (k, v) in &spec.headers {
        req = req.header(render(k, vars), render(v, vars));
    }
    if let Some(body) = &spec.body {
        req = req.body(render(body, vars));
    }
    Ok(req)
}

pub(crate) fn http_request(
    spec: &HttpSpec,
    url: &str,
    vars: &HashMap<String, String>,
) -> Result<(String, u16)> {
    let resp = build_http_request(spec, url, vars)?
        .send()
        .with_context(|| format!("http request failed: {url}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !spec.ignore_status && !status.is_success() {
        let snippet: String = body.chars().take(300).collect();
        if snippet.trim().is_empty() {
            bail!("HTTP {}", status.as_u16());
        }
        bail!("HTTP {}: {snippet}", status.as_u16());
    }
    Ok((body, status.as_u16()))
}

/// `http: {paginate: ...}`'s engine — fetches `url`, then repeatedly renders
/// `pg.next` (with the just-fetched page's body as `{{page}}` / `{{page.status}}`) to get
/// the next URL, stopping when it renders blank, unchanged, or `max_pages` is hit. Each
/// page's `pg.items` array (or its whole body, when `items` is unset) is appended.
/// Returns `(items_json, last_status, page_count)`.
pub(crate) fn http_paginate(
    spec: &HttpSpec,
    pg: &PaginateSpec,
    first_url: &str,
    vars: &HashMap<String, String>,
) -> Result<(String, u16, usize)> {
    let mut collected: Vec<serde_json::Value> = Vec::new();
    let mut url = first_url.to_string();
    let mut last_status: u16;
    let mut pages = 0usize;

    loop {
        let (body, status) = http_request(spec, &url, vars)?;
        last_status = status;
        pages += 1;

        match &pg.items {
            Some(path) => {
                if let Some(picked) = apply_json_filter(&body, path)
                    && let Ok(serde_json::Value::Array(a)) =
                        serde_json::from_str::<serde_json::Value>(&picked)
                {
                    collected.extend(a);
                }
            }
            None => {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
                    collected.push(v);
                } else {
                    collected.push(serde_json::Value::String(body.clone()));
                }
            }
        }

        if pages >= pg.max_pages {
            break;
        }
        // Render pg.next against the current page's body.
        let mut page_vars = vars.clone();
        page_vars.insert("page".to_string(), body);
        page_vars.insert("page.status".to_string(), status.to_string());
        let next = render(&pg.next, &page_vars);
        let next = next.trim();
        if next.is_empty() || next.contains("{{") || next == url {
            break;
        }
        url = next.to_string();
    }

    Ok((
        serde_json::Value::Array(collected).to_string(),
        last_status,
        pages,
    ))
}

/// `http: {download: ...}`'s engine — same request as `http_request`, but reads the
/// response as raw bytes (`resp.bytes()`, never `.text()`) and writes them straight to
/// `out_path`, so a binary response (PDF/zip/image) survives intact instead of being
/// mangled through lossy UTF-8 decoding. On a non-2xx status, still surfaces a
/// best-effort text snippet in the error (lossy-decoded, for diagnostics only) without
/// writing anything to disk.
pub(crate) fn http_download(
    spec: &HttpSpec,
    url: &str,
    vars: &HashMap<String, String>,
    out_path: &Path,
) -> Result<(u64, u16)> {
    let resp = build_http_request(spec, url, vars)?
        .send()
        .with_context(|| format!("http request failed: {url}"))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .with_context(|| format!("reading response body: {url}"))?;
    if !spec.ignore_status && !status.is_success() {
        let snippet = String::from_utf8_lossy(&bytes[..bytes.len().min(300)]).into_owned();
        if snippet.trim().is_empty() {
            bail!("HTTP {}", status.as_u16());
        }
        bail!("HTTP {}: {snippet}", status.as_u16());
    }
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory for {}", out_path.display()))?;
    }
    std::fs::write(out_path, &bytes)
        .with_context(|| format!("writing downloaded file: {}", out_path.display()))?;
    Ok((bytes.len() as u64, status.as_u16()))
}

/// Executes a `scrape:` task: GETs `scrape_spec.url`, parses the HTML, and extracts one
/// `serde_json::Map` per `each:` match (or a single implicit whole-document match if
/// `each:` is absent). Each `fields:` entry is a CSS selector, optionally suffixed with
/// `@<attr>` (see `parse_field_selector`) to grab an attribute instead of trimmed text
/// content; a selector with no match in a given scope yields an empty string rather than
/// failing the task. Same client-builder pattern as `check_url`/`http_request`, with an
/// explicit User-Agent — a well-behaved client, not an evasive one: same trust model as
/// `check_url`/`http:` already have, the user supplies the URL, `tooler` doesn't target
/// sites, rotate proxies, or bypass bot detection.
pub(crate) fn scrape(
    scrape_spec: &ScrapeSpec,
    url: &str,
    vars: &HashMap<String, String>,
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(scrape_spec.timeout))
        .user_agent(format!("tooler/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let mut req = client.get(url);
    for (k, v) in &scrape_spec.headers {
        req = req.header(render(k, vars), render(v, vars));
    }
    let resp = req
        .send()
        .with_context(|| format!("scrape request failed: {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("HTTP {} scraping {url}", status.as_u16());
    }
    let body = resp.text().context("scrape response was not valid text")?;
    let document = scraper::Html::parse_document(&body);

    let field_selectors: Vec<(String, scraper::Selector, Option<String>)> = scrape_spec
        .fields
        .iter()
        .map(|(name, field_spec)| {
            let (css, attr) = parse_field_selector(field_spec);
            let selector = scraper::Selector::parse(css).map_err(|e| {
                anyhow::anyhow!("invalid CSS selector '{css}' for field '{name}': {e:?}")
            })?;
            Ok((name.clone(), selector, attr.map(str::to_string)))
        })
        .collect::<Result<Vec<_>>>()?;

    let extract = |scope: scraper::ElementRef<'_>| -> serde_json::Map<String, serde_json::Value> {
        let mut obj = serde_json::Map::new();
        for (name, selector, attr) in &field_selectors {
            let value = scope
                .select(selector)
                .next()
                .map(|el| match attr {
                    Some(a) => el.value().attr(a).unwrap_or_default().to_string(),
                    None => el.text().collect::<String>().trim().to_string(),
                })
                .unwrap_or_default();
            obj.insert(name.clone(), serde_json::Value::String(value));
        }
        obj
    };

    match &scrape_spec.each {
        Some(each) => {
            let row_selector = scraper::Selector::parse(each)
                .map_err(|e| anyhow::anyhow!("invalid CSS selector '{each}' for each: {e:?}"))?;
            Ok(document.select(&row_selector).map(extract).collect())
        }
        None => Ok(vec![extract(document.root_element())]),
    }
}

/// Splits a `fields:` value like `"a.title@href"` into a CSS selector and an optional
/// attribute name — `"a.title"` alone means "trimmed text content". Splits on the last
/// `@`, so a plain selector with no `@` (or an empty piece on either side) is left
/// untouched with no attribute.
pub(crate) fn parse_field_selector(spec: &str) -> (&str, Option<&str>) {
    match spec.rsplit_once('@') {
        Some((css, attr)) if !css.is_empty() && !attr.is_empty() => (css, Some(attr)),
        _ => (spec, None),
    }
}

pub(crate) fn check_port(host: &str, port: u16, timeout_secs: u64, quiet: bool) -> Result<()> {
    use std::net::ToSocketAddrs;
    let addr = format!("{host}:{port}");
    let socket = addr
        .to_socket_addrs()
        .with_context(|| format!("Cannot resolve '{addr}'"))?
        .next()
        .with_context(|| format!("No address for '{addr}'"))?;

    std::net::TcpStream::connect_timeout(&socket, Duration::from_secs(timeout_secs))
        .map(|_| {
            if !quiet {
                println!("  {} {host}:{port} is open", "✓".green().bold());
            }
        })
        .map_err(|e| anyhow::anyhow!("{host}:{port} — {e}"))
}

pub(crate) fn env_check(reference: &str, target: &str, quiet: bool) -> Result<()> {
    let parse = |path: &str| -> Result<std::collections::HashSet<String>> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("Cannot read {path}"))?;
        Ok(content
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once('=').map(|(k, _)| k.trim().to_string()))
            .collect())
    };

    let ref_keys = parse(reference)?;
    let tgt_keys = parse(target)?;
    let mut missing: Vec<&String> = ref_keys.iter().filter(|k| !tgt_keys.contains(*k)).collect();

    if missing.is_empty() {
        if !quiet {
            println!("  {} all keys present in {target}", "✓".green().bold());
        }
        Ok(())
    } else {
        missing.sort();
        bail!(
            "missing in {target}: {}",
            missing
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}
