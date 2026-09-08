//! Column-header metadata: fetched once at startup from RDW, cached in
//! `AppState`, with a compiled-in fallback so startup still succeeds if
//! that fetch fails (network error, timeout, or non-2xx response).

use rdw_client::RdwClient;

/// Column names used to build the CSV header, one list per dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMetadata {
    pub vehicle_columns: Vec<String>,
    pub fuel_columns: Vec<String>,
    /// True when either list came from the compiled-in fallback rather
    /// than a live RDW fetch, for startup logging/observability.
    pub used_fallback: bool,
}

/// Compiled-in fallback vehicle columns, used only when the live RDW
/// metadata fetch fails. May lag behind RDW's actual schema if it changes;
/// documented in `docs/RATE_LIMIT.md`/`ARCHITECTURE.md` as a known limitation.
pub fn fallback_vehicle_columns() -> Vec<String> {
    [
        "kenteken",
        "voertuigsoort",
        "merk",
        "handelsbenaming",
        "vervaldatum_apk",
        "datum_tenaamstelling",
        "bruto_bpm",
        "inrichting",
        "aantal_zitplaatsen",
        "eerste_kleur",
        "tweede_kleur",
        "aantal_cilinders",
        "cilinderinhoud",
        "massa_ledig_voertuig",
        "toegestane_maximum_massa_voertuig",
        "massa_rijklaar",
        "maximum_massa_trekken_ongeremd",
        "maximum_trekken_massa_geremd",
        "datum_eerste_toelating",
        "datum_eerste_tenaamstelling_in_nederland",
        "wacht_op_keuren",
        "catalogusprijs",
        "wam_verzekerd",
        "maximale_constructiesnelheid",
        "laadvermogen",
        "oplegger_geremd",
        "aanhangwagen_autonoom_geremd",
        "aanhangwagen_middenas_geremd",
        "aantal_staanplaatsen",
        "aantal_deuren",
        "aantal_wielen",
        "afstand_hart_koppeling_tot_achterzijde_voertuig",
        "afstand_voorzijde_voertuig_tot_hart_koppeling",
        "afwijkende_maximum_snelheid",
        "lengte",
        "breedte",
        "europese_voertuigcategorie",
        "europese_voertuigcategorie_toevoeging",
        "europese_uitvoeringcategorie_toevoeging",
        "plaats_chassisnummer",
        "technische_max_massa_voertuig",
        "type",
        "type_gasinstallatie",
        "typegoedkeuringsnummer",
        "variant",
        "uitvoering",
        "volgnummer_wijziging_eu_typegoedkeuring",
        "vermogen_massarijklaar",
        "wielbasis",
        "export_indicator",
        "openstaande_terugroepactie_indicator",
        "vervaldatum_tachograaf",
        "taxi_indicator",
        "maximum_massa_samenstelling",
        "aantal_rolstoelplaatsen",
        "maximum_ondersteunende_snelheid",
        "jaar_laatste_registratie_tellerstand",
        "tellerstandoordeel",
        "code_toelichting_tellerstandoordeel",
        "tenaamstellen_mogelijk",
        "vervaldatum_apk_dt",
        "datum_tenaamstelling_dt",
        "datum_eerste_toelating_dt",
        "datum_eerste_tenaamstelling_in_nederland_dt",
        "vervaldatum_tachograaf_dt",
        "maximum_last_onder_de_vooras_sen_tezamen_koppeling",
        "type_remsysteem_voertuig_code",
        "rupsonderstelconfiguratiecode",
        "wielbasis_voertuig_minimum",
        "wielbasis_voertuig_maximum",
        "lengte_voertuig_minimum",
        "lengte_voertuig_maximum",
        "breedte_voertuig_minimum",
        "breedte_voertuig_maximum",
        "hoogte_voertuig",
        "hoogte_voertuig_minimum",
        "hoogte_voertuig_maximum",
        "massa_bedrijfsklaar_minimaal",
        "massa_bedrijfsklaar_maximaal",
        "technisch_toelaatbaar_massa_koppelpunt",
        "maximum_massa_technisch_maximaal",
        "maximum_massa_technisch_minimaal",
        "subcategorie_nederland",
        "verticale_belasting_koppelpunt_getrokken_voertuig",
        "zuinigheidsclassificatie",
        "registratie_datum_goedkeuring_afschrijvingsmoment_bpm",
        "registratie_datum_goedkeuring_afschrijvingsmoment_bpm_dt",
        "gem_lading_wrde",
        "aerodyn_voorz",
        "massa_alt_aandr",
        "verl_cab_ind",
        "aantal_passagiers_zitplaatsen_wettelijk",
        "aanwijzingsnummer",
        "api_gekentekende_voertuigen_assen",
        "api_gekentekende_voertuigen_brandstof",
        "api_gekentekende_voertuigen_carrosserie",
        "api_gekentekende_voertuigen_carrosserie_specifiek",
        "api_gekentekende_voertuigen_voertuigklasse",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Compiled-in fallback fuel columns.
pub fn fallback_fuel_columns() -> Vec<String> {
    [
        "kenteken",
        "brandstof_volgnummer",
        "brandstof_omschrijving",
        "brandstofverbruik_gecombineerd",
        "co2_uitstoot_gecombineerd",
        "co2_uitstoot_gewogen",
        "geluidsniveau_rijdend",
        "geluidsniveau_stationair",
        "emissiecode_omschrijving",
        "milieuklasse_eg_goedkeuring_licht",
        "milieuklasse_eg_goedkeuring_zwaar",
        "uitstoot_deeltjes_licht",
        "uitstoot_deeltjes_zwaar",
        "nettomaximumvermogen",
        "nominaal_continu_maximumvermogen",
        "toerental_geluidsniveau",
        "emis_deeltjes_type1_wltp",
        "emissie_co2_gecombineerd_wltp",
        "emis_co2_gewogen_gecombineerd_wltp",
        "brandstof_verbruik_gecombineerd_wltp",
        "brandstof_verbruik_gewogen_gecombineerd_wltp",
        "elektrisch_verbruik_enkel_elektrisch_wltp",
        "actie_radius_enkel_elektrisch_wltp",
        "elektrisch_verbruik_extern_opladen_wltp",
        "actie_radius_extern_opladen_wltp",
        "max_vermogen_15_minuten",
        "netto_max_vermogen_elektrisch",
        "klasse_hybride_elektrisch_voertuig",
        "opgegeven_maximum_snelheid",
        "uitlaatemissieniveau",
        "co2_emissieklasse",
        "brandstofverbruik_gewogen_gecombineerd",
        "elektriciteitsverbruik_gewogen_gecombineerd",
        "actieradius_extern_oplaadbaar",
        "actieradius",
        "elektriciteitsverbruik_volledig_elektrisch",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

/// Fetch column metadata from RDW for both datasets, falling back to the
/// compiled-in lists (per dataset, independently) on any failure. Never
/// panics or blocks startup.
pub async fn load_column_metadata(client: &RdwClient) -> ColumnMetadata {
    let mut used_fallback = false;

    let vehicle_columns = match client
        .fetch_column_names(rdw_client::VEHICLE_DATASET_ID)
        .await
    {
        Ok(cols) => cols,
        Err(err) => {
            tracing::warn!(error = %err, "failed to fetch RDW vehicle column metadata; using compiled-in fallback");
            used_fallback = true;
            fallback_vehicle_columns()
        }
    };

    let fuel_columns = match client.fetch_column_names(rdw_client::FUEL_DATASET_ID).await {
        Ok(cols) => cols,
        Err(err) => {
            tracing::warn!(error = %err, "failed to fetch RDW fuel column metadata; using compiled-in fallback");
            used_fallback = true;
            fallback_fuel_columns()
        }
    };

    ColumnMetadata {
        vehicle_columns,
        fuel_columns,
        used_fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn happy_path_uses_live_metadata_when_rdw_responds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/{}.json", rdw_client::VEHICLE_DATASET_ID)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "columns": [{ "fieldName": "kenteken" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{}.json", rdw_client::FUEL_DATASET_ID)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "columns": [{ "fieldName": "kenteken" }, { "fieldName": "brandstof_volgnummer" }]
            })))
            .mount(&server)
            .await;

        let client = RdwClient::new(None).with_metadata_base(server.uri());
        let metadata = load_column_metadata(&client).await;
        assert!(!metadata.used_fallback);
        assert_eq!(metadata.vehicle_columns, vec!["kenteken".to_string()]);
    }

    #[test]
    fn fallback_columns_widen_to_full_204_column_header() {
        // 98 vehicle columns + 3 fuel slots * 35 non-kenteken fuel columns
        // + 1 export_status column (last position) = 204.
        assert_eq!(fallback_vehicle_columns().len(), 98);
        assert_eq!(fallback_fuel_columns().len(), 36);

        let widener =
            crate::widen::RowWidener::new(fallback_vehicle_columns(), fallback_fuel_columns());
        let header = widener.header();
        assert_eq!(header.len(), 98 + 3 * 35 + 1);
        assert_eq!(header.last().unwrap(), "export_status");
    }

    #[tokio::test]
    async fn failure_rdw_metadata_500_falls_back_and_still_starts() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = RdwClient::new(None)
            .with_metadata_base(server.uri())
            .with_retry_config(rdw_client::RetryConfig {
                max_attempts: 5,
                initial_backoff: std::time::Duration::ZERO,
                max_backoff: std::time::Duration::ZERO,
            });
        let metadata = load_column_metadata(&client).await;
        assert!(metadata.used_fallback);
        assert_eq!(metadata.vehicle_columns, fallback_vehicle_columns());
        assert_eq!(metadata.fuel_columns, fallback_fuel_columns());
    }
}
