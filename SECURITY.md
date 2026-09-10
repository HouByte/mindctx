# Security

If you discover a vulnerability in mindctx, please report it privately so we can fix it
before public disclosure.

## Reporting

Email: `hougq.rd@gmail.com` (PGP key: not yet published — request the key in your first
mail and we'll send a fresh fingerprint).

Please include:

- A description of the vulnerability and its impact
- Reproduction steps or a proof-of-concept
- The affected commit/tag and platform

## What to expect

- Acknowledgement within 7 days
- A triage decision (accepted / won't fix / duplicate) within 30 days
- Coordinated disclosure: we aim to release a fix before any public write-up; the
  reporter agrees to a reasonable embargo (typically 90 days)

## Out of scope

- Bugs in third-party crates we depend on (report upstream)
- Theoretical issues without a concrete attack path
- Issues only reproducible on an unmaintained, pre-release version

## Supported versions

| Version | Supported |
|---------|-----------|
| latest stable (`latest` npm tag, `mindctx` crates.io) | yes |
| latest pre-release (`next` npm tag) | yes |
| older stable | best-effort |

## Disclosure policy

We follow [GitHub's coordinated disclosure guidance](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability).
A fix will be released as a patch version, the advisory will be published via the GitHub
Security Advisory API, and the npm + crates.io post-publish metadata will be updated to
reference it.
