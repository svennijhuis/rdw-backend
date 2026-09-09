//! Column-header metadata: fetched once at startup from RDW, cached in
//! `AppState`, with a compiled-in fallback so startup still succeeds if
//! that fetch fails (network error, timeout, or non-2xx response).

use rdw_client::RdwClient;

/// One dataset column: the machine key used to read a value out of a data
/// row, and RDW's own human-readable label used for the CSV header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Socrata `fieldName`, e.g. `gem_lading_wrde`. Used to look values up.
    pub field: String,
    /// Socrata `name`, e.g. "Gemiddelde Lading Waarde". Shown in the header.
    pub display: String,
}

impl Column {
    pub fn new(field: impl Into<String>, display: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            display: display.into(),
        }
    }
}

/// Column names used to build the CSV header, one list per dataset.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnMetadata {
    pub vehicle_columns: Vec<Column>,
    pub fuel_columns: Vec<Column>,
    /// True when either list came from the compiled-in fallback rather
    /// than a live RDW fetch, for startup logging/observability.
    pub used_fallback: bool,
}

/// Compiled-in fallback vehicle columns, used only when the live RDW
/// metadata fetch fails. May lag behind RDW's actual schema if it changes;
/// documented in `docs/RATE_LIMIT.md`/`ARCHITECTURE.md` as a known limitation.
pub fn fallback_vehicle_columns() -> Vec<Column> {
    [
        ("kenteken", "Kenteken"),
        ("voertuigsoort", "Voertuigsoort"),
        ("merk", "Merk"),
        ("handelsbenaming", "Handelsbenaming"),
        ("vervaldatum_apk", "Vervaldatum APK"),
        ("datum_tenaamstelling", "Datum tenaamstelling"),
        ("bruto_bpm", "Bruto BPM"),
        ("inrichting", "Inrichting"),
        ("aantal_zitplaatsen", "Aantal zitplaatsen"),
        ("eerste_kleur", "Eerste kleur"),
        ("tweede_kleur", "Tweede kleur"),
        ("aantal_cilinders", "Aantal cilinders"),
        ("cilinderinhoud", "Cilinderinhoud"),
        ("massa_ledig_voertuig", "Massa ledig voertuig"),
        (
            "toegestane_maximum_massa_voertuig",
            "Toegestane maximum massa voertuig",
        ),
        ("massa_rijklaar", "Massa rijklaar"),
        (
            "maximum_massa_trekken_ongeremd",
            "Maximum massa trekken ongeremd",
        ),
        (
            "maximum_trekken_massa_geremd",
            "Maximum trekken massa geremd",
        ),
        ("datum_eerste_toelating", "Datum eerste toelating"),
        (
            "datum_eerste_tenaamstelling_in_nederland",
            "Datum eerste tenaamstelling in Nederland",
        ),
        ("wacht_op_keuren", "Wacht op keuren"),
        ("catalogusprijs", "Catalogusprijs"),
        ("wam_verzekerd", "WAM verzekerd"),
        (
            "maximale_constructiesnelheid",
            "Maximale constructiesnelheid",
        ),
        ("laadvermogen", "Laadvermogen"),
        ("oplegger_geremd", "Oplegger geremd"),
        (
            "aanhangwagen_autonoom_geremd",
            "Aanhangwagen autonoom geremd",
        ),
        (
            "aanhangwagen_middenas_geremd",
            "Aanhangwagen middenas geremd",
        ),
        ("aantal_staanplaatsen", "Aantal staanplaatsen"),
        ("aantal_deuren", "Aantal deuren"),
        ("aantal_wielen", "Aantal wielen"),
        (
            "afstand_hart_koppeling_tot_achterzijde_voertuig",
            "Afstand hart koppeling tot achterzijde voertuig",
        ),
        (
            "afstand_voorzijde_voertuig_tot_hart_koppeling",
            "Afstand voorzijde voertuig tot hart koppeling",
        ),
        ("afwijkende_maximum_snelheid", "Afwijkende maximum snelheid"),
        ("lengte", "Lengte"),
        ("breedte", "Breedte"),
        ("europese_voertuigcategorie", "Europese voertuigcategorie"),
        (
            "europese_voertuigcategorie_toevoeging",
            "Europese voertuigcategorie toevoeging",
        ),
        (
            "europese_uitvoeringcategorie_toevoeging",
            "Europese uitvoeringcategorie toevoeging",
        ),
        ("plaats_chassisnummer", "Plaats chassisnummer"),
        (
            "technische_max_massa_voertuig",
            "Technische max. massa voertuig",
        ),
        ("type", "Type"),
        ("type_gasinstallatie", "Type gasinstallatie"),
        ("typegoedkeuringsnummer", "Typegoedkeuringsnummer"),
        ("variant", "Variant"),
        ("uitvoering", "Uitvoering"),
        (
            "volgnummer_wijziging_eu_typegoedkeuring",
            "Volgnummer wijziging EU typegoedkeuring",
        ),
        ("vermogen_massarijklaar", "Vermogen massarijklaar"),
        ("wielbasis", "Wielbasis"),
        ("export_indicator", "Export indicator"),
        (
            "openstaande_terugroepactie_indicator",
            "Openstaande terugroepactie indicator",
        ),
        ("vervaldatum_tachograaf", "Vervaldatum tachograaf"),
        ("taxi_indicator", "Taxi indicator"),
        ("maximum_massa_samenstelling", "Maximum massa samenstelling"),
        ("aantal_rolstoelplaatsen", "Aantal rolstoelplaatsen"),
        (
            "maximum_ondersteunende_snelheid",
            "Maximum ondersteunende snelheid",
        ),
        (
            "jaar_laatste_registratie_tellerstand",
            "Jaar laatste registratie tellerstand",
        ),
        ("tellerstandoordeel", "Tellerstandoordeel"),
        (
            "code_toelichting_tellerstandoordeel",
            "Code toelichting tellerstandoordeel",
        ),
        ("tenaamstellen_mogelijk", "Tenaamstellen mogelijk"),
        ("vervaldatum_apk_dt", "Vervaldatum APK DT"),
        ("datum_tenaamstelling_dt", "Datum tenaamstelling DT"),
        ("datum_eerste_toelating_dt", "Datum eerste toelating DT"),
        (
            "datum_eerste_tenaamstelling_in_nederland_dt",
            "Datum eerste tenaamstelling in Nederland DT",
        ),
        ("vervaldatum_tachograaf_dt", "Vervaldatum tachograaf DT"),
        (
            "maximum_last_onder_de_vooras_sen_tezamen_koppeling",
            "Maximum last onder de vooras(sen) (tezamen)/koppeling",
        ),
        (
            "type_remsysteem_voertuig_code",
            "Type remsysteem voertuig code",
        ),
        (
            "rupsonderstelconfiguratiecode",
            "Rupsonderstelconfiguratiecode",
        ),
        ("wielbasis_voertuig_minimum", "Wielbasis voertuig minimum"),
        ("wielbasis_voertuig_maximum", "Wielbasis voertuig maximum"),
        ("lengte_voertuig_minimum", "Lengte voertuig minimum"),
        ("lengte_voertuig_maximum", "Lengte voertuig maximum"),
        ("breedte_voertuig_minimum", "Breedte voertuig minimum"),
        ("breedte_voertuig_maximum", "Breedte voertuig maximum"),
        ("hoogte_voertuig", "Hoogte voertuig"),
        ("hoogte_voertuig_minimum", "Hoogte voertuig minimum"),
        ("hoogte_voertuig_maximum", "Hoogte voertuig maximum"),
        (
            "massa_bedrijfsklaar_minimaal",
            "Massa bedrijfsklaar minimaal",
        ),
        (
            "massa_bedrijfsklaar_maximaal",
            "Massa bedrijfsklaar maximaal",
        ),
        (
            "technisch_toelaatbaar_massa_koppelpunt",
            "Technisch toelaatbaar massa koppelpunt",
        ),
        (
            "maximum_massa_technisch_maximaal",
            "Maximum massa technisch maximaal",
        ),
        (
            "maximum_massa_technisch_minimaal",
            "Maximum massa technisch minimaal",
        ),
        ("subcategorie_nederland", "Subcategorie Nederland"),
        (
            "verticale_belasting_koppelpunt_getrokken_voertuig",
            "Verticale belasting koppelpunt getrokken voertuig",
        ),
        ("zuinigheidsclassificatie", "Zuinigheidsclassificatie"),
        (
            "registratie_datum_goedkeuring_afschrijvingsmoment_bpm",
            "Registratie datum goedkeuring (afschrijvingsmoment BPM)",
        ),
        (
            "registratie_datum_goedkeuring_afschrijvingsmoment_bpm_dt",
            "Registratie datum goedkeuring (afschrijvingsmoment BPM) DT",
        ),
        ("gem_lading_wrde", "Gemiddelde Lading Waarde"),
        ("aerodyn_voorz", "Aerodynamische voorziening of uitrusting"),
        (
            "massa_alt_aandr",
            "Additionele massa alternatieve aandrijving",
        ),
        ("verl_cab_ind", "Verlengde cabine indicator"),
        (
            "aantal_passagiers_zitplaatsen_wettelijk",
            "Aantal passagiers zitplaatsen wettelijk",
        ),
        ("aanwijzingsnummer", "Aanwijzingsnummer"),
        (
            "api_gekentekende_voertuigen_assen",
            "API Gekentekende_voertuigen_assen",
        ),
        (
            "api_gekentekende_voertuigen_brandstof",
            "API Gekentekende_voertuigen_brandstof",
        ),
        (
            "api_gekentekende_voertuigen_carrosserie",
            "API Gekentekende_voertuigen_carrosserie",
        ),
        (
            "api_gekentekende_voertuigen_carrosserie_specifiek",
            "API Gekentekende_voertuigen_carrosserie_specifiek",
        ),
        (
            "api_gekentekende_voertuigen_voertuigklasse",
            "API Gekentekende_voertuigen_voertuigklasse",
        ),
    ]
    .into_iter()
    .map(|(f, n)| Column::new(f, n))
    .collect()
}

