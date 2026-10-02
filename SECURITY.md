# Security Policy

## Reporting a vulnerability

IRIS treats security as a first-class concern. The core of the product —
workspace path confinement, secret redaction, and the tool permission gate — is
held to a higher testing bar than the rest of the codebase.

**Please do not report security vulnerabilities through public GitHub issues.**

Instead, use GitHub's private vulnerability reporting:

1. Go to the repository's **Security** tab → **Report a vulnerability**.
2. Describe the issue, affected version (if any), and steps to reproduce.
3. Allow a reasonable time for assessment and a fix before public disclosure.

If private vulnerability reporting is unavailable, open a minimal issue titled
`SECURITY: <summary>` asking for a private channel, and include **no** exploit
details in the issue itself.

## What belongs here

- Path traversal or sandbox escape in workspace file access.
- Bypass of the tool permission gate.
- Failures in secret redaction (secrets written to disk unredacted).
- Any path by which an API key or credential could be persisted to a config
  file or transcript.
- Injection issues in shell execution or template handling.

## What does not belong here

- General bugs — use the bug report template.
- Model/provider behavior (prompt injection at the model layer should still be
  reported here if it has concrete impact on IRIS's safety guarantees).

## Handling commitments

- Reports are acknowledged as quickly as practicable.
- Confirmed issues are fixed before the next release that contains the change.
- Credit is given to reporters in the changelog unless anonymity is requested.

## Scope notes for users

- IRIS reads API keys **only** from environment variables or the system
  keyring. It will never ask you to place a key in a config file.
- Telemetry is off by default; the only expected outbound connections are to
  the model provider you configure yourself.
- Transcript files may still contain sensitive *code* content by nature — treat
  your local session store with the same care as your repository.
