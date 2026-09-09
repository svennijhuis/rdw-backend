//! Widens merge-joined vehicle+fuel rows into flat CSV rows.
//!
//! One output row per vehicle. Fuel types from every RDW fuel row for that
//! plate are joined into a single `Brandstof` column (e.g. a hybrid becomes
//! `Benzine, Elektriciteit`) rather than repeating the vehicle or widening
//! every fuel field into `Brandstof N - *` slots.
//!
//! Vehicle columns are filtered to [`VEHICLE_EXPORT_FIELDS`]: API URL
//! columns, duplicate `_dt` timestamps, trailer/hitch fields and min/max
//! dimension ranges are dropped so the spreadsheet stays readable.

use std::collections::HashMap;

use crate::merge::WidenedRow;
use crate::metadata::Column;

/// Header label for the export-status column. Kept as a named constant so the
/// header and any consumer looking the column up cannot drift apart.
pub const EXPORT_STATUS_HEADER: &str = "Export status";

/// Header label for the joined fuel-type column.
pub const BRANDSTOF_HEADER: &str = "Brandstof";

/// RDW field that holds the fuel-type display name on a fuel row.
const BRANDSTOF_OMSCHRIJVING_FIELD: &str = "brandstof_omschrijving";

/// Separator between fuel types in the `Brandstof` cell. A hybrid with petrol
/// and electric therefore reads `Benzine, Elektriciteit`.
const BRANDSTOF_SEPARATOR: &str = ", ";

/// Vehicle fields kept in the CSV, in display order.
///
/// RDW's vehicle dataset has ~98 columns. Most of those are empty for a
/// passenger car, duplicated as `_dt` timestamps, or are API link columns.
/// This list is the readable subset: identity, registration, size, and type
/// approval. A field that is absent from the metadata the caller passed in
/// is skipped rather than emitting a blank header.
pub const VEHICLE_EXPORT_FIELDS: &[&str] = &[
    "kenteken",
    "merk",
    "handelsbenaming",
    "voertuigsoort",
    "inrichting",
    "eerste_kleur",
    "tweede_kleur",
    "datum_eerste_toelating",
    "datum_tenaamstelling",
    "vervaldatum_apk",
    "catalogusprijs",
    "bruto_bpm",
    "aantal_zitplaatsen",
    "aantal_deuren",
    "aantal_wielen",
    "aantal_cilinders",
    "cilinderinhoud",
    "massa_ledig_voertuig",
    "massa_rijklaar",
    "toegestane_maximum_massa_voertuig",
    "lengte",
    "breedte",
    "hoogte_voertuig",
    "wielbasis",
    "maximale_constructiesnelheid",
    "europese_voertuigcategorie",
    "type",
    "variant",
    "uitvoering",
    "typegoedkeuringsnummer",
    "zuinigheidsclassificatie",
    "tellerstandoordeel",
    "wam_verzekerd",
    "export_indicator",
    "taxi_indicator",
];

/// Column layout used to build the CSV header and to widen each row into
/// exactly that column order.
pub struct RowWidener {
    vehicle_columns: Vec<Column>,
}

impl RowWidener {
    /// Keep only [`VEHICLE_EXPORT_FIELDS`], in that order. `fuel_columns` is
    /// accepted so callers that already hold RDW fuel metadata do not need a
    /// parallel constructor, but it is not emitted: the CSV keeps a single
    /// joined `Brandstof` cell instead of one slot per fuel-dataset field.
    pub fn new(vehicle_columns: Vec<Column>, _fuel_columns: Vec<Column>) -> Self {
        let by_field: HashMap<String, Column> = vehicle_columns
            .into_iter()
            .map(|c| (c.field.clone(), c))
            .collect();
        let vehicle_columns = VEHICLE_EXPORT_FIELDS
            .iter()
            .filter_map(|field| by_field.get(*field).cloned())
            .collect();
        Self { vehicle_columns }
    }

    /// Header row: vehicle columns, then `Brandstof`, then the export status
    /// last. The status column's position is final: it must never move
    /// earlier, because `widen()` builds rows positionally.
    ///
    /// Vehicle headers use RDW's own display names rather than its
    /// `fieldName` keys, so a reader opening the CSV sees "Gemiddelde Lading
    /// Waarde" instead of `gem_lading_wrde`.
    pub fn header(&self) -> Vec<String> {
        let mut header: Vec<String> = self
            .vehicle_columns
            .iter()
            .map(|c| neutralize_formula(c.display.clone()))
            .collect();
        header.push(BRANDSTOF_HEADER.to_string());
        header.push(EXPORT_STATUS_HEADER.to_string());
        header
    }

    /// Widen one merge-joined row into a flat CSV record matching `header()`.
    pub fn widen(&self, row: &WidenedRow) -> Vec<String> {
        let mut out = Vec::with_capacity(self.header().len());
        for col in &self.vehicle_columns {
            out.push(field_as_string(row.vehicle.0.get(&col.field)));
        }
        out.push(joined_brandstof(&row.fuels));
        out.push(row.export_status.as_str().to_string());
        out
    }
}

