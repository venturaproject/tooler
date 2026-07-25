use super::{Source, Table, extract, label_column, numeric_column};
use anyhow::{Context as _, Result};
use printpdf::{
    BuiltinFont, Color, FontId, Line, LinePoint, Mm, Op, PaintMode, ParsedFont, PdfDocument,
    PdfPage, PdfSaveOptions, Point, Polygon, PolygonRing, Pt, Rgb, TextItem, WindingOrder,
};

const PAGE_W: f32 = 210.0;
const PAGE_H: f32 = 297.0;
const LEFT: f32 = 18.0;
const CONTENT_W: f32 = PAGE_W - 2.0 * LEFT;
const TOP: f32 = PAGE_H - 22.0;
const BOTTOM: f32 = 20.0;
const ROW_H: f32 = 6.5;

fn text_dark() -> Rgb {
    Rgb {
        r: 0.15,
        g: 0.15,
        b: 0.18,
        icc_profile: None,
    }
}
fn text_body() -> Rgb {
    Rgb {
        r: 0.25,
        g: 0.25,
        b: 0.28,
        icc_profile: None,
    }
}
fn text_muted() -> Rgb {
    Rgb {
        r: 0.5,
        g: 0.5,
        b: 0.52,
        icc_profile: None,
    }
}
fn accent() -> Rgb {
    Rgb {
        r: 0.11,
        g: 0.35,
        b: 0.62,
        icc_profile: None,
    }
}
fn header_bg() -> Rgb {
    Rgb {
        r: 0.92,
        g: 0.94,
        b: 0.97,
        icc_profile: None,
    }
}
fn rule() -> Rgb {
    Rgb {
        r: 0.82,
        g: 0.82,
        b: 0.84,
        icc_profile: None,
    }
}

/// Loads printpdf's own bundled Helvetica subset (already embedded for PDF/A
/// conformance) as a real embedded font. This is deliberate: `Op::WriteTextBuiltinFont`
/// goes through printpdf 0.8.2's WinAnsi text encoder, which has a bug (constructs
/// `lopdf::Encoding::SimpleEncoding(b"WinAnsiEncoding")` instead of `OneByteEncoding`)
/// that makes it emit raw UTF-8 bytes instead of transcoding — any non-ASCII text (e.g.
/// Spanish accents) renders as mojibake. Registering the same font as a custom/embedded
/// font and writing through `Op::WriteText` uses printpdf's glyph-ID path instead, which
/// isn't affected.
fn load_font(doc: &mut PdfDocument, builtin: BuiltinFont) -> Result<FontId> {
    let mut warnings = Vec::new();
    let parsed = ParsedFont::from_bytes(&builtin.get_subset_font().bytes, 0, &mut warnings)
        .with_context(|| format!("loading embedded {builtin:?} font"))?;
    Ok(doc.add_font(&parsed))
}

#[derive(Clone, Copy, PartialEq)]
enum Weight {
    Regular,
    Bold,
}

#[derive(Clone, Copy)]
enum Style {
    Title,
    Heading,
    SubHeading,
    Body,
    Meta,
}

impl Style {
    fn params(self) -> (Weight, f32, f32) {
        match self {
            Style::Title => (Weight::Bold, 22.0, 12.0),
            Style::Heading => (Weight::Bold, 15.0, 9.0),
            Style::SubHeading => (Weight::Bold, 11.0, 7.0),
            Style::Body => (Weight::Regular, 10.0, 6.0),
            Style::Meta => (Weight::Regular, 8.5, 5.0),
        }
    }

    fn color(self) -> Rgb {
        match self {
            Style::Title => text_dark(),
            Style::Heading => accent(),
            Style::SubHeading => text_dark(),
            Style::Body => text_body(),
            Style::Meta => text_muted(),
        }
    }
}

struct Canvas {
    pages: Vec<PdfPage>,
    ops: Vec<Op>,
    y: f32,
    font_regular: FontId,
    font_bold: FontId,
}

