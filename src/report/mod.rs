pub mod excel;
pub mod html;
pub mod pdf;

use anyhow::{Context, Result};
use serde_json::Value;
use std::io::Read;

/// A single named JSON input feeding the report (one PDF section / one Excel sheet).
pub struct Source {
    pub name: String,
    pub value: Value,
}

/// A flattened table extracted from a JSON array field.
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// What a JSON value breaks down into for rendering: scalar key/value pairs and tables.
pub struct Extracted {
    pub summary: Vec<(String, String)>,
    pub tables: Vec<Table>,
}

/// Parse `--in` arguments of the form `name=path` or bare `path` (name defaults to the
/// file stem). If no `--in` is given, read a single JSON document from stdin.
pub fn load_sources(inputs: &[String]) -> Result<Vec<Source>> {
    if inputs.is_empty() {
        let value = read_json_stdin()?;
        return Ok(vec![Source {
            name: "report".to_string(),
            value,
        }]);
    }

    let mut sources = Vec::with_capacity(inputs.len());
    for input in inputs {
        let (name, path) = match input.split_once('=') {
            Some((n, p)) => (n.to_string(), p.to_string()),
            None => {
                let stem = std::path::Path::new(input)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(input)
                    .to_string();
                (stem, input.clone())
            }
        };
        let value = if path == "-" {
            read_json_stdin()?
        } else {
            let content =
                std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
            serde_json::from_str(&content).with_context(|| format!("parsing JSON in {path}"))?
        };
        sources.push(Source { name, value });
    }
    Ok(sources)
}

fn read_json_stdin() -> Result<Value> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading JSON from stdin")?;
    serde_json::from_str(&buf).context("parsing JSON from stdin")
}

