# Security Policy

Flavium is a security tool; we take reports seriously and appreciate them.

## Reporting a vulnerability

**Do not open a public issue for security reports.**

Preferred: use GitHub's private vulnerability reporting on this repository
("Report a vulnerability" under the Security tab).

Alternatively, email **security@flavium.ai** with a description, reproduction
steps, and the version/commit affected.

You will receive an acknowledgment within 72 hours. We ask for reasonable
time to ship a fix before public disclosure and will credit reporters in the
release notes unless you prefer otherwise.

## Scope

Pre-v0.1 there is no supported release; reports against `main` are still
welcome. Note the documented enforcement boundaries in [DESIGN.md](DESIGN.md)
§7 — behavior explicitly listed there as out of scope for a given version
(e.g., proxy bypass by a malicious local process before v0.2) is a known
limitation rather than a vulnerability, though reports that sharpen those
boundaries are valuable.
