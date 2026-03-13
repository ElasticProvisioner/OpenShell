---
name: generate-cli-reference
description: Regenerate the CLI reference markdown from source definitions and deploy it to documentation targets. Use when CLI commands, flags, or descriptions change and the reference docs need updating. Trigger keywords - generate cli reference, regenerate cli docs, update cli reference, cli markdown, cli-reference, docs cli-reference.
---

# Generate CLI Reference

Regenerate the auto-generated CLI reference markdown and deploy it to all documentation targets.

## When to Use

Run this after any change to CLI commands, flags, or doc comments in `crates/navigator-cli/src/main.rs`. The generated reference is derived from clap's command tree, so source doc comments are the single source of truth.

## Prerequisites

- Rust toolchain available (`cargo` on PATH, or use `mise exec --`)
- The project compiles successfully

## Quick Start

Regenerate and deploy in one command:

```bash
mise run docs:cli-reference
```

This builds the CLI binary, runs the hidden `docs cli-reference` subcommand, and copies the output to both target locations.

### Check only (no changes)

Verify the committed docs are up to date without modifying files:

```bash
mise run docs:cli-reference:check
```

This check also runs automatically as part of `mise run pre-commit`.

### Manual steps (without mise)

If you need to run the steps individually:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo run --bin openshell -- docs cli-reference > /tmp/cli-reference-generated.md
cp /tmp/cli-reference-generated.md .agents/skills/openshell-cli/cli-reference-generated.md
cp /tmp/cli-reference-generated.md docs/reference/cli-generated.md
```

Note: `.claude/skills/` is a symlink to `.agents/skills/`, so the first copy covers both.

## Target Files

| File | Purpose |
|------|---------|
| `.agents/skills/openshell-cli/cli-reference-generated.md` | Agent skill reference (also serves `.claude/skills/` via symlink) |
| `docs/reference/cli-generated.md` | User-facing documentation site |

## Key Source Files

| File | Purpose |
|------|---------|
| `crates/navigator-cli/src/main.rs` | CLI definitions (commands, flags, doc comments) |
| `crates/navigator-cli/src/cli_reference.rs` | Markdown generator logic |

## Automation

| Trigger | What happens |
|---------|-------------|
| `mise run docs:cli-reference` | Regenerates and deploys to both targets. |
| `mise run docs:cli-reference:check` | Fails if committed docs are stale. |
| `mise run pre-commit` | Runs the staleness check alongside CI. |

## Notes

- The generator is a hidden subcommand (`openshell docs cli-reference`) that introspects clap's command tree at runtime.
- Clap strips trailing periods from single-line doc comments; the generator restores them via `ensure_period()`.
- The generated files contain a `<!-- Do not edit manually -->` header. Always regenerate rather than hand-editing.
