# Skills

A **skill** is a reusable `SKILL.md` instruction set that ignis discovers from
disk, advertises to the model in an `<available_skills>` system-prompt block,
and loads on demand via the built-in `skill` tool or the `/<skill-name>` slash
command. The format is shared with Claude Code, Codex, OpenCode, and Kimi —
the same skill directory works across all of them.

## Discovery

At startup ignis scans four roots, later roots overriding earlier ones on a
name collision (**project beats global; `.ignis` beats `.agents`**):

1. `~/.agents/skills/`
2. `~/.ignis/skills/`
3. `./.agents/skills/`
4. `./.ignis/skills/`

Each immediate subdirectory that contains a `SKILL.md` becomes a skill — the
directory name is *not* used; the skill's identity is the `name:` field in its
frontmatter.

## `SKILL.md` format

```markdown
---
name: react-cleanup
description: Refactor the changed React files to remove dead state and props.
---

Step 1. Run `git diff --name-only origin/master...HEAD` …
Step 2. …
```

**Frontmatter** (`---`-fenced YAML at the top):

| Key | Required | Notes |
|---|---|---|
| `name` | yes | Must start with an ASCII letter or digit, then `a-zA-Z0-9_-`. Becomes both the slash command and the tool argument. |
| `description` | no | One line shown in `/skills`, the autocomplete menu, and the catalog the model sees. |

YAML block scalars are supported (`description: \|` followed by an indented
block collapses to one line). A leading UTF-8 BOM is tolerated.

**Body** (everything after the closing `---`): free-form Markdown, capped at
**50,000 characters** (~12k tokens). Larger files are skipped with a warning
so a runaway skill can never blow the context window.

**Reserved names:** a skill cannot shadow these built-in commands —
`resume`, `clear`, `compact`, `copy`, `model`, `skills`. Skills using one of
these names are skipped at load.

## Bundled resources

Anything you drop alongside `SKILL.md` in the same directory (scripts,
fixtures, templates, sub-directories) is treated as bundled. The file list is
**not** included in the always-on catalog, but the first 10 entries are
appended to the prompt the moment the skill is loaded, with a hint to
`read_file` them only when the instructions reference them:

```
my-skill/
├── SKILL.md
├── prompt.txt
└── scripts/
    └── analyze.sh
```

Pure-instruction skills (no siblings) skip the directory hint entirely, so
they add no extra noise.

## Loading a skill

There are two ways the model can pick up a skill body:

1. **Tool call** — the model calls the built-in `skill` tool with the skill's
   `name`. Use this when the model decides on its own that a stored procedure
   applies.
2. **Force-load** — the user types `/<skill-name> [optional prompt]` in the
   TUI. Ignis injects the skill body as already-loaded instructions and tells
   the model *not* to re-call the `skill` tool. Anything after the command is
   appended as the user prompt.

In both cases the bundled-files hint (if any) is appended automatically.

## Managing skills at runtime

Open the **`/skills`** picker in the TUI to toggle individual skills on or
off:

- **Disabled** skills are hidden from the model's catalog and from
  autocomplete; the `skill` tool refuses to load them.
- The disabled set is persisted per project in `~/.ignis/state.json`, so
  toggling once is sticky across runs.

## Authoring tips

- **Lead with intent, end with checks.** The catalog only shows the
  `description:` line; if it doesn't say what the skill *does*, the model
  won't pick the right one.
- **Keep the body terse.** Long preludes burn tokens on every load; a 200-line
  procedure is usually two skills.
- **Co-locate scripts.** Bundling tools next to `SKILL.md` is cheaper than
  writing absolute paths into the instructions — the path hint is injected
  for you.
- **Use `.agents/skills/` for cross-tool sharing.** Save the same directory to
  Claude Code, Codex, OpenCode, Kimi, and ignis without duplication.
