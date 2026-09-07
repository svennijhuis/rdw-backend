//! Pure query-parameter and API-key parsing logic, kept independent of
//! Axum's request types so it can be unit tested directly.

/// The only brands this service serves. Matched case-insensitively on
/// input, but exactly against the upstream `merk` field once uppercased
/// (so a variant like `TOYOTA-CHINOOK` is excluded).
pub const ALLOWED_BRANDS: [&str; 3] = ["LEXUS", "TOYOTA", "SUZUKI"];

#[derive(Debug, PartialEq, Eq)]
pub struct QueryError(pub String);

/// Parse the `brands` query parameter: comma-separated, case-insensitive,
/// validated against the allowlist. Omitted or empty means all three.
pub fn parse_brands(raw: Option<&str>) -> Result<Vec<String>, QueryError> {
    match raw {
        None => Ok(ALLOWED_BRANDS.iter().map(|s| s.to_string()).collect()),
        Some(value) if value.trim().is_empty() => {
            Ok(ALLOWED_BRANDS.iter().map(|s| s.to_string()).collect())
        }
        Some(value) => {
            let mut out = Vec::new();
            for part in value.split(',') {
                let trimmed = part.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let upper = trimmed.to_ascii_uppercase();
                if !ALLOWED_BRANDS.contains(&upper.as_str()) {
                    return Err(QueryError(format!("unknown brand: {trimmed}")));
                }
                if !out.contains(&upper) {
                    out.push(upper);
                }
            }
            if out.is_empty() {
                return Err(QueryError("brands parameter must not be empty".to_string()));
            }
            Ok(out)
        }
    }
}

/// Parse the `limit` query parameter as a non-negative integer. Omitted
/// means unlimited; `0` is a valid, distinct value (export zero rows).
pub fn parse_limit(raw: Option<&str>) -> Result<Option<u64>, QueryError> {
    match raw {
        None => Ok(None),
        Some(value) if value.trim().is_empty() => Ok(None),
        Some(value) => value
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| QueryError(format!("invalid limit: {value}"))),
    }
}

/// Extract the client API key. The `X-Api-Key` header takes precedence
/// over the `?api_key=` query parameter; an empty string in either place
/// is treated as absent.
pub fn extract_api_key(header_value: Option<&str>, query_value: Option<&str>) -> Option<String> {
    header_value
        .filter(|v| !v.is_empty())
        .or_else(|| query_value.filter(|v| !v.is_empty()))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_brands_omitted_returns_all_three() {
        let brands = parse_brands(None).unwrap();
        assert_eq!(brands, vec!["LEXUS", "TOYOTA", "SUZUKI"]);
    }

    #[test]
    fn happy_path_brands_case_insensitive_and_deduped() {
        let brands = parse_brands(Some("toyota,LEXUS,Toyota")).unwrap();
        assert_eq!(brands, vec!["TOYOTA", "LEXUS"]);
    }

    #[test]
    fn edge_limit_zero_is_a_valid_distinct_value() {
        assert_eq!(parse_limit(Some("0")).unwrap(), Some(0));
    }

    #[test]
    fn edge_limit_omitted_means_unlimited() {
        assert_eq!(parse_limit(None).unwrap(), None);
    }

    #[test]
    fn failure_unknown_brand_is_rejected() {
        let err = parse_brands(Some("ford")).unwrap_err();
        assert!(err.0.contains("ford"));
    }

    #[test]
    fn failure_brand_variant_not_exact_match_is_rejected() {
        let err = parse_brands(Some("toyota-chinook")).unwrap_err();
        assert!(err.0.contains("toyota-chinook"));
    }

    #[test]
    fn failure_malformed_limit_is_rejected() {
        assert!(parse_limit(Some("not-a-number")).is_err());
    }

    #[test]
    fn happy_path_header_api_key_used_when_present() {
        let key = extract_api_key(Some("header-key"), Some("query-key"));
        assert_eq!(key, Some("header-key".to_string()));
    }

    #[test]
    fn edge_header_takes_precedence_over_differing_query_value() {
        let key = extract_api_key(Some("h"), Some("q"));
        assert_eq!(key, Some("h".to_string()));
    }

    #[test]
    fn edge_query_used_when_header_absent() {
        let key = extract_api_key(None, Some("q"));
        assert_eq!(key, Some("q".to_string()));
    }

    #[test]
    fn failure_missing_key_in_both_places_is_none() {
        assert_eq!(extract_api_key(None, None), None);
    }

    #[test]
    fn failure_empty_string_key_is_treated_as_missing() {
        assert_eq!(extract_api_key(Some(""), Some("")), None);
    }
}
