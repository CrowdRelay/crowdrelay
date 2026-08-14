fn snapshot_csv(snapshot: &Value) -> String {
    let mut output = String::from(
        "\u{feff}Rodzaj;Data wydarzenia;Wydarzenie;Typ biletu;Ilość;Cena jednostkowa brutto;Netto;VAT;Brutto;Stawka VAT;Waluta\r\n",
    );
    if let Some(sales) = snapshot.get("sales").and_then(Value::as_array) {
        for line in sales {
            csv_row(&mut output, "Sprzedaż", line, true);
        }
    }
    if let Some(adjustments) = snapshot.get("adjustments").and_then(Value::as_array) {
        for line in adjustments {
            csv_row(&mut output, "Zwrot", line, false);
        }
    }
    output
}

fn csv_row(output: &mut String, kind: &str, line: &Value, sale: bool) {
    let date = line
        .get("event_starts_at")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let event = line
        .get("event_title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let ticket_type = if sale {
        line.get("ticket_type_name")
            .and_then(Value::as_str)
            .unwrap_or_default()
    } else {
        "Korekta / zwrot"
    };
    let quantity = if sale {
        line.get("quantity")
            .and_then(Value::as_i64)
            .map(|v| v.to_string())
            .unwrap_or_default()
    } else {
        String::new()
    };
    let unit = if sale {
        line.get("unit_gross_minor")
            .and_then(Value::as_i64)
            .map(format_minor)
            .unwrap_or_default()
    } else {
        String::new()
    };
    let net = line
        .get("amount_net_minor")
        .and_then(Value::as_i64)
        .map(format_minor)
        .unwrap_or_default();
    let vat = line
        .get("amount_vat_minor")
        .and_then(Value::as_i64)
        .map(format_minor)
        .unwrap_or_default();
    let gross = line
        .get("amount_gross_minor")
        .and_then(Value::as_i64)
        .map(format_minor)
        .unwrap_or_default();
    let rate = line
        .get("vat_rate_basis_points")
        .and_then(Value::as_i64)
        .map(|v| format!("{:.2}%", v as f64 / 100.0))
        .unwrap_or_default();
    let currency = line
        .get("currency")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let values = [
        kind.to_owned(),
        date.to_owned(),
        event.to_owned(),
        ticket_type.to_owned(),
        quantity,
        unit,
        net,
        vat,
        gross,
        rate,
        currency.to_owned(),
    ];
    output.push_str(
        &values
            .iter()
            .map(|value| csv_escape(value))
            .collect::<Vec<_>>()
            .join(";"),
    );
    output.push_str("\r\n");
}

fn format_minor(value: i64) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let absolute = value.unsigned_abs();
    format!("{sign}{},{:02}", absolute / 100, absolute % 100)
}
fn csv_escape(value: &str) -> String {
    if value
        .chars()
        .any(|character| matches!(character, ';' | '"' | '\r' | '\n'))
    {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}
fn sanitize_filename(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            out.push(character);
        } else {
            out.push('-');
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').to_owned()
}
fn is_unique_violation(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|value| value.code())
        .is_some_and(|code| code.as_ref() == "23505")
}
async fn configure_transaction(
    transaction: &mut Transaction<'_, Postgres>,
    state: &crate::AppState,
) -> Result<(), AccountingError> {
    sqlx::query(
        "SELECT set_config('statement_timeout',$1,true),set_config('lock_timeout',$2,true)",
    )
    .bind(format!(
        "{}ms",
        state.ticketing.operation_timeout().as_millis()
    ))
    .bind(format!("{}ms", state.ticketing.lock_timeout().as_millis()))
    .execute(&mut **transaction)
    .await
    .map_err(|_| AccountingError::Unavailable)?;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum AccountingError {
    NotFound,
    Conflict,
    Unavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accounting_period_parses_calendar_boundaries() {
        let december = AccountingPeriod::parse("2026-12", "pln").expect("valid period");
        assert_eq!(december.start.to_string(), "2026-12-01");
        assert_eq!(december.end.to_string(), "2026-12-31");
        assert_eq!(december.next_start.to_string(), "2027-01-01");
        assert_eq!(december.currency(), "PLN");

        assert!(AccountingPeriod::parse("2026-13", "PLN").is_none());
        assert!(AccountingPeriod::parse("2026-07", "PL12").is_none());
    }

    #[test]
    fn csv_values_are_safe_for_semicolon_imports() {
        assert_eq!(csv_escape("Virya; Wrocław"), "\"Virya; Wrocław\"");
        assert_eq!(
            csv_escape("A \"quoted\" title"),
            "\"A \"\"quoted\"\" title\""
        );
        assert_eq!(format_minor(-12_345), "-123,45");
    }

    #[test]
    fn accounting_filename_is_portable() {
        assert_eq!(
            sanitize_filename("WEW/BILETY/07/2026"),
            "WEW-BILETY-07-2026"
        );
    }
}