impl Canvas {
    fn new(font_regular: FontId, font_bold: FontId) -> Self {
        Self {
            pages: Vec::new(),
            ops: Vec::new(),
            y: TOP,
            font_regular,
            font_bold,
        }
    }

    fn font(&self, weight: Weight) -> FontId {
        match weight {
            Weight::Regular => self.font_regular.clone(),
            Weight::Bold => self.font_bold.clone(),
        }
    }

    fn ensure_space(&mut self, needed: f32) {
        if self.y - needed < BOTTOM {
            self.new_page();
        }
    }

    fn new_page(&mut self) {
        let ops = std::mem::take(&mut self.ops);
        self.pages.push(PdfPage::new(Mm(PAGE_W), Mm(PAGE_H), ops));
        self.y = TOP;
    }

    fn finish(mut self) -> Vec<PdfPage> {
        if !self.ops.is_empty() || self.pages.is_empty() {
            let ops = std::mem::take(&mut self.ops);
            self.pages.push(PdfPage::new(Mm(PAGE_W), Mm(PAGE_H), ops));
        }
        self.pages
    }

    fn cell_text(&mut self, s: &str, x: f32, y: f32, font: FontId, size: f32, color: Rgb) {
        if s.is_empty() {
            return;
        }
        self.ops.push(Op::StartTextSection);
        self.ops.push(Op::SetTextCursor {
            pos: Point::new(Mm(x), Mm(y)),
        });
        self.ops.push(Op::SetFontSize {
            size: Pt(size),
            font: font.clone(),
        });
        self.ops.push(Op::SetLineHeight { lh: Pt(size) });
        self.ops.push(Op::SetFillColor {
            col: Color::Rgb(color),
        });
        self.ops.push(Op::WriteText {
            items: text_items(s),
            font,
        });
        self.ops.push(Op::EndTextSection);
    }

    fn text(&mut self, s: &str, style: Style) {
        let (weight, size, advance) = style.params();
        self.ensure_space(advance);
        let font = self.font(weight);
        self.cell_text(s, LEFT, self.y - advance * 0.72, font, size, style.color());
        self.y -= advance;
    }

    fn rect_fill(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
        self.ops.push(Op::SetFillColor {
            col: Color::Rgb(color),
        });
        self.ops.push(Op::DrawPolygon {
            polygon: Polygon {
                rings: vec![PolygonRing {
                    points: vec![
                        LinePoint {
                            p: Point::new(Mm(x), Mm(y)),
                            bezier: false,
                        },
                        LinePoint {
                            p: Point::new(Mm(x + w), Mm(y)),
                            bezier: false,
                        },
                        LinePoint {
                            p: Point::new(Mm(x + w), Mm(y + h)),
                            bezier: false,
                        },
                        LinePoint {
                            p: Point::new(Mm(x), Mm(y + h)),
                            bezier: false,
                        },
                    ],
                }],
                mode: PaintMode::Fill,
                winding_order: WindingOrder::NonZero,
            },
        });
    }

    fn hline(&mut self, x1: f32, x2: f32, y: f32, color: Rgb) {
        self.ops.push(Op::SetOutlineColor {
            col: Color::Rgb(color),
        });
        self.ops.push(Op::SetOutlineThickness { pt: Pt(0.6) });
        self.ops.push(Op::DrawLine {
            line: Line {
                points: vec![
                    LinePoint {
                        p: Point::new(Mm(x1), Mm(y)),
                        bezier: false,
                    },
                    LinePoint {
                        p: Point::new(Mm(x2), Mm(y)),
                        bezier: false,
                    },
                ],
                is_closed: false,
            },
        });
    }

    fn summary_block(&mut self, summary: &[(String, String)]) {
        if summary.is_empty() {
            return;
        }
        self.text("Resumen", Style::SubHeading);
        for (k, v) in summary {
            self.text(&format!("{k}: {v}"), Style::Body);
        }
        self.y -= 2.0;
    }

