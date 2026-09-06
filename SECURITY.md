# Security Policy

## Supported versions

During the initial `0.1` release series, security fixes target the latest
published `0.1.x` version. Older patch releases and unreleased development
snapshots are not supported.

| Version | Supported |
|---|---|
| Latest `0.1.x` | Yes |
| Earlier versions | No |

## Reporting a vulnerability

Please report suspected vulnerabilities privately. When GitHub private
vulnerability reporting is available, use the repository's
[private vulnerability report](https://github.com/zepdb/zeppelin-embed/security/advisories/new)
form.

If that form is unavailable, open a
[GitHub issue](https://github.com/zepdb/zeppelin-embed/issues) that asks the
maintainers to arrange a private reporting channel. Do not include the
vulnerability, reproduction steps, proof of concept, or other sensitive details
in that issue.

Include the following information in the private report when available:

- The affected version, commit, platform, and configuration.
- The affected component and source locations.
- Steps needed to reproduce the issue.
- A proof of concept or minimal reproducer.
- The expected security impact and plausible attack conditions.
- Any known mitigations or suggested fixes.

## Coordinated disclosure

Please keep vulnerability details private while the report is investigated and
a fix and release are coordinated. The maintainers will use the private report
to discuss validation, remediation, credit, and publication with the reporter.
Public disclosure should follow the security release or another disclosure date
agreed through that private discussion.

## Scope

This policy covers security-impacting defects in released Zeppelin Embed code
and project-produced artifacts, including the Rust core, persistent storage
formats, C ABI, and official language bindings. Reports about dependencies are
in scope when the dependency behavior is reachable through Zeppelin Embed.

Unsupported versions, unmodified third-party software, benchmark tooling, test
fixtures, and embedding models that Zeppelin Embed does not distribute are
outside this policy's scope.
