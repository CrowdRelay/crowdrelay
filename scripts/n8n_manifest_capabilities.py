"""Versioned template capabilities attached to an existing verified event route."""
WELCOME_V2_CAPABILITY = "fan.lifecycle.welcome.v2"


def row_capabilities(row: dict[str, str]) -> set[str]:
    extras = {cap.strip() for cap in (row.get("template_capabilities") or "").split(",") if cap.strip()}
    if extras - {WELCOME_V2_CAPABILITY}:
        raise ValueError("unknown template capability")
    if extras and (row.get("capability") != "fan.lifecycle.message" or row.get("event_type") != "crowdrelay.fan_lifecycle.message_requested"):
        raise ValueError("welcome template capability requires the lifecycle route")
    return {row["capability"]} | extras
