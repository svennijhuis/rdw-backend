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
    // Framing check, BEFORE parsing. The `csv` crate treats end-of-input
    // inside a quoted field as a valid end of that field, so a body cut in
    // the middle of a quoted cell parses perfectly cleanly into a short page
    // — and `fetch_one_range`/`fetch_and_widen` read a short page as "this
    // range is exhausted". That silently drops every remaining vehicle, which
    // is precisely the truncation bug this repo already shipped once.
    //
    // Verified against opendata.rdw.nl: every CSV response ends with a
    // newline, including the fuel dataset and a header-only zero-row
    // response. A body that does not is therefore incomplete, whatever the
    // parser makes of it. Reported as Transport so it is RETRIED rather than
    // failing the export outright: truncation is usually transient.
    match bytes.last() {
        Some(b'\n') => {}
        _ => {
            return Err(ClientError::Transport(format!(
                "CSV response is not newline-terminated ({} bytes); the body was truncated",
                bytes.len()
            )))
        }
    }

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

    // A duplicated header name must be rejected, not silently resolved.
    // `collect()` into a HashMap keeps the LAST occurrence, so a response
    // whose header read `kenteken,merk,kenteken` would pass the
    // required-column check below and then read every row's `kenteken` from
    // the third column instead of the first — shadowing the join key with an
    // unrelated value. RDW does not emit duplicate fieldNames, so this is
    // defence in depth against a malformed or substituted response rather
    // than an expected case, but silently picking one of them is never the
    // right answer for the column the whole merge-join keys on.
    let mut index: HashMap<&str, usize> = HashMap::with_capacity(headers.len());
    for (i, h) in headers.iter().enumerate() {
        if index.insert(h, i).is_some() {
            return Err(ClientError::Decode(format!(
                "CSV response header contains duplicate column '{h}'"
            )));
        }
    }
    let index = index;

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
    fn failure_duplicate_header_column_is_rejected_rather_than_shadowing() {
        // The join key appears twice. Resolving it to the LAST occurrence
        // would silently key merge_join on `merk`'s value.
        let csv = "kenteken,merk,kenteken\nAA001A,TOYOTA,ZZ999Z\n";
        let err = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap_err();
        match err {
            ClientError::Decode(msg) => {
                assert!(msg.contains("duplicate"), "got: {msg}");
                assert!(msg.contains("kenteken"), "got: {msg}");
            }
            other => panic!("expected a Decode error naming the duplicate, got {other:?}"),
        }
    }

    #[test]
    fn failure_duplicate_non_key_header_is_also_rejected() {
        // Not just the join key: any duplicate means one column's values are
        // being discarded, and which one is arbitrary.
        let csv = "kenteken,merk,merk\nAA001A,TOYOTA,LEXUS\n";
        assert!(matches!(
            parse_csv_rows(csv.as_bytes(), &["kenteken"]),
            Err(ClientError::Decode(_))
        ));
    }

    #[test]
    fn edge_quoted_cell_with_embedded_comma_newline_and_quote_survives_intact() {
        // RFC4180 handling must come from the csv crate, never from splitting
        // on '\n' by hand: a quoted newline would otherwise split one record
        // into two and shift every subsequent column.
        let csv = "kenteken,handelsbenaming\nAA001A,\"COROLLA, 1.8 \"\"HYBRID\"\"\nTOURING\"\n";
        let rows = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "an embedded newline must not split the record"
        );
        assert_eq!(
            rows[0].get("handelsbenaming"),
            Some(&Value::String(
                "COROLLA, 1.8 \"HYBRID\"\nTOURING".to_string()
            ))
        );
    }

    #[test]
    fn failure_record_with_wrong_field_count_errors_instead_of_shifting_columns() {
        // flexible(false): a short record must not silently leave later
        // columns holding the wrong values.
        let csv = "kenteken,merk,variant\nAA001A,TOYOTA\n";
        assert!(
            matches!(
                parse_csv_rows(csv.as_bytes(), &["kenteken"]),
                Err(ClientError::Transport(_))
            ),
            "a wrong-field-count record must be a retryable read error"
        );
    }

    #[test]
    fn failure_empty_body_is_a_retryable_truncation_error() {
        match parse_csv_rows(b"", &["kenteken"]) {
            Err(ClientError::Transport(_)) => {}
            other => panic!("an empty body must be a retryable truncation error, got {other:?}"),
        }
    }

    #[test]
    fn failure_body_truncated_after_a_complete_record_is_still_caught() {
        // Truncation does not have to land inside a quoted field to be
        // invisible to the parser: a body cut at a record boundary but before
        // the final newline also parses cleanly.
        let csv = "kenteken,merk\nAA001A,TOYOTA\nBB002B,TOYOTA";
        match parse_csv_rows(csv.as_bytes(), &["kenteken"]) {
            Err(ClientError::Transport(msg)) => assert!(msg.contains("truncated"), "got: {msg}"),
            other => panic!("expected a retryable truncation error, got {other:?}"),
        }
    }

    #[test]
    fn failure_body_truncated_mid_quoted_field_is_a_retryable_error() {
        // The truncation case the whole CSV switch is most exposed to: a body
        // cut inside a quoted field must never look like a short-but-complete
        // page.
        let csv = "kenteken,handelsbenaming\nAA001A,\"COROLLA";
        match parse_csv_rows(csv.as_bytes(), &["kenteken"]) {
            Err(ClientError::Transport(_)) => {}
            Ok(rows) => panic!("a truncated body must not parse cleanly, got {rows:?}"),
            Err(other) => panic!("expected a retryable Transport error, got {other:?}"),
        }
    }

    #[test]
    fn edge_formula_injection_payload_reaches_the_parser_verbatim() {
        // The parser must NOT sanitize: neutralize_formula in rdw-core owns
        // that, and it must receive the raw value. This pins the contract
        // between the two so neither side starts assuming the other did it.
        let csv = "kenteken,handelsbenaming\nAA001A,\"=cmd|' /c calc'!A1\"\n";
        let rows = parse_csv_rows(csv.as_bytes(), &["kenteken"]).unwrap();
        assert_eq!(
            rows[0].get("handelsbenaming"),
            Some(&Value::String("=cmd|' /c calc'!A1".to_string())),
            "the parser passes the payload through untouched; widen() neutralizes it"
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
