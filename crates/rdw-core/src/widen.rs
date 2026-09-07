//! Widens merge-joined vehicle+fuel rows into flat CSV rows.
//!
//! One output row per vehicle. Fuel columns are widened into `fuel1_*`,
//! `fuel2_*`, `fuel3_*` (bounded to `MAX_FUEL_ENTRIES`, enforced upstream by
//! `merge::merge_join`), rather than emitting a separate fuel sheet.

use crate::merge::{WidenedRow, MAX_FUEL_ENTRIES};

/// Column layout used to build the CSV header and to widen each row into
/// exactly that column order.
pub struct RowWidener {
    vehicle_columns: Vec<String>,
    /// Fuel dataset columns excluding `kenteken` (the join key, already
    /// present via the vehicle columns).
    fuel_columns: Vec<String>,
}

impl RowWidener {
    pub fn new(vehicle_columns: Vec<String>, fuel_columns: Vec<String>) -> Self {
        let fuel_columns = fuel_columns
            .into_iter()
            .filter(|c| c != "kenteken")
            .collect();
        Self {
            vehicle_columns,
            fuel_columns,
        }
    }

    /// Header row: vehicle columns, then `fuel1_*`, `fuel2_*`, `fuel3_*`.
    pub fn header(&self) -> Vec<String> {
        let mut header = self.vehicle_columns.clone();
        for slot in 1..=MAX_FUEL_ENTRIES {
            for col in &self.fuel_columns {
                header.push(format!("fuel{slot}_{col}"));
            }
        }
        header
    }

    /// Widen one merge-joined row into a flat CSV record matching `header()`.
    pub fn widen(&self, row: &WidenedRow) -> Vec<String> {
        let mut out = Vec::with_capacity(self.header().len());
        for col in &self.vehicle_columns {
            out.push(field_as_string(row.vehicle.0.get(col)));
        }
        for slot in 0..MAX_FUEL_ENTRIES {
            let fuel = row.fuels.get(slot);
            for col in &self.fuel_columns {
                match fuel {
                    Some(f) => out.push(field_as_string(f.0.get(col))),
                    None => out.push(String::new()),
                }
            }
        }
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
            vec!["kenteken".to_string(), "merk".to_string()],
            vec![
                "kenteken".to_string(),
                "brandstof_volgnummer".to_string(),
                "brandstof_omschrijving".to_string(),
            ],
        )
    }

    #[test]
    fn header_excludes_kenteken_from_fuel_columns_and_widens_three_slots() {
        let header = widener().header();
        assert_eq!(
            header,
            vec![
                "kenteken",
                "merk",
                "fuel1_brandstof_volgnummer",
                "fuel1_brandstof_omschrijving",
                "fuel2_brandstof_volgnummer",
                "fuel2_brandstof_omschrijving",
                "fuel3_brandstof_volgnummer",
                "fuel3_brandstof_omschrijving",
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
        let widened = merge_join(&vehicles, &[]).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(row, vec!["AA001A", "TOYOTA", "", "", "", "", "", ""]);
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
        let widened = merge_join(&vehicles, &fuels).unwrap();
        let row = widener().widen(&widened[0]);
        assert_eq!(row[2], "1");
        assert_eq!(row[3], "Benzine1");
        assert_eq!(row[6], "3");
        assert_eq!(row[7], "Benzine3");
    }
}