    fn draw_header(&mut self, columns: &[String], widths: &[f32]) {
        self.rect_fill(LEFT, self.y - ROW_H, CONTENT_W, ROW_H, header_bg());
        let font = self.font(Weight::Bold);
        let mut x = LEFT;
        for (c, w) in columns.iter().zip(widths) {
            let label = truncate_to_width(c, *w, 9.0);
            self.cell_text(
                &label,
                x + 1.5,
                self.y - ROW_H * 0.72,
                font.clone(),
                9.0,
                text_dark(),
            );
            x += w;
        }
        self.y -= ROW_H;
        self.hline(LEFT, LEFT + CONTENT_W, self.y, accent());
    }

    fn draw_row(&mut self, row: &[String], widths: &[f32]) {
        let font = self.font(Weight::Regular);
        let mut x = LEFT;
        for (cell, w) in row.iter().zip(widths) {
            let label = truncate_to_width(cell, *w, 9.0);
            self.cell_text(
                &label,
                x + 1.5,
                self.y - ROW_H * 0.72,
                font.clone(),
                9.0,
                text_body(),
            );
            x += w;
        }
        self.y -= ROW_H;
    }

    fn table(&mut self, t: &Table) {
        if t.rows.is_empty() {
            return;
        }
        let widths = column_widths(t);
        self.ensure_space(ROW_H * 2.0);
        self.draw_header(&t.columns, &widths);
        for row in &t.rows {
            if self.y - ROW_H < BOTTOM {
                self.new_page();
                self.draw_header(&t.columns, &widths);
            }
            self.draw_row(row, &widths);
        }
        self.hline(LEFT, LEFT + CONTENT_W, self.y, rule());
        self.y -= 4.0;
    }

    /// Draws a simple vector bar chart for `t` if it has a numeric column and a
    /// label column, embedded directly on the page (no external chart lib).
    fn bar_chart(&mut self, t: &Table) {
        let Some(num_col) = numeric_column(t) else {
            return;
        };
        let Some(label_col) = label_column(t, num_col) else {
            return;
        };
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
            return;
        }
        points.truncate(16);
        let max = points.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);
        if max <= 0.0 {
            return;
        }

        const CHART_H: f32 = 42.0;
        const LABEL_H: f32 = 6.0;
        self.ensure_space(CHART_H + LABEL_H + 16.0);
        self.text(
            &format!("{} por {}", t.columns[num_col], t.columns[label_col]),
            Style::SubHeading,
        );
        self.y -= 2.0;
        let chart_bottom = self.y - CHART_H;
        self.hline(LEFT, LEFT + CONTENT_W, chart_bottom, rule());

        let n = points.len() as f32;
        let gap = 2.5;
        let bar_w = ((CONTENT_W - gap * (n - 1.0)) / n).clamp(3.0, 20.0);
        let label_font = self.font(Weight::Regular);
        let mut x = LEFT;
        for (label, value) in &points {
            let h = ((*value / max) as f32 * (CHART_H - 4.0)).max(0.8);
            self.rect_fill(x, chart_bottom, bar_w, h, accent());
            let cap = truncate_to_width(label, bar_w + gap, 6.5);
            self.cell_text(
                &cap,
                x,
                chart_bottom - 4.0,
                label_font.clone(),
                6.5,
                text_muted(),
            );
            x += bar_w + gap;
        }
        self.y = chart_bottom - LABEL_H - 4.0;
    }
}

fn column_widths(t: &Table) -> Vec<f32> {
    let weights: Vec<f32> = t
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let max_len = t
                .rows
                .iter()
                .map(|r| r[i].chars().count())
                .max()
                .unwrap_or(0)
                .max(c.chars().count());
            (max_len as f32).max(3.0)
        })
        .collect();
    let total: f32 = weights.iter().sum();
    weights.iter().map(|w| (w / total) * CONTENT_W).collect()
}

