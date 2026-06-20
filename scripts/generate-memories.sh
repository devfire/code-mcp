#!/usr/bin/env bash
#
# generate-memories.sh — bootstrap a memory directory for code-mcp.
#
# code-mcp itself is a read-only, LLM-free tool server: it can serve a memory
# directory but cannot create one (no write channel, no model). This script
# runs a local coding agent *on the box where the files live* to produce the
# memory layout the `memories` tool expects (MEMORY.md index + per-area files).
#
# Run it once, before (or whenever you want to refresh) starting the server,
# then point `--memory-dir` at the output.
#
# Usage:
#   scripts/generate-memories.sh <project-dir> <memory-out-dir>
#
# Env:
#   AGENT   the agent command to pipe the prompt into. Default: "claude -p".
#           Anything that reads a prompt on stdin and has read+write access to
#           the local filesystem works (e.g. AGENT="codex exec").
#
# Examples:
#   scripts/generate-memories.sh ./my/repo ./memories
#   AGENT="codex exec" scripts/generate-memories.sh /srv/monorepo /srv/memories

set -euo pipefail

PROJECT="${1:?usage: generate-memories.sh <project-dir> <memory-out-dir>}"
OUT="${2:?usage: generate-memories.sh <project-dir> <memory-out-dir>}"
AGENT="${AGENT:-claude -p}"

if [[ ! -d "$PROJECT" ]]; then
  echo "error: project dir '$PROJECT' does not exist" >&2
  exit 1
fi
mkdir -p "$OUT"

# Resolve to absolute paths so the agent isn't confused by its own cwd.
PROJECT="$(cd "$PROJECT" && pwd)"
OUT="$(cd "$OUT" && pwd)"

echo "Generating memories for $PROJECT -> $OUT (agent: $AGENT)" >&2

read -r -d '' PROMPT <<EOF || true
You are bootstrapping a persistent "memory" directory for an MCP code-intelligence
server. Future LLM clients will connect to a large codebase with only grep/find/cat
tools. Your job is to give them a MENTAL MODEL so they navigate efficiently instead
of rediscovering structure from scratch on every session.

The codebase to analyze is at: $PROJECT
Write all output files into: $OUT

Explore the codebase with your file tools before writing anything. Do NOT guess —
verify file paths and module names.

Produce a set of Markdown files in the output directory:

1. MEMORY.md — the index, loaded first by every client. Contains:
   - A one-paragraph "what is this codebase" orientation.
   - A FUNCTIONAL AREA MAP: the 5-15 major areas (e.g. "HTTP gateway",
     "session management", "search engine", "auth"), each with: one-line
     responsibility, the entry-point file(s), and a pointer to its detail file.
   - A short "how the pieces talk" section: the main data/control flow across areas.
   - A list of the detail files below, with cat-able relative paths.

2. One file per functional area (e.g. area_search.md, area_sessions.md). Each:
   - What it does and why it exists.
   - Key files and the symbols/entry points worth knowing (with paths).
   - Cross-cutting concerns and non-obvious gotchas (invariants, footguns,
     "looks like X but actually Y").
   - Where its tests live.

Rules:
- Capture what is NON-OBVIOUS and EXPENSIVE to rediscover. Do not transcribe code
  or list every file — a client can grep. Favor the map over the territory.
- Be concrete: real paths, real module/function names, verified by reading files.
- Keep each file scannable. Terse > exhaustive.
- State uncertainty explicitly rather than inventing structure.
- End MEMORY.md by reminding the client this map is a starting point; confirm
  details with grep/cat before relying on them.
EOF

printf '%s\n' "$PROMPT" | $AGENT

echo "Done. Review the generated files in $OUT before serving them." >&2
