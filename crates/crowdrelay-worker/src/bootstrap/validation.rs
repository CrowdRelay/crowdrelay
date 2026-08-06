fn validate_database_timeouts(database: &DatabaseConfig) -> Result<(), BootstrapError> {
    if database.operation_timeout.is_zero()
        || database.lock_timeout.is_zero()
        || database.lock_timeout > database.operation_timeout
    {
        return Err(BootstrapError::InvalidDatabaseTimeouts);
    }
    duration_milliseconds(database.operation_timeout)?;
    duration_milliseconds(database.lock_timeout)?;
    Ok(())
}

fn duration_milliseconds(duration: Duration) -> Result<u64, BootstrapError> {
    let milliseconds =
        u64::try_from(duration.as_millis()).map_err(|_| BootstrapError::InvalidDatabaseTimeouts)?;
    if milliseconds == 0 {
        return Err(BootstrapError::InvalidDatabaseTimeouts);
    }
    Ok(milliseconds)
}

fn ensure_count(
    collection: &'static str,
    count: usize,
    max: usize,
) -> Result<(), BootstrapSpecError> {
    if count > max {
        return Err(BootstrapSpecError::TooMany { collection, max });
    }
    Ok(())
}

fn validate_name(value: String, field: &'static str) -> Result<String, BootstrapSpecError> {
    validate_text(value, field, MAX_NAME_BYTES)
}

fn validate_optional_text(
    value: Option<String>,
    field: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, BootstrapSpecError> {
    value
        .map(|value| validate_text(value, field, max_bytes))
        .transpose()
}

fn validate_text(
    value: String,
    field: &'static str,
    max_bytes: usize,
) -> Result<String, BootstrapSpecError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(invalid_field(field));
    }
    Ok(value)
}

fn validate_coordinates(
    latitude: Option<f64>,
    longitude: Option<f64>,
) -> Result<(Option<f64>, Option<f64>), BootstrapSpecError> {
    match (latitude, longitude) {
        (None, None) => Ok((None, None)),
        (Some(latitude), Some(longitude))
            if latitude.is_finite()
                && longitude.is_finite()
                && (-90.0..=90.0).contains(&latitude)
                && (-180.0..=180.0).contains(&longitude) =>
        {
            Ok((Some(latitude), Some(longitude)))
        }
        _ => Err(invalid_field("cities[].lat/lng")),
    }
}

fn parse_rfc3339(value: &str, field: &'static str) -> Result<OffsetDateTime, BootstrapSpecError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|_| invalid_field(field))
}

fn parse_optional_rfc3339(
    value: Option<&str>,
    field: &'static str,
) -> Result<Option<OffsetDateTime>, BootstrapSpecError> {
    value.map(|value| parse_rfc3339(value, field)).transpose()
}

fn parse_optional_url(
    value: Option<String>,
    production: bool,
    field: &'static str,
) -> Result<Option<DestinationUrl>, BootstrapSpecError> {
    value
        .map(|value| {
            let url = DestinationUrl::parse(value).map_err(|_| invalid_field(field))?;
            ensure_environment_url(&url, production, field)?;
            Ok(url)
        })
        .transpose()
}

fn ensure_environment_url(
    url: &DestinationUrl,
    production: bool,
    field: &'static str,
) -> Result<(), BootstrapSpecError> {
    if production && !url.as_str().starts_with("https://") {
        return Err(BootstrapSpecError::HttpsRequired { field });
    }
    Ok(())
}

fn valid_secret_reference(reference: &str) -> bool {
    (1..=MAX_SECRET_REFERENCE_BYTES).contains(&reference.len())
        && reference.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

const fn invalid_field(field: &'static str) -> BootstrapSpecError {
    BootstrapSpecError::InvalidField { field }
}

const fn duplicate(kind: &'static str) -> BootstrapSpecError {
    BootstrapSpecError::Duplicate { kind }
}
