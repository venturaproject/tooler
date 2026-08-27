use super::{Source, Table, extract, label_column, numeric_column};
use anyhow::Result;

// Same palette pdf.rs's Style/color constants and excel.rs's heading/header formats use
// (0x1c599e accent, etc.) — a PDF/Excel/HTML report of the same data should feel like the
// same product, not three unrelated tools. Used both here (the bar chart's SVG attributes,
// which need hex literals, not CSS `var()`) and duplicated as literals in `STYLE` below —
// a `<style>` block can't reference Rust constants, so the two are kept in sync by eye.
const TEXT_MUTED: &str = "#808085";
const ACCENT: &str = "#1c599e";
const RULE: &str = "#d1d1d6";

/// Escapes the five HTML-significant characters. Used everywhere a value that didn't
/// originate as literal markup (title, source/table names, summary keys/values, cell
/// text) is written into the page — a `scrape:`d page's text can legitimately contain
/// `<`/`&`, and this must never be interpreted as markup.
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn truncate_label(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn summary_block(summary: &[(String, String)]) -> String {
    if summary.is_empty() {
        return String::new();
    }
    let mut out = String::from("<dl class=\"summary\">\n");
    for (k, v) in summary {
        out.push_str(&format!(
            "<dt>{}</dt><dd>{}</dd>\n",
            html_escape(k),
            html_escape(v)
        ));
    }
    out.push_str("</dl>\n");
    out
}

fn table_block(t: &Table) -> String {
    let mut out = format!("<h3>{}</h3>\n", html_escape(&t.name));
    if t.rows.is_empty() {
        out.push_str("<p class=\"empty\">Sin datos.</p>\n");
        return out;
    }
    out.push_str("<table>\n<thead><tr>");
    for c in &t.columns {
        out.push_str(&format!("<th>{}</th>", html_escape(c)));
    }
    out.push_str("</tr></thead>\n<tbody>\n");
    for row in &t.rows {
        out.push_str("<tr>");
        for cell in row {
            out.push_str(&format!("<td>{}</td>", html_escape(cell)));
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody>\n</table>\n");
    if let Some(chart) = bar_chart_svg(t) {
        out.push_str(&chart);
    }
    out
}

/// Renders an inline SVG bar chart for `t` if it has a numeric column and a label
/// column, mirroring `pdf.rs::bar_chart`'s selection/geometry logic (same
/// `numeric_column`/`label_column` helpers, same "need at least 2 points, cap at 16"
/// rule) but in SVG viewBox units instead of PDF point-space. Colors are hardcoded hex
/// (not CSS `var()`) so the chart renders correctly wherever the HTML is opened, not
/// only in renderers with full inline-SVG custom-property support.
fn bar_chart_svg(t: &Table) -> Option<String> {
    let num_col = numeric_column(t)?;
    let label_col = label_column(t, num_col)?;
    let mut points: Vec<(String, f64)> = t
        .rows
        .iter()
        .filter_map(|r| {
            r[num_col]
                .trim()
                .parse::<f64>()
                .ok()
                .map(|v| (r[label_col].clone(), v))
        })
        .collect();
    if points.len() < 2 {
        return None;
    }
    points.truncate(16);
    let max = points.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);
    if max <= 0.0 {
        return None;
    }

    const W: f64 = 640.0;
    const CHART_H: f64 = 120.0;
    const LABEL_H: f64 = 28.0;
    let total_h = CHART_H + LABEL_H;
    let n = points.len() as f64;
    let gap = 6.0;
    let bar_w = ((W - gap * (n - 1.0)) / n).clamp(6.0, 64.0);

    let mut svg = format!(
        "<div class=\"chart-title\">{} por {}</div>\n\
         <svg viewBox=\"0 0 {W} {total_h}\" width=\"100%\" style=\"max-width:{W}px\" \
         role=\"img\" aria-label=\"bar chart\">\n\
         <line x1=\"0\" y1=\"{CHART_H}\" x2=\"{W}\" y2=\"{CHART_H}\" stroke=\"{RULE}\" stroke-width=\"1\"/>\n",
        html_escape(&t.columns[num_col]),
        html_escape(&t.columns[label_col]),
    );
    let mut x = 0.0_f64;
    for (label, value) in &points {
        let h = ((value / max) * (CHART_H - 4.0)).max(2.0);
        let y = CHART_H - h;
        svg.push_str(&format!(
            "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{bar_w:.1}\" height=\"{h:.1}\" fill=\"{ACCENT}\"/>\n"
        ));
        svg.push_str(&format!(
            "<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"middle\" font-size=\"9\" fill=\"{TEXT_MUTED}\">{}</text>\n",
            x + bar_w / 2.0,
            CHART_H + 14.0,
            html_escape(&truncate_label(label, 10)),
        ));
        x += bar_w + gap;
    }
    svg.push_str("</svg>\n");
    Some(svg)
}

/// Render `sources` into one self-contained HTML document: a title/meta header
/// followed by one section per source (a summary key/value list, then one table per
/// array-of-objects field, each with an inline SVG bar chart if it has a numeric
/// column). No external assets — inline `<style>` only, same self-containment every
/// other tooler-generated artifact keeps.
pub fn build(title: &str, sources: &[Source]) -> Result<Vec<u8>> {
    let generated = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();

    let mut body = String::new();
    body.push_str(&format!("<h1>{}</h1>\n", html_escape(title)));
    body.push_str(&format!(
        "<p class=\"meta\">Generado: {}</p>\n",
        html_escape(&generated)
    ));
    if !sources.is_empty() {
        let names: Vec<String> = sources.iter().map(|s| html_escape(&s.name)).collect();
        body.push_str(&format!(
            "<p class=\"meta\">Secciones: {}</p>\n",
            names.join(", ")
        ));
    } else {
        body.push_str("<p class=\"empty\">Sin datos.</p>\n");
    }

    for source in sources {
        let extracted = extract(&source.value);
        body.push_str(&format!("<h2>{}</h2>\n", html_escape(&source.name)));
        body.push_str(&summary_block(&extracted.summary));
        for table in &extracted.tables {
            body.push_str(&table_block(table));
        }
    }

    let html = format!(
        "<!doctype html>\n<html lang=\"es\">\n<head>\n<meta charset=\"utf-8\">\n\
         <title>{title}</title>\n<style>\n{style}\n</style>\n</head>\n<body>\n{body}</body>\n</html>\n",
        title = html_escape(title),
        style = STYLE,
        body = body,
    );
    Ok(html.into_bytes())
}

const STYLE: &str = r#"
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif;
       color: #404047; background: #fff; margin: 0; padding: 32px 40px; max-width: 960px; }
h1 { color: #26262e; font-size: 22px; margin: 0 0 4px; }
.meta { color: #808085; font-size: 12px; margin: 0 0 2px; }
h2 { color: #1c599e; font-size: 16px; margin: 28px 0 8px; border-bottom: 1px solid #d1d1d6; padding-bottom: 4px; }
h3 { color: #26262e; font-size: 13px; margin: 16px 0 6px; }
dl.summary { display: grid; grid-template-columns: max-content 1fr; gap: 2px 12px; margin: 0 0 12px; font-size: 13px; }
dl.summary dt { color: #26262e; font-weight: 600; }
dl.summary dd { margin: 0; color: #404047; }
table { border-collapse: collapse; width: 100%; margin-bottom: 8px; font-size: 12.5px; }
th { background: #ebf0f7; color: #26262e; text-align: left; padding: 6px 8px; border-bottom: 2px solid #1c599e; font-weight: 600; }
td { padding: 5px 8px; border-bottom: 1px solid #d1d1d6; color: #404047; }
.chart-title { font-size: 11.5px; font-weight: 600; color: #26262e; margin: 14px 0 4px; }
.empty { color: #808085; font-style: italic; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Source;

    fn source(name: &str, value: serde_json::Value) -> Source {
        Source {
            name: name.to_string(),
            value,
        }
    }

    #[test]
    fn build_produces_a_valid_html_document() {
        let sources = vec![source(
            "servers",
            serde_json::json!({"servers": [
                {"name": "web1", "cpu_pct": 42.5},
                {"name": "web2", "cpu_pct": 78.2}
            ]}),
        )];
        let bytes = build("Test Report", &sources).unwrap();
        let html = String::from_utf8(bytes).unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<html"));
        assert!(html.contains("Test Report"));
        assert!(html.contains("<th>name</th>") || html.contains("<th>cpu_pct</th>"));
        assert!(html.contains("<svg")); // numeric column -> bar chart
    }

    #[test]
    fn build_handles_empty_sources() {
        let bytes = build("Empty Report", &[]).unwrap();
        let html = String::from_utf8(bytes).unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("Sin datos."));
    }

    #[test]
    fn build_handles_non_ascii_text_without_erroring() {
        let sources = vec![source(
            "clientes",
            serde_json::json!({"clientes": [
                {"nombre": "José Peña", "ciudad": "Logroño"},
                {"nombre": "¿Quién? ¡Ñoño!", "ciudad": "Cádiz"}
            ]}),
        )];
        let bytes = build("Prueba", &sources).unwrap();
        let html = String::from_utf8(bytes).unwrap();
        assert!(html.contains("José Peña"));
        assert!(html.contains("¿Quién? ¡Ñoño!"));
    }

    #[test]
    fn build_escapes_html_special_characters_in_data() {
        let sources = vec![source(
            "scraped",
            serde_json::json!({"scraped": [
                {"title": "<script>alert(1)</script>", "note": "A & B \"quoted\""}
            ]}),
        )];
        let bytes = build("Scrape Report", &sources).unwrap();
        let html = String::from_utf8(bytes).unwrap();
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("A &amp; B &quot;quoted&quot;"));
    }

    #[test]
    fn html_escape_covers_all_five_special_characters() {
        assert_eq!(html_escape("<>&\"'"), "&lt;&gt;&amp;&quot;&#39;");
    }
}
