# Security policy

## Supported versions

Security fixes are applied to the current `main` branch and the latest tagged release.

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Contact the repository owner privately through the email published on the GitHub profile and include:

- the affected endpoint or component;
- a minimal reproduction;
- expected and observed behavior;
- possible impact;
- whether any real personal data or secrets were involved.

Do not test against production accounts, attempt persistence, exfiltrate data, or publish one-time tokens. The project owner will acknowledge a complete report, coordinate remediation, and credit the reporter when appropriate.

## High-impact areas

Authentication, cross-workspace access, consent, referrals, coupon use, webhook replay, claim links, QR signatures, pass redemption, logs and backups are security-sensitive areas and should receive regression coverage when changed.
