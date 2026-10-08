---
title: Code contribution guide
description: The git workflow — feature branches, structured commits, DCO sign-off, pull requests, and the review process.
sidebar:
  label: Code contribution guide
  order: 3
---

The workflow mirrors a standard branch/fork-and-PR model. Start from an issue where possible.

## Branch

```bash
git checkout -b feature/your-feature-name
```

Use descriptive names: `feature/user-authentication`, `fix/memory-leak-in-router`, `docs/api-gateway-examples`, `refactor/entity-to-contract-conversions`. You may also fork the repository to your own account.

## Make your changes

Follow the coding standards and, when adding new code, always include unit tests. Gear directories under `gears/` must use **kebab-case** (validated in CI). For architecture rules and required checks, see [Architecture and quality gates](../architecture-and-quality-gates/); for gear-specific conventions, see [Add or change a gear](../add-or-change-a-gear/).

## Sign your commits (DCO)

This project uses the Developer Certificate of Origin (DCO) v1.1. Every commit must carry a `Signed-off-by` line:

```bash
git commit -s -m "your message"
# or enable it globally:
git config --global format.signoff true
```

## Commit message format

```text
<type>(<gear>): <description>
```

Accepted types include `feat`, `fix`, `tech`, `cleanup`, `refactor`, `test`, `docs`, `style`, `chore`, `perf`, `ci`, `build`, `revert`, `security`, `breaking`.

```text
feat(auth): add OAuth2 support for login
fix(ui): resolve button alignment issue on mobile
docs(readme): update installation instructions
```

Keep the title ≤ 50 chars, use imperative mood, make commits atomic, and for breaking changes use `feat!:`/`fix!:` or a `BREAKING CHANGE:` footer.

## Open a pull request

```bash
git push origin feature/your-feature-name
```

Include a clear title/description, linked issues, testing information, and any breaking changes. The repository provides a PR description template covering type of change, testing, documentation, and a self-review checklist (`cargo clippy`, `cargo fmt`, `cargo test`).

## Review process

1. **Automated checks** must pass (CI/CD).
2. **At least one maintainer approval** is required.
3. **All conversations resolved** before merge.
4. **Up to date with `main`.**

Merge strategy: squash-and-merge for feature branches, rebase-and-merge for simple fixes, merge commit for release branches.

## Local PR review with Constructor Studio

After cloud AI review bots complete, run a local Constructor Studio review to catch more issues before requesting human review:

```text
cf-gears-pr-review PR <number>
cf-gears-pr-status PR <number>
```

Results are written to `.prs/{ID}/`. See [docs/pr-review/README.md](https://github.com/constructorfabric/gears-rust/blob/main/docs/pr-review/README.md) for setup.

## See also

- [Spec-driven workflow (SDD + Studio)](../spec-driven-workflow/) — for larger features.
- [Release process](../release-process/) — versioning public contracts.
