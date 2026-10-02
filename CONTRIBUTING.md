# Contributing to IRIS

Thank you for your interest in IRIS. This project is in **pre-release development**:
the public roadmap lives in the [README](README.md#project-status), and the
implementation plan drives what gets built and when.

## Before you start

- Search [existing issues](../../issues) before opening a new one.
- Use the provided issue templates — they exist so reports arrive with the
  context needed to act on them.
- For large or structural changes, **open an issue first** describing the
  proposal. IRIS is following a staged implementation plan; unsolicited PRs that
  jump the plan are unlikely to be merged early on.

## Reporting bugs

Use the **Bug report** template and include:

1. What you expected and what actually happened.
2. Exact steps to reproduce, including environment (OS, terminal, install method).
3. Any relevant logs or transcript excerpts — **redact secrets first**. Never
   paste API keys, tokens, `.env` contents, or private repository code into a
   public issue.

## Suggesting features

Use the **Feature request** template. Strong proposals describe:

- The problem, not just the solution.
- How existing tools fail at this today (links to upstream issues welcome).
- Whether the request fits IRIS's stated principles: cache-first, local-first
  privacy, replayable/open session format.

## Code contributions

Code contribution guidelines will be published once the workspace skeleton is in
place. Planned standards that apply from the first commit:

- **Test-driven development.** Red → green → refactor. A change is not done
  until previously failing tests pass.
- **Coverage.** ≥ 80% on the core crate; security-critical code (path
  confinement, redaction, permission gate) targets 100%.
- **Immutability.** Prefer builder/`with_*` patterns over mutating shared state.
- **No secrets in the tree.** API keys come from environment variables or the
  system keyring — never from a config file, never hard-coded.
- **Conventional commits.** `feat:`, `fix:`, `refactor:`, `docs:`, `test:`,
  `chore:`, `perf:`, `ci:`.

## Pull request process

1. Keep PRs focused and small; one logical change per PR.
2. Ensure all checks pass (CI details will be documented when workflows land).
3. Update documentation and the changelog when behavior changes.
4. By contributing, you agree that your contributions are licensed under the
   [Apache License 2.0](LICENSE).

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
By participating, you agree to uphold its standards.
