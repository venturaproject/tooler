use super::{Source, Table, extract, label_column, numeric_column};
use anyhow::Result;
use rust_xlsxwriter::{Chart, ChartType, Color, Format, FormatBorder, Workbook, Worksheet};
use std::collections::HashSet;

const CHART_ROW_SPAN: u32 = 17;

fn title_format() -> Format {
    Format::new().set_bold().set_font_size(16.0)
}
fn heading_format() -> Format {
    Format::new()
        .set_bold()
        .set_font_size(12.0)
        .set_font_color(Color::RGB(0x1c599e))
}
fn subheading_format() -> Format {
    Format::new().set_bold().set_font_size(11.0)
}
fn header_format() -> Format {
    Format::new()
        .set_bold()
        .set_background_color(Color::RGB(0xEAF0FA))
        .set_border(FormatBorder::Thin)
}
fn cell_format() -> Format {
    Format::new().set_border(FormatBorder::Thin)
}
fn key_format() -> Format {
    Format::new().set_bold()
}

/// Excel worksheet names: no `[]:*?/\`, non-empty, max 31 chars, unique per workbook.
fn sanitize_sheet_name(name: &str, used: &mut HashSet<String>) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if "[]:*?/\\".contains(c) { '_' } else { c })
        .collect();
    let base: String = if cleaned.trim().is_empty() {
        "Sheet".to_string()
    } else {
        cleaned.chars().take(31).collect()
    };

    let mut candidate = base.clone();
    let mut n = 2;
    while !used.insert(candidate.clone()) {
        let suffix = format!("_{n}");
        let trimmed_len = 31usize.saturating_sub(suffix.len());
        candidate = format!(
            "{}{suffix}",
            base.chars().take(trimmed_len).collect::<String>()
        );
        n += 1;
    }
    candidate
}

/// Writes a cell as a real number when the trimmed text parses as one, so Excel
/// treats it as numeric (sortable, chartable) rather than as text.
fn write_cell(ws: &mut Worksheet, row: u32, col: u16, value: &str, fmt: &Format) -> Result<()> {
    let trimmed = value.trim();
    if !trimmed.is_empty()
        && let Ok(n) = trimmed.parse::<f64>()
    {
        ws.write_with_format(row, col, n, fmt)?;
        return Ok(());
    }
    ws.write_with_format(row, col, value, fmt)?;
    Ok(())
}

fn write_table(ws: &mut Worksheet, sheet_name: &str, table: &Table, mut row: u32) -> Result<u32> {
    ws.write_with_format(row, 0, table.name.as_str(), &subheading_format())?;
    row += 1;

    let header_row = row;
    for (ci, col_name) in table.columns.iter().enumerate() {
        ws.write_with_format(row, ci as u16, col_name.as_str(), &header_format())?;
    }
    row += 1;

    let first_data_row = row;
    for data_row in &table.rows {
        for (ci, cell) in data_row.iter().enumerate() {
            write_cell(ws, row, ci as u16, cell, &cell_format())?;
        }
        row += 1;
    }
    let last_data_row = row - 1;

    if last_data_row >= first_data_row
        && let Some(num_col) = numeric_column(table)
        && let Some(label_col) = label_column(table, num_col)
    {
        let mut chart = Chart::new(ChartType::Column);
        chart
            .add_series()
            .set_categories((
                sheet_name,
                first_data_row,
                label_col as u16,
                last_data_row,
                label_col as u16,
            ))
            .set_values((
                sheet_name,
                first_data_row,
                num_col as u16,
                last_data_row,
                num_col as u16,
            ))
            .set_name((sheet_name, header_row, num_col as u16));
        chart.title().set_name(table.name.as_str());
        ws.insert_chart(row, 0, &chart)?;
        row += CHART_ROW_SPAN;
    }

    row += 1;
    Ok(row)
}

/// Render `sources` into an Excel workbook: one sheet per source, each with a
/// summary block, one formatted table per array field, and a native column
/// chart for any table with a numeric column.
pub fn build(title: &str, sources: &[Source]) -> Result<Vec<u8>> {
    let mut workbook = Workbook::new();
    let mut used_names = HashSet::new();

    for source in sources {
        let extracted = extract(&source.value);
        let sheet_name = sanitize_sheet_name(&source.name, &mut used_names);
        let ws = workbook.add_worksheet();
        ws.set_name(&sheet_name)?;

        let mut row: u32 = 0;
        ws.write_with_format(row, 0, title, &title_format())?;
        row += 1;
        ws.write_with_format(row, 0, source.name.as_str(), &heading_format())?;
        row += 2;

        if !extracted.summary.is_empty() {
            ws.write_with_format(row, 0, "Resumen", &subheading_format())?;
            row += 1;
            for (k, v) in &extracted.summary {
                ws.write_with_format(row, 0, k.as_str(), &key_format())?;
                write_cell(ws, row, 1, v, &cell_format())?;
                row += 1;
            }
            row += 1;
        }

        for table in &extracted.tables {
            row = write_table(ws, &sheet_name, table, row)?;
        }

        ws.autofit();
    }

    let bytes = workbook.save_to_buffer()?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Source;
    use std::collections::HashSet;

    fn source(name: &str, value: serde_json::Value) -> Source {
        Source {
            name: name.to_string(),
            value,
        }
    }

    #[test]
    fn build_produces_a_valid_xlsx() {
        let sources = vec![source(
            "servers",
            serde_json::json!({"servers": [
                {"name": "web1", "cpu_pct": 42.5},
                {"name": "web2", "cpu_pct": 78.2}
            ]}),
        )];
        let bytes = build("Test Report", &sources).unwrap();
        // .xlsx is a zip archive
        assert!(bytes.starts_with(b"PK"));
        assert!(bytes.len() > 500);
    }

    #[test]
    fn build_handles_multiple_sources_as_separate_sheets() {
        let sources = vec![
            source("a", serde_json::json!({"items": [{"x": 1}]})),
            source("b", serde_json::json!({"items": [{"x": 2}]})),
        ];
        let bytes = build("Multi", &sources).unwrap();
        assert!(bytes.starts_with(b"PK"));
    }

    #[test]
    fn sanitize_sheet_name_strips_forbidden_characters_and_dedupes() {
        let mut used = HashSet::new();
        let a = sanitize_sheet_name("a/b:c", &mut used);
        assert_eq!(a, "a_b_c");
        let b = sanitize_sheet_name("a/b:c", &mut used);
        assert_ne!(a, b, "second use of the same name must be deduped");
    }

    #[test]
    fn sanitize_sheet_name_rejects_empty_and_overlong_names() {
        let mut used = HashSet::new();
        let empty = sanitize_sheet_name("", &mut used);
        assert_eq!(empty, "Sheet");

        let mut used = HashSet::new();
        let long = "x".repeat(50);
        let sanitized = sanitize_sheet_name(&long, &mut used);
        assert!(sanitized.chars().count() <= 31);
    }
}
