//! Merge-join of sorted vehicle and fuel cursors.
//!
//! Both inputs must already be sorted ascending by `kenteken` (fuel rows
//! additionally by `brandstof_volgnummer` within a `kenteken` group), which
//! is how the RDW client requests them. This module never builds an
//! in-memory hash join of the whole dataset; it walks both cursors once.

use rdw_client::{FuelRow, VehicleRow};

pub const MAX_FUEL_ENTRIES: usize = 3;

/// One vehicle widened with up to `MAX_FUEL_ENTRIES` fuel entries.
#[derive(Debug, Clone)]
pub struct WidenedRow {
    pub vehicle: VehicleRow,
    pub fuels: Vec<FuelRow>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MergeJoinError {
    #[error("vehicle row is missing a kenteken")]
    MissingVehicleKenteken,
    #[error("fuel row is missing a kenteken")]
    MissingFuelKenteken,
    #[error("fuel row for kenteken {0} is missing brandstof_volgnummer")]
    MissingVolgnummer(String),
    #[error("vehicle cursor is not sorted ascending by kenteken at {0}")]
    UnsortedVehicles(String),
    #[error("fuel cursor is not sorted ascending by kenteken/volgnummer at {0}")]
    UnsortedFuel(String),
    #[error("fuel entries for kenteken {0} are out of sequence (expected volgnummer {expected}, got {actual})", expected = .1, actual = .2)]
    FuelInversion(String, u32, u32),
    #[error("kenteken {0} has more than {max} fuel entries", max = MAX_FUEL_ENTRIES)]
    TooManyFuelEntries(String),
}

/// Merge-join a sorted batch of vehicles with a sorted batch of fuel rows.
///
/// - A fuel row whose `kenteken` has no matching vehicle (stale RDW state)
///   is skipped, not an error.
/// - A fuel row group out of sequence, or a vehicle/fuel cursor found not
///   sorted ascending, fails loudly rather than silently producing
///   corrupt output.
/// - A 4th (or later) fuel entry for one vehicle fails loudly rather than
///   being silently dropped.
pub fn merge_join(
    vehicles: &[VehicleRow],
    fuel: &[FuelRow],
) -> Result<Vec<WidenedRow>, MergeJoinError> {
    validate_vehicle_order(vehicles)?;
    validate_fuel_order(fuel)?;

    let mut result = Vec::with_capacity(vehicles.len());
    let mut fi = 0usize;

    for v in vehicles {
        let vk = v.kenteken().ok_or(MergeJoinError::MissingVehicleKenteken)?;

        // Skip orphan fuel rows (kenteken not present in this vehicle batch).
        while fi < fuel.len() {
            let fk = fuel[fi]
                .kenteken()
                .ok_or(MergeJoinError::MissingFuelKenteken)?;
            if fk < vk {
                fi += 1;
            } else {
                break;
            }
        }

        let mut fuels_for_v = Vec::new();
        while fi < fuel.len() {
            let fk = fuel[fi]
                .kenteken()
                .ok_or(MergeJoinError::MissingFuelKenteken)?;
            if fk != vk {
                break;
            }
            let expected = fuels_for_v.len() as u32 + 1;
            let actual = fuel[fi]
                .volgnummer()
                .ok_or_else(|| MergeJoinError::MissingVolgnummer(vk.to_string()))?;
            if actual != expected {
                return Err(MergeJoinError::FuelInversion(
                    vk.to_string(),
                    expected,
                    actual,
                ));
            }
            fuels_for_v.push(fuel[fi].clone());
            fi += 1;
            if fuels_for_v.len() > MAX_FUEL_ENTRIES {
                return Err(MergeJoinError::TooManyFuelEntries(vk.to_string()));
            }
        }

        result.push(WidenedRow {
            vehicle: v.clone(),
            fuels: fuels_for_v,
        });
    }

    Ok(result)
}

fn validate_vehicle_order(vehicles: &[VehicleRow]) -> Result<(), MergeJoinError> {
    let mut prev: Option<&str> = None;
    for v in vehicles {
        let k = v.kenteken().ok_or(MergeJoinError::MissingVehicleKenteken)?;
        if let Some(p) = prev {
            if k <= p {
                return Err(MergeJoinError::UnsortedVehicles(k.to_string()));
            }
        }
        prev = Some(k);
    }
    Ok(())
}

fn validate_fuel_order(fuel: &[FuelRow]) -> Result<(), MergeJoinError> {
    let mut prev: Option<(&str, u32)> = None;
    for f in fuel {
        let k = f.kenteken().ok_or(MergeJoinError::MissingFuelKenteken)?;
        let vol = f
            .volgnummer()
            .ok_or_else(|| MergeJoinError::MissingVolgnummer(k.to_string()))?;
        if let Some((pk, pv)) = prev {
            let out_of_order = k < pk || (k == pk && vol <= pv);
            if out_of_order {
                return Err(MergeJoinError::UnsortedFuel(k.to_string()));
            }
        }
        prev = Some((k, vol));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rdw_client::{FuelRow, VehicleRow};
    use serde_json::json;

    fn vehicle(kenteken: &str) -> VehicleRow {
        VehicleRow(
            json!({ "kenteken": kenteken, "merk": "TOYOTA" })
                .as_object()
                .unwrap()
                .clone(),
        )
    }

    fn fuel(kenteken: &str, volgnummer: u32) -> FuelRow {
        FuelRow(
            json!({ "kenteken": kenteken, "brandstof_volgnummer": volgnummer.to_string() })
                .as_object()
                .unwrap()
                .clone(),
        )
    }

    #[test]
    fn happy_path_two_vehicles_different_fuel_counts() {
        let vehicles = vec![vehicle("AA001A"), vehicle("BB002B")];
        let fuels = vec![fuel("AA001A", 1), fuel("AA001A", 2), fuel("BB002B", 1)];
        let widened = merge_join(&vehicles, &fuels).unwrap();
        assert_eq!(widened.len(), 2);
        assert_eq!(widened[0].fuels.len(), 2);
        assert_eq!(widened[1].fuels.len(), 1);
    }

    #[test]
    fn edge_stale_fuel_entry_with_no_matching_vehicle_is_skipped() {
        let vehicles = vec![vehicle("BB002B")];
        // "AA001A" fuel entry has no matching vehicle in this batch.
        let fuels = vec![fuel("AA001A", 1), fuel("BB002B", 1)];
        let widened = merge_join(&vehicles, &fuels).unwrap();
        assert_eq!(widened.len(), 1);
        assert_eq!(widened[0].vehicle.kenteken(), Some("BB002B"));
        assert_eq!(widened[0].fuels.len(), 1);
    }

    #[test]
    fn edge_vehicle_with_zero_fuel_entries() {
        let vehicles = vec![vehicle("AA001A")];
        let fuels = vec![];
        let widened = merge_join(&vehicles, &fuels).unwrap();
        assert_eq!(widened[0].fuels.len(), 0);
    }

    #[test]
    fn failure_fourth_fuel_entry_fails_loudly() {
        let vehicles = vec![vehicle("AA001A")];
        let fuels = vec![
            fuel("AA001A", 1),
            fuel("AA001A", 2),
            fuel("AA001A", 3),
            fuel("AA001A", 4),
        ];
        let err = merge_join(&vehicles, &fuels).unwrap_err();
        assert_eq!(
            err,
            MergeJoinError::TooManyFuelEntries("AA001A".to_string())
        );
    }

    #[test]
    fn failure_inverted_fuel_sequence_is_detected() {
        let vehicles = vec![vehicle("AA001A")];
        // volgnummer 1 then 3: sequence gap/inversion relative to expected 2.
        let fuels = vec![fuel("AA001A", 1), fuel("AA001A", 3)];
        let err = merge_join(&vehicles, &fuels).unwrap_err();
        assert!(matches!(err, MergeJoinError::FuelInversion(_, 2, 3)));
    }

    #[test]
    fn failure_unsorted_vehicle_cursor_is_detected() {
        let vehicles = vec![vehicle("BB002B"), vehicle("AA001A")];
        let err = merge_join(&vehicles, &[]).unwrap_err();
        assert_eq!(err, MergeJoinError::UnsortedVehicles("AA001A".to_string()));
    }

    #[test]
    fn failure_unsorted_fuel_cursor_across_kenteken_is_detected() {
        let vehicles = vec![vehicle("AA001A"), vehicle("BB002B")];
        // Fuel page returned out of order relative to kenteken ordering.
        let fuels = vec![fuel("BB002B", 1), fuel("AA001A", 1)];
        let err = merge_join(&vehicles, &fuels).unwrap_err();
        assert_eq!(err, MergeJoinError::UnsortedFuel("AA001A".to_string()));
    }
}