/// Join every non-empty `brandstof_omschrijving` on this vehicle's fuel rows,
/// in the order `merge_join` already sorted them (`brandstof_volgnummer`).
fn joined_brandstof(fuels: &[rdw_client::FuelRow]) -> String {
    let mut parts = Vec::new();
    for fuel in fuels {
        let raw = match fuel.0.get(BRANDSTOF_OMSCHRIJVING_FIELD) {
            None | Some(serde_json::Value::Null) => continue,
            Some(serde_json::Value::String(s)) if s.is_empty() => continue,
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        parts.push(raw);
    }
    neutralize_formula(parts.join(BRANDSTOF_SEPARATOR))
}

fn field_as_string(value: Option<&serde_json::Value>) -> String {
    let raw = match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    };
    neutralize_formula(raw)
}

/// Stop a cell from being executed as a formula when the CSV is opened in a
/// spreadsheet.
///
/// Every value here is third-party text from RDW, and this file exists to be
/// opened in Excel, where a cell beginning `=`, `+`, `@`, or a control
/// character is evaluated rather than displayed. Prefixing an apostrophe forces
/// the cell to be read as text. The `csv` crate quotes delimiters correctly but
/// does nothing about this, because it is a spreadsheet behaviour rather than a
/// CSV one.
///
/// A leading `-` is deliberately treated differently. Blindly escaping it would
/// turn every negative number in the dataset into text and quietly corrupt the
/// export, so a `-` is only escaped when what follows is not a number.
pub(crate) fn neutralize_formula(value: String) -> String {
    let first = match value.chars().next() {
        Some(c) => c,
        None => return value,
    };

    let dangerous = match first {
        '=' | '+' | '@' => true,
        '\t' | '\r' => true,
        '-' => value.parse::<f64>().is_err(),
        _ => false,
    };

    if dangerous {
        let mut out = String::with_capacity(value.len() + 1);
        out.push('\'');
        out.push_str(&value);
        out
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::merge_join;
    use rdw_client::{FuelRow, VehicleRow};
    use serde_json::json;

    fn widener() -> RowWidener {
        RowWidener::new(
            vec![
                Column::new("kenteken", "Kenteken"),
                Column::new("merk", "Merk"),
            ],
            vec![
                Column::new("kenteken", "Kenteken"),
                Column::new("brandstof_volgnummer", "Brandstof volgnummer"),
                Column::new("brandstof_omschrijving", "Brandstof omschrijving"),
            ],
        )
    }

    fn vehicle(kenteken: &str, merk: &str) -> VehicleRow {
        VehicleRow(
            json!({ "kenteken": kenteken, "merk": merk })
                .as_object()
                .unwrap()
                .clone(),
        )
    }

    fn fuel(kenteken: &str, volgnummer: u32, omschrijving: &str) -> FuelRow {
        FuelRow(
            json!({
                "kenteken": kenteken,
                "brandstof_volgnummer": volgnummer.to_string(),
                "brandstof_omschrijving": omschrijving
            })
            .as_object()
            .unwrap()
            .clone(),
        )
    }

    #[test]
    fn header_is_vehicle_columns_then_one_brandstof_then_status() {
        let header = widener().header();
        assert_eq!(
            header,
            vec!["Kenteken", "Merk", "Brandstof", "Export status"]
        );
    }

    #[test]
    fn header_drops_api_dt_and_trailer_columns_and_keeps_allowlist_order() {
        // Input order is shuffled and includes columns the CSV must not show.
        let widener = RowWidener::new(
            vec![
                Column::new(
                    "api_gekentekende_voertuigen_brandstof",
                    "API Gekentekende_voertuigen_brandstof",
                ),
                Column::new("vervaldatum_apk_dt", "Vervaldatum APK DT"),
                Column::new("merk", "Merk"),
                Column::new("oplegger_geremd", "Oplegger geremd"),
                Column::new("handelsbenaming", "Handelsbenaming"),
                Column::new("kenteken", "Kenteken"),
                Column::new("lengte_voertuig_maximum", "Lengte voertuig maximum"),
            ],
            vec![],
        );
        assert_eq!(
            widener.header(),
            vec![
                "Kenteken",
                "Merk",
                "Handelsbenaming",
                "Brandstof",
                "Export status",
            ]
        );
    }

    #[test]
    fn zero_fuel_entries_widen_to_empty_brandstof() {
        let widened = merge_join(&[vehicle("AA001A", "TOYOTA")], &[], false).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(row, vec!["AA001A", "TOYOTA", "", "no_fuel_data"]);
    }

    #[test]
    fn one_fuel_entry_fills_brandstof_with_the_omschrijving() {
        let vehicles = vec![vehicle("AA001A", "TOYOTA")];
        let fuels = vec![fuel("AA001A", 1, "Benzine")];
        let widened = merge_join(&vehicles, &fuels, false).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(row, vec!["AA001A", "TOYOTA", "Benzine", "ok"]);
    }

    #[test]
    fn hybrid_joins_both_fuel_types_in_one_column() {
        // Toyota Prius shape: one plate, two RDW fuel rows.
        let vehicles = vec![vehicle("00GBX4", "TOYOTA")];
        let fuels = vec![
            fuel("00GBX4", 1, "Benzine"),
            fuel("00GBX4", 2, "Elektriciteit"),
        ];
        let widened = merge_join(&vehicles, &fuels, false).unwrap();
        assert_eq!(widened.len(), 1, "a hybrid is still one output row");
        let row = widener().widen(&widened[0]);
        assert_eq!(
            row,
            vec!["00GBX4", "TOYOTA", "Benzine, Elektriciteit", "ok"]
        );
    }

    #[test]
    fn three_fuel_entries_are_joined_in_volgnummer_order() {
        let vehicles = vec![vehicle("AA001A", "TOYOTA")];
        let fuels = vec![
            fuel("AA001A", 1, "Benzine"),
            fuel("AA001A", 2, "Elektriciteit"),
            fuel("AA001A", 3, "CNG"),
        ];
        let widened = merge_join(&vehicles, &fuels, false).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(
            row[2], "Benzine, Elektriciteit, CNG",
            "all types land in the one Brandstof cell"
        );
        assert_eq!(*row.last().unwrap(), "ok");
    }

    // Criterion 2: export_status column at the final position, three values.

    #[test]
    fn happy_path_status_column_is_last_and_ok_when_fuel_present() {
        let vehicles = vec![vehicle("AA001A", "TOYOTA")];
        let fuels = vec![fuel("AA001A", 1, "Benzine")];
        let widened = merge_join(&vehicles, &fuels, false).unwrap();
        let row = widener().widen(&widened[0]);
        let header = widener().header();
        let status_index = header.len() - 1;
        assert_eq!(header[status_index], "Export status");
        assert_eq!(row[status_index], "ok");
        assert_eq!(row.len(), header.len());
    }

    #[test]
    fn edge_status_column_is_no_fuel_data_when_fetch_succeeded_with_zero_rows() {
        let widened = merge_join(&[vehicle("AA001A", "TOYOTA")], &[], false).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(*row.last().unwrap(), "no_fuel_data");
    }

    #[test]
    fn failure_status_column_is_fuel_unavailable_when_range_fetch_failed() {
        let widened = merge_join(&[vehicle("AA001A", "TOYOTA")], &[], true).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(*row.last().unwrap(), "fuel_unavailable");
        // Brandstof stays blank, exactly like the no_fuel_data case; only
        // the status column distinguishes fetch failure from genuine
        // absence.
        assert_eq!(row[2], "");
    }

    #[test]
    fn happy_path_ordinary_values_are_left_untouched() {
        for v in ["Benzine", "TOYOTA", "00GBX4", "104", "1.5", ""] {
            assert_eq!(neutralize_formula(v.to_string()), v);
        }
    }

    #[test]
    fn failure_formula_starting_values_are_neutralized() {
        // A spreadsheet evaluates these; the apostrophe forces text.
        for v in [
            "=cmd|'/c calc'!A1",
            "+1+1",
            "@SUM(A1)",
            "\tformula",
            "\rformula",
        ] {
            let out = neutralize_formula(v.to_string());
            assert!(out.starts_with('\''), "{v} must be escaped, got {out}");
            assert!(
                out.ends_with(v),
                "the original value must be preserved after the quote"
            );
        }
    }

    #[test]
    fn edge_negative_numbers_are_not_escaped_but_negative_text_is() {
        // Escaping every leading '-' would turn real negative measurements
        // into text and corrupt the export.
        for v in ["-5", "-0.5", "-1000"] {
            assert_eq!(neutralize_formula(v.to_string()), v, "{v} is a number");
        }
        for v in ["-1+1", "-cmd", "-=1"] {
            assert!(
                neutralize_formula(v.to_string()).starts_with('\''),
                "{v} is not a number and must be escaped"
            );
        }
    }

    #[test]
    fn failure_formula_in_a_data_value_reaches_the_csv_escaped() {
        let widener = widener();
        let vehicle = vehicle("AA001A", "=cmd|'/c calc'!A1");
        let fuel = fuel("AA001A", 1, "Benzine");
        let rows = merge_join(&[vehicle], &[fuel], false).unwrap();
        let widened = widener.widen(&rows[0]);
        let merk = &widened[1];
        assert!(
            merk.starts_with('\''),
            "a formula in RDW data must not reach the spreadsheet live: {merk}"
        );
    }
}