fn stringify_scalar(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn is_object_array(items: &[Value]) -> bool {
    !items.is_empty() && items.iter().all(Value::is_object)
}

fn object_array_table(name: &str, items: &[Value]) -> Table {
    let mut columns: Vec<String> = Vec::new();
    for item in items {
        if let Value::Object(map) = item {
            for k in map.keys() {
                if !columns.contains(k) {
                    columns.push(k.clone());
                }
            }
        }
    }
    let rows = items
        .iter()
        .map(|item| {
            columns
                .iter()
                .map(|c| item.get(c).map(stringify_scalar).unwrap_or_default())
                .collect()
        })
        .collect();
    Table {
        name: name.to_string(),
        columns,
        rows,
    }
}

fn scalar_array_table(name: &str, items: &[Value]) -> Table {
    Table {
        name: name.to_string(),
        columns: vec!["value".to_string()],
        rows: items.iter().map(|i| vec![stringify_scalar(i)]).collect(),
    }
}

/// Break a JSON value from a tooler command's `--output json` down into scalar
/// summary fields and tables (one per array-of-objects field), for generic rendering.
pub fn extract(value: &Value) -> Extracted {
    let mut summary = Vec::new();
    let mut tables = Vec::new();

    match value {
        Value::Object(map) => {
            for (k, v) in map {
                match v {
                    Value::Array(items) if is_object_array(items) => {
                        tables.push(object_array_table(k, items));
                    }
                    Value::Array(items) if !items.is_empty() => {
                        tables.push(scalar_array_table(k, items));
                    }
                    Value::Array(_) => {
                        summary.push((k.clone(), "(empty)".to_string()));
                    }
                    Value::Object(_) => {
                        summary.push((k.clone(), serde_json::to_string(v).unwrap_or_default()));
                    }
                    scalar => summary.push((k.clone(), stringify_scalar(scalar))),
                }
            }
        }
        Value::Array(items) if is_object_array(items) => {
            tables.push(object_array_table("items", items));
        }
        Value::Array(items) if !items.is_empty() => {
            tables.push(scalar_array_table("items", items));
        }
        Value::Array(_) => {}
        scalar => summary.push(("value".to_string(), stringify_scalar(scalar))),
    }

    Extracted { summary, tables }
}

fn column_is_numeric(table: &Table, ci: usize) -> bool {
    let mut has_value = false;
    for row in &table.rows {
        let cell = row[ci].trim();
        if cell.is_empty() {
            continue;
        }
        if cell.parse::<f64>().is_err() {
            return false;
        }
        has_value = true;
    }
    has_value
}

/// Index of the first column in `table` whose non-empty values all parse as numbers.
pub fn numeric_column(table: &Table) -> Option<usize> {
    (0..table.columns.len()).find(|&ci| column_is_numeric(table, ci))
}

/// Best column to use as chart category labels: the first non-numeric column
/// other than `exclude`, falling back to any other column if the table is all
/// numbers (e.g. two numeric columns).
pub fn label_column(table: &Table, exclude: usize) -> Option<usize> {
    (0..table.columns.len())
        .filter(|&ci| ci != exclude)
        .find(|&ci| !column_is_numeric(table, ci))
        .or_else(|| (0..table.columns.len()).find(|&ci| ci != exclude))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_turns_array_of_objects_into_a_table() {
        let value = json!({
            "servers": [
                {"name": "web1", "cpu_pct": 42.5},
                {"name": "web2", "cpu_pct": 78.2}
            ]
        });
        let extracted = extract(&value);
        assert!(extracted.summary.is_empty());
        assert_eq!(extracted.tables.len(), 1);
        let table = &extracted.tables[0];
        assert_eq!(table.name, "servers");
        assert_eq!(table.columns, vec!["cpu_pct", "name"]);
        assert_eq!(table.rows.len(), 2);
    }

    #[test]
    fn extract_scalar_fields_become_summary() {
        let value = json!({"healthy": true, "checks": 5, "server": "prod"});
        let extracted = extract(&value);
        assert!(extracted.tables.is_empty());
        assert_eq!(extracted.summary.len(), 3);
        assert!(
            extracted
                .summary
                .contains(&("healthy".to_string(), "true".to_string()))
        );
    }

    #[test]
    fn extract_empty_array_becomes_summary_marker() {
        let value = json!({"items": []});
        let extracted = extract(&value);
        assert!(extracted.tables.is_empty());
        assert_eq!(
            extracted.summary,
            vec![("items".to_string(), "(empty)".to_string())]
        );
    }

    #[test]
    fn extract_top_level_array_of_objects() {
        let value = json!([{"id": 1}, {"id": 2}]);
        let extracted = extract(&value);
        assert_eq!(extracted.tables.len(), 1);
        assert_eq!(extracted.tables[0].name, "items");
    }

    #[test]
    fn extract_top_level_scalar_array_uses_value_column() {
        let value = json!({"tags": ["a", "b", "c"]});
        let extracted = extract(&value);
        assert_eq!(extracted.tables[0].columns, vec!["value"]);
        assert_eq!(extracted.tables[0].rows.len(), 3);
    }

    fn table_from_rows(columns: &[&str], rows: Vec<Vec<&str>>) -> Table {
        Table {
            name: "t".to_string(),
            columns: columns.iter().map(|s| s.to_string()).collect(),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(|c| c.to_string()).collect())
                .collect(),
        }
    }

    #[test]
    fn numeric_column_finds_first_all_numeric_column() {
        let table = table_from_rows(
            &["cpu_pct", "mem_pct", "name"],
            vec![vec!["42.5", "60.1", "web1"], vec!["78.2", "55.4", "web2"]],
        );
        assert_eq!(numeric_column(&table), Some(0));
    }

    #[test]
    fn numeric_column_none_when_no_column_is_fully_numeric() {
        let table = table_from_rows(&["name"], vec![vec!["web1"], vec!["web2"]]);
        assert_eq!(numeric_column(&table), None);
    }

    #[test]
    fn label_column_prefers_non_numeric_column() {
        // Two numeric columns (cpu_pct, mem_pct) plus a text column (name) — the
        // label for cpu_pct's chart should be "name", not the other numeric column.
        let table = table_from_rows(
            &["cpu_pct", "mem_pct", "name"],
            vec![vec!["42.5", "60.1", "web1"], vec!["78.2", "55.4", "web2"]],
        );
        assert_eq!(label_column(&table, 0), Some(2));
    }

    #[test]
    fn label_column_falls_back_to_any_other_column_when_all_numeric() {
        let table = table_from_rows(&["a", "b"], vec![vec!["1", "2"], vec!["3", "4"]]);
        assert_eq!(label_column(&table, 0), Some(1));
    }
}
