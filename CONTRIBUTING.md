# Contributing to CrowdRelay

CrowdRelay accepts focused changes that preserve the reliability and privacy guarantees of the existing flows.

## Development workflow

1. Create a short-lived branch.
2. Keep changes scoped to one vertical slice.
3. Add or update Rustdoc for public APIs.
4. Add unit tests for domain/application rules and PostgreSQL integration tests for transactional behavior.
5. Keep `openapi/openapi.yaml`, the browser client, migrations, and examples synchronized.
6. Run `just check` and `node --disable-warning=ExperimentalWarning --experimental-strip-types scripts/validate-contract-assets.ts`.
7. Explain security, migration, and operational implications in the pull request.

## Engineering rules

- no secrets, personal data, or production URLs in source control;
- no synchronous dependency on n8n or email delivery in public request paths;
- no unbounded channels, retries, request bodies, or external I/O;
- durable writes must be transactional and idempotent where retries are plausible;
- tenant-owned rows must remain workspace-scoped;
- do not add infrastructure until measurements justify it;
- do not claim benchmark results without publishing the environment and method.

Report security problems privately as described in `SECURITY.md` rather than opening a public issue.
