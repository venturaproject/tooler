//! `{{token}}` resolution and the `| filter | filter` pipeline: `render`/
//! `render_for_display`, `parse_pipeline`/`FilterOp`/`apply_filter_op` (json/quote/
//! default/pluck/where/sort/date/hash/numeric/... — see `FilterOp`'s own doc comment
//! for the full set), and the small pure helpers they're built from.
use super::*;
use std::collections::HashMap;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Resolves a single `{{token}}`: the playbook's own vars first, then `env.<name>` (the
/// process environment) and `secret.<profile>.<key>` (the OS keychain, same store as
/// `tooler config set profile.<name>.token`/OAuth2 profiles — see `secrets::get_secret`).
/// `None` means "leave the token literal" — covers both a genuinely unknown name and a
/// secret lookup that failed (unset key, or the keychain itself being unreachable), so a
/// playbook never crashes over a missing secret, it just doesn't get substituted.
pub(crate) fn resolve_token(token: &str, vars: &HashMap<String, String>) -> Option<String> {
    if let Some(v) = vars.get(token) {
        return Some(v.clone());
    }
    // `{{now}}` — the current instant as RFC3339 (UTC). Chain `| date:`/`| shift:`/`| unix`
    // to reformat. A playbook var named `now` still wins (checked above).
    if token == "now" {
        return Some(chrono::Utc::now().to_rfc3339());
    }
    if let Some(name) = token.strip_prefix("env.") {
        return std::env::var(name).ok();
    }
    if let Some(rest) = token.strip_prefix("secret.") {
        let (profile, key) = rest.split_once('.')?;
        return crate::secrets::get_secret(profile, key).ok().flatten();
    }
    None
}

/// Single-pass `{{token}}` substitution shared by `render()` and `render_for_display()` —
/// the scan is identical, only how a resolved *token* (post `parse_pipeline`) is turned
/// into a replacement string differs (real value vs. masked). The `| a | b | c` filter
/// pipeline (see `parse_pipeline`/`apply_filter_op`) is applied left to right, uniformly
/// regardless of `resolve`, so `render_for_display` masks-then-filters too: a masked
/// secret piped through `| json:...` never parses as JSON and just stays unresolved,
/// while one piped through `| quote` becomes `'***'` — either way the real value never
/// leaks. `| default:X` is the one stage that produces a value from an unresolved token.
/// Anything still unresolved at the end is left exactly as written.
pub(crate) fn render_with(s: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str("{{");
            rest = after;
            continue;
        };
        let inner = after[..end].trim();
        let (token, ops) = parse_pipeline(inner);
        // A quoted token is a string literal — lets `{{ '[1,2,3]' | sum }}` /
        // `{{ 'x' | sha256 }}` feed a filter pipeline without a throwaway var.
        let mut cur = string_literal(token).or_else(|| resolve(token));
        for op in &ops {
            cur = match (cur, op) {
                // `default:` is the only op that produces a value from nothing.
                (None, FilterOp::Default(d)) => Some((*d).to_string()),
                (None, _) => None,
                (Some(v), op) => apply_filter_op(op, v, &resolve),
            };
        }
        out.push_str(&cur.unwrap_or_else(|| format!("{{{{{inner}}}}}")));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// One stage of a `{{token | a | b | c}}` render pipeline — see `parse_pipeline`.
