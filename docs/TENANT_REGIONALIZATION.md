# Tenant regional runtime

CrowdRelay validates tenant regional config once at startup and keeps it in AppState; no request-time profile DB lookup is added. Public tenant config exposes effective values and provenance. Non-Virya tenants fail startup when regional fields are missing. Virya keeps historic presentation defaults but missing data region remains explicitly unclassified. Fan quiet hours use the same tenant timezone in API and worker. Browser/IP locale never decides tenant currency, timezone or data residency.
