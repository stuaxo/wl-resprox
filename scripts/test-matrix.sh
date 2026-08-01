#!/usr/bin/env bash
# Phase 10's automated matrix runner: loops scripts/self-test.sh (build
# harness -> spin container -> run L1 -> diagnose -> tear down) over
# every Phase 9 WM container, then surfaces pass/fail + per-WM logs as a
# markdown table -- see plan-test-harness.md's Phase 10 section.
#
# Run from the HOST.
#
# Usage: ./scripts/test-matrix.sh [wm...]
#   Defaults to every Phase 9 container (labwc sway kwin mutter) if none
#   are named explicitly.
#
# Writes results.md (gitignored -- host/timing-dependent, regenerated
# per run, not a source artifact) in the project root and prints its
# path. Exit code is 0 only if every WM passed.
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

WMS=("$@")
[[ ${#WMS[@]} -eq 0 ]] && WMS=(labwc sway kwin mutter)

RESULTS_FILE="$PROJECT_DIR/results.md"
LOG_DIR="$(mktemp -d)"

{
    echo "# Phase 9/10 test matrix results"
    echo ""
    echo "Run: $(date -Iseconds)"
    echo ""
    echo "| WM | Result | Log |"
    echo "|---|---|---|"
} > "$RESULTS_FILE"

OVERALL=0
for wm in "${WMS[@]}"; do
    log="$LOG_DIR/$wm.log"
    echo ""
    echo "############################################"
    echo "# $wm"
    echo "############################################"
    if "$SCRIPT_DIR/self-test.sh" --wm="$wm" 2>&1 | tee "$log"; then
        echo "| $wm | PASS | \`$log\` |" >> "$RESULTS_FILE"
    else
        echo "| $wm | **FAIL** | \`$log\` |" >> "$RESULTS_FILE"
        OVERALL=1
    fi
done

echo "" >> "$RESULTS_FILE"
if [[ "$OVERALL" -eq 0 ]]; then
    echo "All WMs passed." >> "$RESULTS_FILE"
else
    echo "One or more WMs failed -- see their log for the FAIL line and full output (setup-env.sh, test-crash.sh --l1, or diagnose.sh)." >> "$RESULTS_FILE"
fi

echo ""
echo "== Results =="
cat "$RESULTS_FILE"
echo ""
echo "Full logs kept at: $LOG_DIR"
echo "Results table written to: $RESULTS_FILE"

exit "$OVERALL"