/// Applied left to right in `render_with`. Every op except `Default` is a
/// `String -> Option<String>` transform (a shape mismatch — bad JSON, wrong type —
/// yields `None`, which leaves the whole `{{...}}` token literal, same fail-soft rule a
/// bad `json:` path already has); `Default` is the one that runs when the token itself
/// didn't resolve.
#[derive(Debug, PartialEq)]
pub(crate) enum FilterOp<'a> {
    /// `| json:<path>` — see `apply_json_filter`.
    Json(&'a str),
    /// `| quote` — POSIX single-quote for a shell command line (see `shell_quote`).
    Quote,
    /// `| default:<value>` — substitute `<value>` when the token didn't resolve.
    Default(&'a str),
    /// `| pluck:<field>` — a JSON array of objects → a JSON array of that field's values.
    Pluck(&'a str),
    /// `| where:<field>==<value>` / `| where:<field>!=<value>` — filter a JSON array of
    /// objects, comparing each object's `<field>` (stringified) to `<value>`.
    Where(&'a str, bool, &'a str),
    /// `| join:<sep>` — a JSON array → its elements joined by `<sep>`.
    Join(&'a str),
    /// `| first` / `| last` — a JSON array → its first / last element (rendered like a
    /// `json:` leaf).
    First,
    Last,
    /// `| upper` / `| lower` / `| trim` — plain string transforms.
    Upper,
    Lower,
    Trim,

    // ── date/time (parse the value as RFC3339 / unix seconds / `%Y-%m-%d[ %H:%M:%S]`) ──
    /// `| date:<strftime>` — reformat with a `chrono` format string.
    Date(&'a str),
    /// `| shift:<±N[smhdw]>` — add/subtract a duration, keep RFC3339 output.
    Shift(&'a str),
    /// `| unix` — epoch seconds.
    Unix,

    // ── encoding / hash ──
    Base64,
    Base64d,
    UrlEncode,
    Sha256,
    Sha1,
    Md5,
    /// `| hmac_sha256:<keyref>` — hex HMAC-SHA256; `<keyref>` resolves as a var name.
    HmacSha256(&'a str),

    // ── numeric (parse value, and the arg, as f64) ──
    Add(&'a str),
    Sub(&'a str),
    Mul(&'a str),
    Div(&'a str),
    /// `| round` → nearest integer; `| round:<n>` → n decimal places.
    Round(Option<&'a str>),

    // ── array aggregate / reshape (value parsed as a JSON array) ──
    Sum,
    Min,
    Max,
    Avg,
    Count,
    Sort,
    /// `| sort_by:<field>` — array of objects, ascending by that field.
    SortBy(&'a str),
    Unique,
    Reverse,
    /// `| slice:<a>:<b>` — Python-ish half-open slice, negative indices allowed; either
    /// bound may be empty (`slice:5:`, `slice::3`).
    Slice(&'a str),
}

/// A `'…'` / `"…"` quoted token → its unquoted content, else `None`. Used so a filter
/// pipeline can start from a literal (`{{ '[1,2]' | sum }}`) rather than a var.
pub(crate) fn string_literal(token: &str) -> Option<String> {
    let bytes = token.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'\'' || bytes[0] == b'"')
        && bytes[bytes.len() - 1] == bytes[0]
    {
        Some(token[1..token.len() - 1].to_string())
    } else {
        None
    }
}

/// Splits a `{{...}}` token's trimmed inner text into `(base token, pipeline)` on `|` —
/// e.g. `"rows | where:active==true | pluck:name | join:,"` →
/// `("rows", [Where("active", true, "true"), Pluck("name"), Join(",")])`. An unrecognized
/// filter word leaves the rest of the `|…` as part of the token name (so a stray `|`
/// never silently vanishes — it just fails to resolve like any unknown token).
pub(crate) fn parse_pipeline(inner: &str) -> (&str, Vec<FilterOp<'_>>) {
    let mut parts = inner.split('|');
    let token = parts.next().unwrap_or("").trim();
    let mut ops = Vec::new();
    for raw in parts {
        let f = raw.trim();
        let op = if let Some(p) = f.strip_prefix("json:") {
            FilterOp::Json(p.trim())
        } else if let Some(v) = f.strip_prefix("default:") {
            FilterOp::Default(v.trim())
        } else if let Some(v) = f.strip_prefix("pluck:") {
            FilterOp::Pluck(v.trim())
        } else if let Some(v) = f.strip_prefix("where:") {
            let v = v.trim();
            if let Some((field, val)) = v.split_once("==") {
                FilterOp::Where(field.trim(), true, val.trim())
            } else if let Some((field, val)) = v.split_once("!=") {
                FilterOp::Where(field.trim(), false, val.trim())
            } else {
                // Malformed where: — abandon the pipeline, token resolves as-is.
                return (token, ops);
            }
        } else if let Some(v) = f.strip_prefix("join:") {
            FilterOp::Join(v)
        } else if let Some(v) = f.strip_prefix("date:") {
            FilterOp::Date(v.trim())
        } else if let Some(v) = f.strip_prefix("shift:") {
            FilterOp::Shift(v.trim())
        } else if let Some(v) = f.strip_prefix("hmac_sha256:") {
            FilterOp::HmacSha256(v.trim())
        } else if let Some(v) = f.strip_prefix("add:") {
            FilterOp::Add(v.trim())
        } else if let Some(v) = f.strip_prefix("sub:") {
            FilterOp::Sub(v.trim())
        } else if let Some(v) = f.strip_prefix("mul:") {
            FilterOp::Mul(v.trim())
        } else if let Some(v) = f.strip_prefix("div:") {
            FilterOp::Div(v.trim())
        } else if let Some(v) = f.strip_prefix("round:") {
            FilterOp::Round(Some(v.trim()))
        } else if let Some(v) = f.strip_prefix("sort_by:") {
            FilterOp::SortBy(v.trim())
        } else if let Some(v) = f.strip_prefix("slice:") {
            FilterOp::Slice(v.trim())
        } else {
            match f {
                "quote" => FilterOp::Quote,
                "first" => FilterOp::First,
                "last" => FilterOp::Last,
                "upper" => FilterOp::Upper,
                "lower" => FilterOp::Lower,
                "trim" => FilterOp::Trim,
                "unix" => FilterOp::Unix,
                "base64" => FilterOp::Base64,
                "base64d" => FilterOp::Base64d,
                "urlencode" => FilterOp::UrlEncode,
                "sha256" => FilterOp::Sha256,
                "sha1" => FilterOp::Sha1,
                "md5" => FilterOp::Md5,
                "round" => FilterOp::Round(None),
                "sum" => FilterOp::Sum,
                "min" => FilterOp::Min,
                "max" => FilterOp::Max,
                "avg" => FilterOp::Avg,
                "count" => FilterOp::Count,
                "sort" => FilterOp::Sort,
                "unique" => FilterOp::Unique,
                "reverse" => FilterOp::Reverse,
                // Unknown word — stop here; the leftover text isn't a valid token name
                // either, so the whole `{{...}}` will render literal, same as today.
                _ => return (token, ops),
            }
        };
        ops.push(op);
    }
    (token, ops)
}

/// Formats an `f64` the way this DSL stringifies numbers everywhere else — no trailing
/// `.0` on a whole number.
pub(crate) fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Parses a string as a datetime for the `date:`/`shift:`/`unix` filters: RFC3339, then a
/// bare unix-seconds integer, then `%Y-%m-%d %H:%M:%S`, then `%Y-%m-%d`.
pub(crate) fn parse_datetime(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
    let s = s.trim();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(secs) = s.parse::<i64>() {
        return Utc.timestamp_opt(secs, 0).single();
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Some(Utc.from_utc_datetime(&ndt));
    }
    if let Ok(nd) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&nd.and_hms_opt(0, 0, 0)?));
    }
    None
}

/// Parses a `shift:` argument like `-7d` / `+2h` / `30m` into a `chrono::Duration`.
pub(crate) fn parse_shift(arg: &str) -> Option<chrono::Duration> {
    let arg = arg.trim();
    let (sign, rest) = match arg.strip_prefix('-') {
        Some(r) => (-1i64, r),
        None => (1i64, arg.strip_prefix('+').unwrap_or(arg)),
    };
    let (num, unit) = rest.split_at(rest.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 604800,
        _ => return None,
    };
    Some(chrono::Duration::seconds(sign * n * secs))
}

/// Hex-encodes bytes (lowercase) — for the hash filters.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Applies one pipeline stage to an already-resolved string value. `resolve` is threaded
/// in for the one op (`hmac_sha256:<keyref>`) whose argument is a var reference.
pub(crate) fn apply_filter_op(
    op: &FilterOp,
    v: String,
    resolve: &impl Fn(&str) -> Option<String>,
) -> Option<String> {
    // Value parsed as a JSON array of numbers, for the numeric aggregates.
    let as_num_array = |v: &str| -> Option<Vec<f64>> {
        let arr = serde_json::from_str::<serde_json::Value>(v).ok()?;
        arr.as_array()?
            .iter()
            .map(|el| match el {
                serde_json::Value::Number(n) => n.as_f64(),
                serde_json::Value::String(s) => s.parse().ok(),
                _ => None,
            })
            .collect()
    };
    let as_json_array = |v: &str| -> Option<Vec<serde_json::Value>> {
        serde_json::from_str::<serde_json::Value>(v)
            .ok()?
            .as_array()
            .cloned()
    };

    match op {
        FilterOp::Json(path) => apply_json_filter(&v, path),
        FilterOp::Quote => Some(shell_quote(&v)),
        // Default only matters on the None path (handled in render_with); on a value it's
        // a no-op.
        FilterOp::Default(_) => Some(v),
        FilterOp::Pluck(field) => {
            let arr = serde_json::from_str::<serde_json::Value>(&v).ok()?;
            let out: Vec<serde_json::Value> = arr
                .as_array()?
                .iter()
                .filter_map(|el| el.get(field).cloned())
                .collect();
            Some(serde_json::Value::Array(out).to_string())
        }
        FilterOp::Where(field, want_eq, value) => {
            let arr = serde_json::from_str::<serde_json::Value>(&v).ok()?;
            let out: Vec<serde_json::Value> = arr
                .as_array()?
                .iter()
                .filter(|el| {
                    let cell = el.get(field).map(json_cell_to_string).unwrap_or_default();
                    (&cell == value) == *want_eq
                })
                .cloned()
                .collect();
            Some(serde_json::Value::Array(out).to_string())
        }
        FilterOp::Join(sep) => {
            let arr = serde_json::from_str::<serde_json::Value>(&v).ok()?;
            Some(
                arr.as_array()?
                    .iter()
                    .map(json_cell_to_string)
                    .collect::<Vec<_>>()
                    .join(sep),
            )
        }
        FilterOp::First | FilterOp::Last => {
            let arr = serde_json::from_str::<serde_json::Value>(&v).ok()?;
            let a = arr.as_array()?;
            let el = if matches!(op, FilterOp::First) {
                a.first()
            } else {
                a.last()
            }?;
            Some(json_cell_to_string(el))
        }
        FilterOp::Upper => Some(v.to_uppercase()),
        FilterOp::Lower => Some(v.to_lowercase()),
        FilterOp::Trim => Some(v.trim().to_string()),

        FilterOp::Date(fmt) => Some(parse_datetime(&v)?.format(fmt).to_string()),
        FilterOp::Shift(arg) => {
            let shifted = parse_datetime(&v)? + parse_shift(arg)?;
            Some(shifted.to_rfc3339())
        }
        FilterOp::Unix => Some(parse_datetime(&v)?.timestamp().to_string()),

        FilterOp::Base64 => {
            use base64::Engine;
            Some(base64::engine::general_purpose::STANDARD.encode(v.as_bytes()))
        }
        FilterOp::Base64d => {
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(v.trim())
                .ok()?;
            String::from_utf8(bytes).ok()
        }
        FilterOp::UrlEncode => Some(
            percent_encoding::utf8_percent_encode(&v, percent_encoding::NON_ALPHANUMERIC)
                .to_string(),
        ),
        FilterOp::Sha256 => {
            use sha2::Digest;
            Some(hex(&sha2::Sha256::digest(v.as_bytes())))
        }
        FilterOp::Sha1 => {
            use sha1::Digest;
            Some(hex(&sha1::Sha1::digest(v.as_bytes())))
        }
        FilterOp::Md5 => {
            use md5::Digest;
            Some(hex(&md5::Md5::digest(v.as_bytes())))
        }
        FilterOp::HmacSha256(keyref) => {
            use hmac::Mac;
            let key = resolve(keyref)?;
            let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).ok()?;
            mac.update(v.as_bytes());
            Some(hex(&mac.finalize().into_bytes()))
        }

        FilterOp::Add(n) => Some(fmt_num(
            v.trim().parse::<f64>().ok()? + n.parse::<f64>().ok()?,
        )),
        FilterOp::Sub(n) => Some(fmt_num(
            v.trim().parse::<f64>().ok()? - n.parse::<f64>().ok()?,
        )),
        FilterOp::Mul(n) => Some(fmt_num(
            v.trim().parse::<f64>().ok()? * n.parse::<f64>().ok()?,
        )),
        FilterOp::Div(n) => {
            let d = n.parse::<f64>().ok()?;
            if d == 0.0 {
                return None;
            }
            Some(fmt_num(v.trim().parse::<f64>().ok()? / d))
        }
        FilterOp::Round(places) => {
            let x = v.trim().parse::<f64>().ok()?;
            match places {
                None => Some(fmt_num(x.round())),
                Some(p) => {
                    let f = 10f64.powi(p.parse::<i32>().ok()?);
                    Some(fmt_num((x * f).round() / f))
                }
            }
        }

        FilterOp::Sum => Some(fmt_num(as_num_array(&v)?.iter().sum())),
        FilterOp::Min => as_num_array(&v)?.into_iter().reduce(f64::min).map(fmt_num),
        FilterOp::Max => as_num_array(&v)?.into_iter().reduce(f64::max).map(fmt_num),
        FilterOp::Avg => {
            let a = as_num_array(&v)?;
            if a.is_empty() {
                return None;
            }
            Some(fmt_num(a.iter().sum::<f64>() / a.len() as f64))
        }
        FilterOp::Count => {
            let val = serde_json::from_str::<serde_json::Value>(&v).ok()?;
            let n = match &val {
                serde_json::Value::Array(a) => a.len(),
                serde_json::Value::Object(o) => o.len(),
                serde_json::Value::String(s) => s.chars().count(),
                _ => return None,
            };
            Some(n.to_string())
        }
        FilterOp::Sort => {
            let mut a = as_json_array(&v)?;
            let all_num = a.iter().all(|e| {
                matches!(e, serde_json::Value::Number(_))
                    || e.as_str().is_some_and(|s| s.parse::<f64>().is_ok())
            });
            if all_num {
                a.sort_by(|x, y| {
                    let xn = x
                        .as_f64()
                        .or_else(|| x.as_str()?.parse().ok())
                        .unwrap_or(0.0);
                    let yn = y
                        .as_f64()
                        .or_else(|| y.as_str()?.parse().ok())
                        .unwrap_or(0.0);
                    xn.partial_cmp(&yn).unwrap_or(std::cmp::Ordering::Equal)
                });
            } else {
                a.sort_by_key(json_cell_to_string);
            }
            Some(serde_json::Value::Array(a).to_string())
        }
        FilterOp::SortBy(field) => {
            let mut a = as_json_array(&v)?;
            a.sort_by(|x, y| {
                let xk = x.get(field).map(json_cell_to_string).unwrap_or_default();
                let yk = y.get(field).map(json_cell_to_string).unwrap_or_default();
                match (xk.parse::<f64>(), yk.parse::<f64>()) {
                    (Ok(xn), Ok(yn)) => xn.partial_cmp(&yn).unwrap_or(std::cmp::Ordering::Equal),
                    _ => xk.cmp(&yk),
                }
            });
            Some(serde_json::Value::Array(a).to_string())
        }
        FilterOp::Unique => {
            let a = as_json_array(&v)?;
            let mut seen = std::collections::HashSet::new();
            let out: Vec<_> = a
                .into_iter()
                .filter(|e| seen.insert(e.to_string()))
                .collect();
            Some(serde_json::Value::Array(out).to_string())
        }
        FilterOp::Reverse => {
            let mut a = as_json_array(&v)?;
            a.reverse();
            Some(serde_json::Value::Array(a).to_string())
        }
        FilterOp::Slice(arg) => {
            let a = as_json_array(&v)?;
            let len = a.len() as i64;
            let norm = |raw: &str, default: i64| -> i64 {
                if raw.is_empty() {
                    return default;
                }
                let n: i64 = raw.parse().unwrap_or(default);
                if n < 0 { (len + n).max(0) } else { n.min(len) }
            };
            let (a_raw, b_raw) = arg.split_once(':').unwrap_or((arg, ""));
            let start = norm(a_raw.trim(), 0);
            let end = norm(b_raw.trim(), len).max(start);
            let out: Vec<_> = a
                .into_iter()
                .skip(start as usize)
                .take((end - start) as usize)
                .collect();
            Some(serde_json::Value::Array(out).to_string())
        }
    }
}

/// POSIX single-quote escaping for a value about to be interpolated into a `run:`/`ssh:`/
/// `fleet:` shell command line via `| quote`: wraps `value` in single quotes, escaping any
/// embedded `'` as `'\''` (close the quote, emit an escaped literal quote, reopen it) —
/// the standard shlex-safe technique. Targets the `sh -c` `run:` already shells out to
/// (see `run_task_once`'s `task.run` branch), not a non-POSIX shell. Always succeeds
/// (unlike `apply_json_filter`, there's no "doesn't match" case), so `| quote` never
/// leaves a token unresolved the way a bad `json:` path can. See the README's "Trust
/// model" section for why this exists: a `{{var}}` sourced from untrusted external data
/// (`scrape:`, `http:` + `json:`, a `db_query:` row, a dynamic `loop: {from: ...}` item)
/// can otherwise inject shell metacharacters straight into `run:`'s command line.
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Applies a `json:<path>` filter to `value` (parsed as JSON), walking dot-separated
/// `path` segments, each optionally suffixed with one or more `[N]` array indices (e.g.
/// `data.items[0].title`, `[2]`). A final segment of exactly `length` returns the
/// current value's element/key/char count instead of doing a field lookup (arrays have
/// no literal `"length"` field to `.get()`) — e.g. `{{prs | json:length}}`,
/// `{{resp | json:data.items.length}}`. A string leaf renders raw (unquoted); any other
/// JSON value (number/bool/object/array/null) renders via its JSON text form. Returns
/// `None` on invalid JSON, a path that doesn't match, or `length` on a value that has no
/// length (number/bool/null) — `render_with` then leaves the whole `{{...}}` token
/// literal, same as any other unresolvable token.
pub(crate) fn apply_json_filter(value: &str, path: &str) -> Option<String> {
    let root: serde_json::Value = serde_json::from_str(value).ok()?;
    let mut cur = &root;
    let segments: Vec<&str> = path.split('.').filter(|s| !s.is_empty()).collect();
    for (i, segment) in segments.iter().enumerate() {
        if *segment == "length" && i == segments.len() - 1 {
            let len = match cur {
                serde_json::Value::Array(a) => a.len(),
                serde_json::Value::Object(o) => o.len(),
                serde_json::Value::String(s) => s.chars().count(),
                _ => return None,
            };
            return Some(len.to_string());
        }
        let (field, indices) = parse_path_segment(segment);
        if !field.is_empty() {
            cur = cur.get(field)?;
        }
        for idx in indices {
            cur = cur.get(idx)?;
        }
    }
    Some(match cur {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

/// Splits one `.`-separated path segment like `items[0]` or `[2]` into an optional field
/// name and zero or more array indices, so `data.items[0][1]` chains cleanly.
pub(crate) fn parse_path_segment(segment: &str) -> (&str, Vec<usize>) {
    let bracket = segment.find('[');
    let field = &segment[..bracket.unwrap_or(segment.len())];
    let mut rest = bracket.map(|b| &segment[b..]).unwrap_or("");
    let mut indices = Vec::new();
    while let Some(stripped) = rest.strip_prefix('[') {
        let Some(close) = stripped.find(']') else {
            break;
        };
        if let Ok(idx) = stripped[..close].parse::<usize>() {
            indices.push(idx);
        }
        rest = &stripped[close + 1..];
    }
    (field, indices)
}

/// Resolves and substitutes every `{{token}}` in `s` for real — see `resolve_token` for
/// resolution order. This is the value actually used to run a command / build a request;
/// for a copy meant only to be printed, use `render_for_display` instead so a secret isn't
/// echoed in cleartext.
pub(crate) fn render(s: &str, vars: &HashMap<String, String>) -> String {
    render_with(s, |t| resolve_token(t, vars))
}

/// Same substitution as `render()`, except a `{{secret.<profile>.<key>}}` token resolves to
/// the literal `***` instead of its real value. Used only for lines that get `println!`'d
/// (echoing a `run:`/`ssh:`/`fleet:`/`sync_files:` command) — never for the string actually
/// executed, which must stay `render()`'s real, unmasked output. `debug:` is a deliberate
/// exception and stays on plain `render()`: printing *is* its entire purpose, so masking it
/// would defeat the point of the action.
pub(crate) fn render_for_display(s: &str, vars: &HashMap<String, String>) -> String {
    render_with(s, |t| {
        if t.starts_with("secret.") {
            Some("***".to_string())
        } else {
            resolve_token(t, vars)
        }
    })
}
