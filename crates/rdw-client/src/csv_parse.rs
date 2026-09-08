//! Parses one Socrata CSV page into row maps, resolved by the response's
//! OWN header row — never positionally against any hardcoded column order,
//! since the hardcoded fallback list (`rdw_core::metadata`) can be stale.
//!
//! Every cell becomes `serde_json::Value::String` unconditionally: CSV has
//! no type system, and inferring numbers/booleans would silently corrupt
//! values like a kenteken of "007" or a volgnummer of "1" (both of which
//! must stay strings). An empty cell is skipped entirely rather than stored
//! as `String::new()`, matching the JSON path's "absent field" semantics and
//! keeping each row to only the columns actually populated.

use std::collections::HashMap;

use serde_json::{Map, Value};

use crate::ClientError;

/// Parse one CSV page (already read fully into memory) into row maps.
///
/// `required_columns` are checked against the header BEFORE any row is
/// parsed; a header missing one of them is a data-format error (e.g. a
/// Socrata error page served with HTTP 200), not a truncation, so it fails
/// immediately rather than being retried.
///
/// A malformed or truncated record (wrong field count under `flexible(false)`,
/// an unterminated quoted field, etc.) is reported as [`ClientError::Transport`],
/// which `is_retryable` treats as retryable: a body cut off mid-record must
/// never be mistaken for a short-but-complete page.
pub(crate) fn parse_csv_rows(
    bytes: &[u8],
    required_columns: &[&str],
) -> Result<Vec<Map<String, Value>>, ClientError> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        // Explicit even though this is the crate default: a record whose
        // field count does not match the header must be an error, never a
        // silent column shift.
        .flexible(false)
        .from_reader(bytes);

    let headers = reader
        .headers()
        .map_err(|e| ClientError::Transport(format!("failed to read CSV header: {e}")))?
        .clone();

    let index: HashMap<&str, usize> = headers.iter().enumerate().map(|(i, h)| (h, i)).collect();

    for &col in required_columns {
        if !index.contains_key(col) {
            return Err(ClientError::Decode(format!(
                "CSV response is missing required column '{col}'"
            )));
        }
    }

    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.map_err(|e| {
            ClientError::Transport(format!(
                "failed to read CSV record (body may be truncated): {e}"
            ))
        })?;
        let mut map = Map::new();
        for (&name, &idx) in &index {
            if let Some(cell) = record.get(idx) {
                if !cell.is_empty() {
                    // Unconditionally a String: never infer numeric/boolean,
                    // or "007" would become 7 and "1" would become a JSON
                    // number instead of the text volgnummer callers expect.
                    map.insert(name.to_string(), Value::String(cell.to_string()));
                }
            }
        }
        rows.push(map);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_cells_are_unconditionally_strings() {
        let csv = "kenteken,catalogusprijs\n007,1.50\n";
        let rows = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].get("kenteken"),
            Some(&Value::String("007".to_string())),
            "a leading-zero kenteken must not be parsed as a number"
        );
        assert_eq!(
            rows[0].get("catalogusprijs"),
            Some(&Value::String("1.50".to_string())),
            "trailing zero must be preserved, not normalized to 1.5"
        );
    }

    #[test]
    fn edge_empty_cells_are_skipped_not_stored_as_empty_string() {
        let csv = "kenteken,merk,variant\nAA001A,TOYOTA,\n";
        let rows = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap();
        assert_eq!(
            rows[0].len(),
            2,
            "the empty `variant` cell must be absent, not \"\""
        );
        assert!(!rows[0].contains_key("variant"));
    }

    #[test]
    fn happy_path_columns_resolved_by_header_name_not_position() {
        // Column order reversed relative to what a hardcoded fallback list
        // would assume; resolution must still be correct.
        let csv = "merk,kenteken\nTOYOTA,AA001A\n";
        let rows = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap();
        assert_eq!(
            rows[0].get("kenteken"),
            Some(&Value::String("AA001A".to_string()))
        );
        assert_eq!(
            rows[0].get("merk"),
            Some(&Value::String("TOYOTA".to_string()))
        );
    }

    #[test]
    fn failure_missing_required_column_is_rejected_before_any_row_parses() {
        let csv = "merk,variant\nTOYOTA,LE\n";
        let err = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap_err();
        match err {
            ClientError::Decode(msg) => assert!(msg.contains("kenteken")),
            other => panic!("expected Decode error naming the missing column, got {other:?}"),
        }
    }

    #[test]
    fn failure_fuel_page_missing_brandstof_volgnummer_is_rejected() {
        let csv = "kenteken,brandstof_omschrijving\nAA001A,Benzine\n";
        let err =
            parse_csv_rows(csv.as_bytes(), &["kenteken", "brandstof_volgnummer"]).unwrap_err();
        match err {
            ClientError::Decode(msg) => assert!(msg.contains("brandstof_volgnummer")),
            other => panic!("expected Decode error, got {other:?}"),
        }
    }

    #[test]
    fn failure_wrong_field_count_record_is_a_retryable_error_not_silently_shifted() {
        // flexible(false): a record with fewer fields than the header must
        // error, never silently shift the remaining columns.
        let csv = "kenteken,merk,variant\nAA001A,TOYOTA\n";
        let err = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap_err();
        match err {
            ClientError::Transport(_) => {}
            other => panic!("expected Transport (retryable) error, got {other:?}"),
        }
    }

    #[test]
    fn edge_no_data_rows_still_parses_the_header() {
        let csv = "kenteken,merk\n";
        let rows = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap();
        assert!(rows.is_empty());
    }
}