/// Compiled-in fallback fuel columns.
pub fn fallback_fuel_columns() -> Vec<Column> {
    [
        ("kenteken", "Kenteken"),
        ("brandstof_volgnummer", "Brandstof volgnummer"),
        ("brandstof_omschrijving", "Brandstof omschrijving"),
        (
            "brandstofverbruik_gecombineerd",
            "Brandstofverbruik gecombineerd",
        ),
        ("co2_uitstoot_gecombineerd", "CO2 uitstoot gecombineerd"),
        ("co2_uitstoot_gewogen", "CO2 uitstoot gewogen"),
        ("geluidsniveau_rijdend", "Geluidsniveau rijdend"),
        ("geluidsniveau_stationair", "Geluidsniveau stationair"),
        ("emissiecode_omschrijving", "Emissieklasse"),
        (
            "milieuklasse_eg_goedkeuring_licht",
            "Milieuklasse EG Goedkeuring (licht)",
        ),
        (
            "milieuklasse_eg_goedkeuring_zwaar",
            "Milieuklasse EG Goedkeuring (zwaar)",
        ),
        ("uitstoot_deeltjes_licht", "Uitstoot deeltjes (licht)"),
        ("uitstoot_deeltjes_zwaar", "Uitstoot deeltjes (zwaar)"),
        ("nettomaximumvermogen", "Nettomaximumvermogen"),
        (
            "nominaal_continu_maximumvermogen",
            "Nominaal continu maximumvermogen",
        ),
        ("toerental_geluidsniveau", "Toerental geluidsniveau"),
        ("emis_deeltjes_type1_wltp", "Emissie deeltjes type1 wltp"),
        (
            "emissie_co2_gecombineerd_wltp",
            "Emissie co2 gecombineerd wltp",
        ),
        (
            "emis_co2_gewogen_gecombineerd_wltp",
            "Emissie co2 gewogen gecombineerd wltp",
        ),
        (
            "brandstof_verbruik_gecombineerd_wltp",
            "Brandstof verbruik gecombineerd wltp",
        ),
        (
            "brandstof_verbruik_gewogen_gecombineerd_wltp",
            "Brandstof verbruik gewogen gecombineerd wltp",
        ),
        (
            "elektrisch_verbruik_enkel_elektrisch_wltp",
            "Elektrisch verbruik enkel elektrisch wltp",
        ),
        (
            "actie_radius_enkel_elektrisch_wltp",
            "Actie radius enkel elektrisch wltp",
        ),
        (
            "elektrisch_verbruik_extern_opladen_wltp",
            "Elektrisch verbruik extern opladen wltp",
        ),
        (
            "actie_radius_extern_opladen_wltp",
            "Actie radius extern opladen wltp",
        ),
        ("max_vermogen_15_minuten", "Max vermogen 15 minuten"),
        (
            "netto_max_vermogen_elektrisch",
            "Netto max vermogen elektrisch",
        ),
        (
            "klasse_hybride_elektrisch_voertuig",
            "Klasse hybride elektrisch voertuig",
        ),
        ("opgegeven_maximum_snelheid", "Opgegeven maximum snelheid"),
        ("uitlaatemissieniveau", "Uitlaatemissieniveau"),
        ("co2_emissieklasse", "CO2 emissieklasse"),
        (
            "brandstofverbruik_gewogen_gecombineerd",
            "Brandstofverbruik gewogen gecombineerd",
        ),
        (
            "elektriciteitsverbruik_gewogen_gecombineerd",
            "Elektriciteitsverbruik gewogen gecombineerd",
        ),
        (
            "actieradius_extern_oplaadbaar",
            "Actieradius extern oplaadbaar",
        ),
        ("actieradius", "Actieradius"),
        (
            "elektriciteitsverbruik_volledig_elektrisch",
            "Elektriciteitsverbruik volledig elektrisch",
        ),
    ]
    .into_iter()
    .map(|(f, n)| Column::new(f, n))
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
        Ok(cols) => cols.into_iter().map(|(f, n)| Column::new(f, n)).collect(),
        Err(err) => {
            tracing::warn!(error = %err, "failed to fetch RDW vehicle column metadata; using compiled-in fallback");
            used_fallback = true;
            fallback_vehicle_columns()
        }
    };

    let fuel_columns = match client.fetch_column_names(rdw_client::FUEL_DATASET_ID).await {
        Ok(cols) => cols.into_iter().map(|(f, n)| Column::new(f, n)).collect(),
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
                "columns": [{ "fieldName": "kenteken", "name": "Kenteken" }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/{}.json", rdw_client::FUEL_DATASET_ID)))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "columns": [
                    { "fieldName": "kenteken", "name": "Kenteken" },
                    { "fieldName": "brandstof_volgnummer", "name": "Brandstof volgnummer" }
                ]
            })))
            .mount(&server)
            .await;

        let client = RdwClient::new(None).with_metadata_base(server.uri());
        let metadata = load_column_metadata(&client).await;
        assert!(!metadata.used_fallback);
        assert_eq!(
            metadata.vehicle_columns,
            vec![Column::new("kenteken", "Kenteken")]
        );
    }

    #[test]
    fn fallback_columns_widen_to_vehicle_brandstof_and_status_header() {
        // 98 vehicle columns + 1 joined Brandstof column + 1 export_status.
        assert_eq!(fallback_vehicle_columns().len(), 98);
        assert_eq!(fallback_fuel_columns().len(), 36);

        let widener =
            crate::widen::RowWidener::new(fallback_vehicle_columns(), fallback_fuel_columns());
        let header = widener.header();
        assert_eq!(header.len(), 98 + 1 + 1);
        // Headers carry RDW's display names, not its fieldName keys.
        assert_eq!(header[0], "Kenteken");
        assert!(
            header.contains(&"Gemiddelde Lading Waarde".to_string()),
            "the fallback list must carry display names, not gem_lading_wrde"
        );
        assert_eq!(header[header.len() - 2], "Brandstof");
        assert!(
            !header.iter().any(|h| h.starts_with("Brandstof 1 - ")
                || h.starts_with("Brandstof 2 - ")
                || h.starts_with("Brandstof 3 - ")),
            "the old per-slot fuel columns must not appear"
        );
        assert_eq!(header.last().unwrap(), "Export status");
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
