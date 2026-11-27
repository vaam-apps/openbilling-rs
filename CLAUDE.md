# Agent guide — openbilling-rs

`AGENTS.md` is a symlink to this file. Edit `CLAUDE.md` only.

## What this is

An open-source usage billing service in Rust on PostgreSQL alone: intake of usage events, rating against a price book,
and a double-entry ledger. **Status: design**; [ADR 0001](docs/adr/0001-postgres-is-the-queue.md) is the design.

## Rules

1. **PostgreSQL is the only infrastructure.** No broker, no cache in the write path. A notification wakes a worker; the
   table is the truth.
2. **Every write is once-only.** An event, a credit and a debit each have a caller-given id; a replay changes nothing.
3. **Money is integers** in the currency's minor unit, never a float. Time is UTC.
4. **Ports and adapters.** Storage and identity are traits with a conformance testkit; binaries only compose them.
5. **Decisions are ADRs** in `docs/adr/NNNN-kebab-title.md`, next number, with a row in `docs/adr/README.md`. Change a
   past decision with a dated *Amended* note, never silently.
6. **Mark facts** about third parties *verified* (date and source) or *unverified*.
7. **This repository is public.** No customer, deployment, price list or market of any operator goes in it.

## Skills

Skills live in `.agents/skills/` (symlinked into `.claude/skills/`, `.goose/skills/` and `.kiro/skills/`), installed
with the skills CLI and pinned in `skills-lock.json`. Update them with `npx skills update -p -y`, never by hand-editing
their files.

| When you are… | Use |
|---|---|
| Writing a schema, a server, procedures, migrations or clients | `cratestack` first; it routes to the `cratestack-*` skills |
| Authentication, tokens, the identity provider | `authkestra-quickstart`, then `authkestra-features`, `authkestra-op`, `authkestra-store` |
| Money, ledgers, idempotency | `domain-fintech`, `cratestack-data-integrity` |
| A decision | `documentation-and-adrs`, `doubt-driven-development` |
| Rust | `rust-router`, `rust-skills`, `coding-guidelines` |
| Implementing | `spec-driven-development`, `incremental-implementation`, `test-driven-development` |
| Images and compose | `docker-build-strategies`, `docker-compose-patterns` |

## Commits

Conventional Commits: `<type>[(scope)][!]: <description>`.
