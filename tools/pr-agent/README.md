# PR-Agent review benchmark (CLLM-598)

Runs [PR-Agent](https://github.com/The-PR-Agent/pr-agent) `/review` over the B9-minichat PRs
(#72–#102; #72 is the oracle, the reference solution that passes every hidden test) with
`glm-5.3-flash` and `deepseek-v4.1-flash` from the LiteLLM router and Claude Opus 5.5 through
Claude Code, using the gears-rust `toolkit-pr-review` rules from `docs/toolkit-pr-review/`
(identical to `constructorfabric/gears-rust@main`, checked 2026-10-23). Then compares the
reviews: with each other, with the oracle, with the hidden-test failures, with static analysis,
and by cross-review.

PR-Agent is a pinned dependency (`pr-agent[github]==0.48.0`). The benchmark only plugs into its
extension points: a git provider (Rust-only file filter) and an AI handler (call recording,
router slots, Claude Code backend).

## Layout

| Path | Role |
|---|---|
| `run.sh` | entry point: `instructions`, `glm`, `deepseek`, `opus`, `judge`, `report` |
| `pr_agent.toml` | PR-Agent settings; every deviation from the defaults is commented |
| `pr_bench/runner/build_instructions.py` | `extra_instructions` texts from `docs/toolkit-pr-review/` → `instructions/` |
| `pr_bench/runner/run_review.py` | one `/review` of the checked-out PR branch against `main` |
| `pr_bench/runner/run_matrix.py` | PRs × models × variants × runs, per-model concurrency, resumable |
| `pr_bench/runner/router_slots.py` | machine-wide cap on parallel router requests |
| `pr_bench/runner/claude_code_backend.py` | answers PR-Agent's calls through `claude -p` on rotating seats |
| `pr_bench/runner/seat_budget.py` | seat fill gate, vendored unchanged from spec-bench |
| `pr_bench/analysis/to_findings.py` | `findings.<model>.json` (flat list, one object per finding) + `summary.json` |
| `pr_bench/analysis/oracle_floor.py` | findings per KLOC on the oracle vs the agent PRs |
| `pr_bench/analysis/judge_findings.py` | do two reviewers report the same problem (line window + LLM judge) |
| `pr_bench/analysis/failed_tests.py`, `localize_failures.py` | hidden-test failures → root-cause defects |
| `pr_bench/analysis/match_defects.py` | recall: did a reviewer flag a test-confirmed defect |
| `pr_bench/analysis/sast_match.py` | code_quality_service issues vs review findings |
| `pr_bench/analysis/cross_validate.py` | validator models rate sampled findings valid / invalid |
| `pr_bench/analysis/findings.py`, `llm.py` | shared: findings loading and spans; judge backends, cached task loop |

Generated data goes to `out/` (gitignored): `out/worktrees/pr-<N>` (PR checkouts),
`out/full` and `out/opus` (reviews), `out/compare` (findings, judge and cross-review caches and
reports), `out/tests`, `out/sast`, `out/logs`.

## Setup

```bash
uv sync                               # .venv with pr-agent, litellm and this package
cp .env.example .env                  # then fill in the key and the seat tokens
./run.sh instructions                 # -> instructions/{per-module,single}/
```

`.env` is gitignored. `LITELLM_KEY` is the `cfs-pr-review` key (its label on the router's metrics
is where usage is read from). The router is reachable only over the NetBird VPN:
`http://100.100.123.225:8080/v1`.

## Running

```bash
systemd-run --user --unit cllm598-glm --collect --same-dir ./run.sh glm
./run.sh deepseek
./run.sh opus
./run.sh judge        # LLM steps of the analysis
./run.sh report       # every report from the caches, no LLM calls
```

Every step skips finished work (a job with `result.json`, a cached verdict), so an interrupted
step is resumed by running it again. Run long steps as user units: a closed terminal or a
reboot kills them. Output per review job, under `out/full/pr-<N>/<model>/run-<k>/<variant>/<module>/`:
`review.md` (PR-Agent's rendering), `result.json` (merged review, coverage, per-call stats),
`calls/NNN.{prompt,raw,parsed}.txt`, `stderr.log`.

**Incomplete reviews are not kept as finished.** PR-Agent drops a chunk it could not get an
answer for and still finishes the job; when every chunk fails it falls back to one pruned chunk
(`findings_count = 0`, `files_omitted_by_budget` nearly all files); an error in the review itself
is logged and swallowed. `run_review` writes such a job as `result.partial.json` and exits non-zero,
so the job shows as failed and the next run of the step redoes it. Results produced before this
check (the CLLM-598 campaign) were checked by hand: see `complete` in the results' `jobs.csv`.

The analysis inputs from outside this tool: `out/tests/pr-<N>/` (the spec-bench verifier output of
each PR's trial: `junit.xml`, `pytest-stdout.txt`, `reward.json`), the hidden suite's sources
(`SPEC_BENCH_SUITE`, a spec-bench checkout), and `out/sast/pr-<N>.json` (the `quality_static`
block of the spec-bench scorecards).

## What the reviewer gets

PR-Agent's own review prompt with the rules as `extra_instructions`, and the diff of the PR branch
against `main`, Rust files and manifests only. No PRD/DESIGN, no PR description, no test results,
no code outside the diff; it cannot open files.

- **single**: one pass, all six rule modules and the architecture prompt (~91 KB).
- **per-module**: seven passes per PR, one per rule module plus the architecture pass, mirroring
  the gears-rust skill (one agent holds one module).

Findings carry the check ID twice: `issue_header` is `RUST-ERR-001 - <title>` (no colon: an
unquoted `ID: title` is invalid YAML) and `issue_content` starts with `[RUST-ERR-001][HIGH] `.
`to_findings` takes the ID from either.

## Settings and why

- **Chunking.** PR-Agent caps every model at `max_model_tokens` (default 32000); PR #91 then got
  4 of 207 files into the prompt. `pr_agent.toml` sets 120k and turns on large-PR chunking; each
  chunk is one call, and the chunk reviews are merged.
- **Findings cap 30 per chunk.** With 15, deepseek hit the cap in 41% of calls. With no cap
  (`num_max_findings = 0`, which makes `run_review` drop the range from the prompt) deepseek
  degenerated: 631 findings on PR #73, 143 distinct, calls of 15–85 min.
- **Timeouts.** `ai_timeout = 1800`; the router cuts a call at 900 s anyway.
- **Rust-only.** `ignore.regex` is not implemented for PR-Agent's `local` platform, so
  `run_review` filters in `get_diff_files()` and records what it skipped.
- **glm `</think>` leak.** When the diff contains a literal `</think>` (mini-chat's
  `vllm_responses.rs` does), glm's answer starts mid-reasoning and the YAML parse fails. The final
  answer still comes last and starts with a top-level `review:` line, so the handler passes
  PR-Agent the text from the last such line (`reasoning_leaked` per call). On the pilot this
  recovered 9 of 16 parse failures and broke none. Turning thinking off is worse.

## Router capacity and concurrency

PR-Agent fires every chunk of a review at once. A 16-chunk PR times several jobs slowed the shared
nodes 4–16× for everyone. Three caps:

- `run_matrix --concurrency model=N`: jobs per model (default 2);
- `--calls-per-job` (`PR_AGENT_MAX_CONCURRENT_CALLS`): in-flight chunk calls per job (default 2);
- `router_slots.py`: every router call, from any process, takes one of `ROUTER_MAX_PARALLEL`
  machine-wide slots (flock on `out/.router_slots/slot*`, released when a process dies) and waits
  out a 429. Set it to the key's `max_parallel_requests`; past it the router answers 429 and
  PR-Agent drops the chunk.

## Claude Code backend

PR-Agent makes one call per chunk: system + user prompt in, YAML out. `claude_code_backend`
answers it through `claude -p`; `run_review` selects it for any model named
`claude-code/<model>`. The process never sees an Anthropic API key.

- **Seats.** Tokens from `claude setup-token` go into `.env`; `CLAUDE_SEATS` sets the order. A
  seat is used until its tighter subscription window (5h or 7d) is 95% full (`seat_budget.py`
  reads the rate-limit headers of an 8-token probe), then the next one, so the run stays off the
  overage credits the seats' people rely on. `python -m pr_bench.runner.claude_code_backend --seats`
  prints each seat's state.
- **bare** (default): no tools, no settings or memory: the same single-shot call the API models
  get. **agentic** (`--agentic`): Read/Grep/Glob in the PR worktree; a different tool.

## Analysis

- **Same problem or not** (`judge_findings`). For a pair of reviewers on one PR, each finding of
  the smaller side gets the other side's findings in the same file within 25 lines (at most 8)
  plus the code around them; the judge labels each `same`, `related` or `different`, and `same`
  edges are matched one-to-one per line threshold W = 0/3/10/25. Opus judges every pair, glm a
  random 300 for judge agreement (Cohen's κ).
- **Oracle floor** (`oracle_floor`). Findings per KLOC (added lines of Rust and Cargo.toml) on the
  oracle against the median agent PR, per severity and rule class.
- **Recall** (`failed_tests` → `localize_failures` → `match_defects`). Failed hidden tests are
  clustered per PR; agentic Opus locates each cluster's root cause and classifies it
  code_evident / spec_dependent / missing_feature; then each reviewer's findings near the cause
  are judged catches / partial / no.
- **Static analysis** (`sast_match`). code_quality_service issues on the changed Rust lines;
  complexity rules span the whole function; Opus judges each issue against nearby findings.
- **Cross-review** (`cross_validate`). A stratified sample per source (severity × rule family,
  frozen in `out/compare/crossval/sample.json`) is rated valid-important / valid-minor / invalid
  by Opus and by glm, each as a Claude Code agent in the PR worktree with the rule text; a model
  rating its own findings is the leniency control.

Every LLM step caches its verdicts as JSON lines next to its report; a rerun calls only what is
missing. `--dry-run` sizes the work.

## Known limitations

None of these changed the CLLM-598 numbers (checked on its data), but they would on other data:

- A PR where a reviewer has no finding at all is left out of that reviewer's recall denominator
  (`match_defects`) and of the oracle floor (`oracle_floor`); the findings files do not record
  reviewed PRs without findings.
- `finding_id` gives verbatim duplicate findings of one run the same ID, so the one-to-one matchers
  count them once while the totals count every copy (0.3% of deepseek's findings).
- `localize_failures` takes the JSON answer with a greedy `{.*}`: a brace in the text before it
  makes the answer unparsable (retried once, then counted as unparsed).
- `match_defects` expects `start_line` on every located cause and stops with `KeyError` otherwise.
- When every Claude Code seat is blocked, including by an expired token, a call waits up to
  `SEAT_WAIT_MAX` (6 h) before failing.
