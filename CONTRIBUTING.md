# Contributing to ecsctl

Thank you for contributing to ecsctl. Keep pull requests focused on one logical
change and explain user-visible behavior, operational constraints, and recovery
procedures.

## Review Contract

Every pull request must include the exact `## Review Contract` structure in the
[Review Contract policy](docs/review-contract.md):

- Goal
- Non-goals
- Accepted Residual Risks
- Acceptance Criteria
- Follow-ups

Each subsection must contain meaningful content. For a small or
documentation-only change, `None` or `Not applicable` must include a brief
reason. The author proposes the contract, reviewers challenge it during the
first full review, and a maintainer/owner freezes it. Authors cannot
unilaterally accept correctness, security, operational, or data-loss risks.

After the freeze, reviews are incremental: unresolved findings, new changes,
regressions, and compliance with the frozen Acceptance Criteria. Broader
hardening is a non-blocking Follow-up unless concrete evidence passes the
policy's Late Blocker Gate. The default stopping sequence is full review and
freeze, fix verification, then a final regression check. Contract revisions or
additional rounds require an explicit maintainer/owner decision.

The `Review Contract` workflow validates required headings, order, and
non-placeholder content. It does not judge whether the contract is safe or
sufficient. A maintainer may document an exceptional case and apply the
`review-contract-exempt` label.

## Development

Before opening a PR, run:

```bash
cargo fmt -- --check
cargo clippy -- -D warnings
cargo test
```

Add focused tests for behavior changes and update user-facing documentation for
new flags, changed defaults, limitations, or recovery steps.

## Pull Requests

- Start from the repository's default branch and keep commits scoped.
- Use a Conventional Commit-style title such as `feat(restart): ...`,
  `fix(apply): ...`, or `docs(review): ...`.
- Complete every section of the PR template.
- Link a related issue or discussion when one exists.
- Provide the commands and results used for validation.
- Do not mix unrelated refactoring or hardening into the same PR; record it in
  Follow-ups instead.
