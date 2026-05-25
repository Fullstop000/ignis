---
name: commit-style
description: Use when writing git commit messages — enforces Conventional Commits and a tidy subject line.
---
# Commit Style

Write commits as Conventional Commits:

- `type(scope): summary` — types: feat, fix, refactor, docs, test, chore.
- Subject ≤ 72 chars, imperative mood ("add", not "added").
- Body explains *why*, not *what*, wrapped at 72 cols. Omit for trivial changes.
- One logical change per commit.
