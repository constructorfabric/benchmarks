#!/bin/bash
# CLLM-598 benchmark entry point. Every step is resumable: finished jobs and cached verdicts are skipped.
#
#   ./run.sh instructions        build extra_instructions from docs/toolkit-pr-review
#   ./run.sh glm                 glm-5.3-flash: single x5 runs, per-module x2 runs
#   ./run.sh deepseek            deepseek-v4.1-flash: single + per-module, 1 run
#   ./run.sh opus                Opus via Claude Code seats: single on all PRs, per-module on the pilots
#   ./run.sh judge               LLM steps of the analysis (Opus seats + glm on the router)
#   ./run.sh report              findings + every report from the caches, no LLM calls
#
# Long steps belong in a user unit so they outlive the terminal, e.g.
#   systemd-run --user --unit cllm598-glm --collect --same-dir ./run.sh glm
# Pause: systemctl --user stop <unit>; resume: start it again. Logs: out/logs/.
set -euo pipefail
cd "$(dirname "$0")"
[ -f .env ] && { set -a; . ./.env; set +a; }
export ROUTER_MAX_PARALLEL="${ROUTER_MAX_PARALLEL:-2}"

PY=(uv run --frozen python -I -m)
export PR_AGENT_PYTHON="$PWD/.venv/bin/python"
LOG=out/logs
mkdir -p "$LOG"

# Rust diff size ascending, so whatever finishes first is a complete slice of small PRs
ALL_PRS="78 73 76 77 75 80 81 84 87 86 85 74 83 82 79 89 91 88 90 100 99 98 72 101 102"
PILOTS="73 91 102"
FINDINGS=out/compare/findings
# out/pilot3 holds the pilots' deepseek runs of the first campaign; a fresh run writes everything to out/full
MATRICES="out/full out/opus"
if [ -d out/pilot3 ]; then MATRICES="out/full out/pilot3 out/opus"; fi

# reviewer pairs the judge compares: Opus and deepseek against every glm run, and all glm run pairs
PAIRS="opus:deepseek"
for i in 1 2 3 4 5; do
  PAIRS+=" opus:glm$i glm$i:deepseek"
  for j in $(seq $((i + 1)) 5); do PAIRS+=" glm$i:glm$j"; done
done

matrix() { "${PY[@]}" pr_bench.runner.run_matrix --prs $ALL_PRS --out out/full "$@"; }
analysis() { "${PY[@]}" "pr_bench.analysis.$1" "${@:2}"; }

need_router() { : "${LITELLM_KEY:?set LITELLM_KEY in .env}" "${LITELLM_ROUTER_V1:?set LITELLM_ROUTER_V1 in .env}"; }

case "${1:-}" in
  instructions)
    "${PY[@]}" pr_bench.runner.build_instructions ;;
  glm)
    need_router
    matrix --models glm-5.3-flash --variants single --runs 5 >"$LOG/glm-single.log" 2>&1
    matrix --models glm-5.3-flash --variants per-module --runs 2 >"$LOG/glm-per-module.log" 2>&1 ;;
  deepseek)
    need_router
    matrix --models deepseek-v4.1-flash --variants single per-module --runs 1 >"$LOG/deepseek.log" 2>&1 ;;
  opus)
    : "${CLAUDE_SEATS:=CC_TOKEN_2,CC_TOKEN_3,CC_TOKEN}"; export CLAUDE_SEATS
    "${PY[@]}" pr_bench.runner.run_matrix --prs $ALL_PRS --models claude-code/claude-opus-5-5 --variants single \
        --calls-per-job 1 --out out/opus >"$LOG/opus-single.log" 2>&1
    "${PY[@]}" pr_bench.runner.run_matrix --prs $PILOTS --models claude-code/claude-opus-5-5 --variants per-module \
        --calls-per-job 1 --out out/opus >"$LOG/opus-per-module.log" 2>&1 ;;
  judge)
    need_router
    analysis to_findings --matrix $MATRICES --out "$FINDINGS"
    analysis judge_findings --findings "$FINDINGS" --judge opus --pairs $PAIRS >"$LOG/judge-opus.log" 2>&1
    analysis judge_findings --findings "$FINDINGS" --judge glm --sample 300 >"$LOG/judge-glm.log" 2>&1
    analysis failed_tests >"$LOG/failed-tests.log" 2>&1
    analysis localize_failures >"$LOG/localize.log" 2>&1
    analysis match_defects --findings "$FINDINGS" >"$LOG/recall.log" 2>&1
    analysis sast_match >"$LOG/sast.log" 2>&1
    analysis cross_validate --findings "$FINDINGS" --validator opus >"$LOG/crossval-opus.log" 2>&1
    analysis cross_validate --findings "$FINDINGS" --validator glm --limit 75 >"$LOG/crossval-glm.log" 2>&1 ;;
  report)
    analysis to_findings --matrix $MATRICES --out "$FINDINGS"
    analysis oracle_floor --findings "$FINDINGS"
    analysis judge_findings --findings "$FINDINGS" --pairs $PAIRS --report
    analysis match_defects --findings "$FINDINGS" --report
    analysis sast_match --report
    analysis cross_validate --findings "$FINDINGS" --report ;;
  *)
    sed -n '2,13p' "$0" >&2; exit 2 ;;
esac