/// Splits `s` on spaces and represents each one as a `TJ`-array kerning offset
/// (a cursor move, per the PDF spec) instead of a literal space glyph. The subset
/// font loaded via `load_font` has no glyph for U+0020 — the encoder falls back to
/// glyph 0, which in this particular subset happens to be '!' rather than a blank
/// .notdef, so literal spaces would otherwise render as stray "!" characters.
fn text_items(s: &str) -> Vec<TextItem> {
    const SPACE_WIDTH: f32 = -280.0;
    let mut items = Vec::new();
    for (i, word) in s.split(' ').enumerate() {
        if i > 0 {
            items.push(TextItem::Offset(SPACE_WIDTH));
        }
        if !word.is_empty() {
            items.push(TextItem::Text(word.to_string()));
        }
    }
    if items.is_empty() {
        items.push(TextItem::Text(String::new()));
    }
    items
}

fn truncate_to_width(s: &str, width_mm: f32, size_pt: f32) -> String {
    let char_w = size_pt * 0.5 * 0.3528;
    let max_chars = (((width_mm - 3.0) / char_w).floor().max(1.0)) as usize;
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        format!("{truncated}...")
    }
}

/// Render `sources` into a multi-page PDF: a cover page followed by one section
/// per source (summary fields, tables, and a bar chart for any table that has a
/// numeric column).
pub fn build(title: &str, sources: &[Source]) -> Result<Vec<u8>> {
    let mut doc = PdfDocument::new(title);
    let font_regular = load_font(&mut doc, BuiltinFont::Helvetica)?;
    let font_bold = load_font(&mut doc, BuiltinFont::HelveticaBold)?;
    let mut canvas = Canvas::new(font_regular, font_bold);

    canvas.y = PAGE_H - 110.0;
    canvas.text(title, Style::Title);
    canvas.y -= 4.0;
    let generated = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    canvas.text(&format!("Generado: {generated}"), Style::Meta);
    if !sources.is_empty() {
        let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
        canvas.text(&format!("Secciones: {}", names.join(", ")), Style::Meta);
    }
    canvas.new_page();

    for source in sources {
        let extracted = extract(&source.value);
        canvas.text(&source.name, Style::Heading);
        canvas.summary_block(&extracted.summary);
        for table in &extracted.tables {
            canvas.text(&table.name, Style::SubHeading);
            canvas.table(table);
            canvas.bar_chart(table);
        }
        canvas.y -= 6.0;
    }

    let pages = canvas.finish();
    let bytes = doc
        .with_pages(pages)
        .save(&PdfSaveOptions::default(), &mut Vec::new());
    Ok(bytes)
}

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
    fn build_produces_a_valid_pdf() {
        let sources = vec![source(
            "servers",
            serde_json::json!({"servers": [
                {"name": "web1", "cpu_pct": 42.5},
                {"name": "web2", "cpu_pct": 78.2}
            ]}),
        )];
        let bytes = build("Test Report", &sources).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
        assert!(bytes.len() > 500);
    }

    #[test]
    fn build_handles_empty_sources() {
        let bytes = build("Empty Report", &[]).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn build_handles_non_ascii_text_without_erroring() {
        // Regression check for the printpdf 0.8.2 WinAnsi-encoding bug worked around
        // in `text_items`/`load_font` — this should not panic or produce an error.
        let sources = vec![source(
            "clientes",
            serde_json::json!({"clientes": [
                {"nombre": "José Peña", "ciudad": "Logroño"},
                {"nombre": "¿Quién? ¡Ñoño!", "ciudad": "Cádiz"}
            ]}),
        )];
        let bytes = build("Prueba", &sources).unwrap();
        assert!(bytes.starts_with(b"%PDF"));
    }

    #[test]
    fn text_items_splits_words_without_literal_space_glyphs() {
        let items = text_items("hello world");
        // Should contain two Text items and one Offset in between, never a Text
        // item containing a literal space (the space glyph is the one that's broken).
        let has_space_text = items
            .iter()
            .any(|i| matches!(i, TextItem::Text(t) if t.contains(' ')));
        assert!(!has_space_text);
        assert!(items.iter().any(|i| matches!(i, TextItem::Offset(_))));
    }
}
