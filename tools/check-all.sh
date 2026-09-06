#!/bin/bash
# check-all.sh — the full house gate suite in one command.
#
# This is now a THIN SHIM over the manifest runner: the artifact graph is
# declared in build.toml and the gates run through tools/glyph.py, which
# preserves this script's output contract (exit 0 only when every gate is
# green; final line "CHECK-ALL: ALL GATES GREEN").
#
# What changed on 2026-09-06, and why: the battery's rebuild-and-compare
# gates were three hand-written variants (generators / fixture corpus / pixel
# A/B), each with its own restore logic and coverage counting — a fix to one
# reached none of the others, and the fixture gate ls-derived its expected
# count from the tree it was checking. Those mechanisms now live once in
# tools/glyph.py, driven by build.toml: counts declared, fixtures rebuilt in
# a scratch COPY (the committed corpus is never deleted mid-check), and the
# four baseline PNGs declared golden — verified, with NO build path, because
# re-baselining is a human act.
#
# Gate names (not numbers — the positions were renumbered twice already):
#   products-current · committed-artifacts · vendor-hashes · engine-suites ·
#   cargo-build · cargo-clippy · cargo-test · engine-check · pick-oracle ·
#   pixel-ab · repo-verify · reference-port
# What each compares, what makes it red, and what it CANNOT SEE: root
# AGENTS.md, "Verification". One gate alone: python3 tools/glyph.py gate <name>.
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 -u tools/glyph.py check "$@"
