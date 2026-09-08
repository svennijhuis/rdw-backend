//! Widens merge-joined vehicle+fuel rows into flat CSV rows.
//!
//! One output row per vehicle. Fuel columns are widened into `fuel1_*`,
//! `fuel2_*`, `fuel3_*` (bounded to `MAX_FUEL_ENTRIES`, enforced upstream by
//! `merge::merge_join`), rather than emitting a separate fuel sheet.

use crate::merge::{WidenedRow, MAX_FUEL_ENTRIES};
use crate::metadata::Column;

/// Header label for the export-status column. Kept as a named constant so the
/// header and any consumer looking the column up cannot drift apart.
pub const EXPORT_STATUS_HEADER: &str = "Export status";

/// Column layout used to build the CSV header and to widen each row into
/// exactly that column order.
pub struct RowWidener {
    vehicle_columns: Vec<Column>,
    /// Fuel dataset columns excluding `kenteken` (the join key, already
    /// present via the vehicle columns).
    fuel_columns: Vec<Column>,
}

impl RowWidener {
    pub fn new(vehicle_columns: Vec<Column>, fuel_columns: Vec<Column>) -> Self {
        let fuel_columns = fuel_columns
            .into_iter()
            .filter(|c| c.field != "kenteken")
            .collect();
        Self {
            vehicle_columns,
            fuel_columns,
        }
    }

    /// Header row: vehicle columns, then each fuel slot, then the export
    /// status last. The status column's position is final: it must never move
    /// earlier, because `widen()` builds rows positionally and existing
    /// consumers (and tests) assert fixed column indices for the 203-wide
    /// vehicle+fuel prefix.
    ///
    /// Headers use RDW's own display names rather than its `fieldName` keys,
    /// so a reader opening the CSV sees "Gemiddelde Lading Waarde" instead of
    /// `gem_lading_wrde`. Fuel slots are prefixed "Brandstof N - " so all the
    /// columns of one slot sort together.
    pub fn header(&self) -> Vec<String> {
        let mut header: Vec<String> = self
            .vehicle_columns
            .iter()
            .map(|c| c.display.clone())
            .collect();
        for slot in 1..=MAX_FUEL_ENTRIES {
            for col in &self.fuel_columns {
                header.push(format!("Brandstof {slot} - {}", col.display));
            }
        }
        header.push(EXPORT_STATUS_HEADER.to_string());
        header
    }

    /// Widen one merge-joined row into a flat CSV record matching `header()`.
    pub fn widen(&self, row: &WidenedRow) -> Vec<String> {
        let mut out = Vec::with_capacity(self.header().len());
        for col in &self.vehicle_columns {
            out.push(field_as_string(row.vehicle.0.get(&col.field)));
        }
        for slot in 0..MAX_FUEL_ENTRIES {
            let fuel = row.fuels.get(slot);
            for col in &self.fuel_columns {
                match fuel {
                    Some(f) => out.push(field_as_string(f.0.get(&col.field))),
                    None => out.push(String::new()),
                }
            }
        }
        out.push(row.export_status.as_str().to_string());
        out
    }
}

fn field_as_string(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
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

    #[test]
    fn header_excludes_kenteken_from_fuel_columns_and_widens_three_slots() {
        let header = widener().header();
        assert_eq!(
            header,
            vec![
                "Kenteken",
                "Merk",
                "Brandstof 1 - Brandstof volgnummer",
                "Brandstof 1 - Brandstof omschrijving",
                "Brandstof 2 - Brandstof volgnummer",
                "Brandstof 2 - Brandstof omschrijving",
                "Brandstof 3 - Brandstof volgnummer",
                "Brandstof 3 - Brandstof omschrijving",
                "Export status",
            ]
        );
    }

    #[test]
    fn zero_fuel_entries_widen_to_empty_fuel_columns() {
        let vehicles = vec![VehicleRow(
            json!({ "kenteken": "AA001A", "merk": "TOYOTA" })
                .as_object()
                .unwrap()
                .clone(),
        )];
        let widened = merge_join(&vehicles, &[], false).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(
            row,
            vec!["AA001A", "TOYOTA", "", "", "", "", "", "", "no_fuel_data"]
        );
    }

    #[test]
    fn three_fuel_entries_populate_all_slots() {
        let vehicles = vec![VehicleRow(
            json!({ "kenteken": "AA001A", "merk": "TOYOTA" })
                .as_object()
                .unwrap()
                .clone(),
        )];
        let fuels: Vec<FuelRow> = (1..=3)
            .map(|n| {
                FuelRow(
                    json!({ "kenteken": "AA001A", "brandstof_volgnummer": n.to_string(), "brandstof_omschrijving": format!("Benzine{n}") })
                        .as_object()
                        .unwrap()
                        .clone(),
                )
            })
            .collect();
        let widened = merge_join(&vehicles, &fuels, false).unwrap();
        let row = widener().widen(&widened[0]);
        // Existing positional assertions on the vehicle/fuel prefix must
        // stay unchanged: the status column is appended, never inserted.
        assert_eq!(row[2], "1");
        assert_eq!(row[3], "Benzine1");
        assert_eq!(row[6], "3");
        assert_eq!(row[7], "Benzine3");
    }

    // Criterion 2: export_status column at the final position, three values.

    #[test]
    fn happy_path_status_column_is_last_and_ok_when_fuel_present() {
        let vehicles = vec![VehicleRow(
            json!({ "kenteken": "AA001A", "merk": "TOYOTA" })
                .as_object()
                .unwrap()
                .clone(),
        )];
        let fuels = vec![FuelRow(
            json!({ "kenteken": "AA001A", "brandstof_volgnummer": "1", "brandstof_omschrijving": "Benzine" })
                .as_object()
                .unwrap()
                .clone(),
        )];
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
        let vehicles = vec![VehicleRow(
            json!({ "kenteken": "AA001A", "merk": "TOYOTA" })
                .as_object()
                .unwrap()
                .clone(),
        )];
        let widened = merge_join(&vehicles, &[], false).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(*row.last().unwrap(), "no_fuel_data");
    }

    #[test]
    fn failure_status_column_is_fuel_unavailable_when_range_fetch_failed() {
        let vehicles = vec![VehicleRow(
            json!({ "kenteken": "AA001A", "merk": "TOYOTA" })
                .as_object()
                .unwrap()
                .clone(),
        )];
        let widened = merge_join(&vehicles, &[], true).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(*row.last().unwrap(), "fuel_unavailable");
        // Fuel columns stay blank, exactly like the no_fuel_data case; only
        // the status column distinguishes fetch failure from genuine
        // absence.
        assert_eq!(&row[2..8], ["", "", "", "", "", ""]);
    }
}
