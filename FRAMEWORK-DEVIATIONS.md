# Framework Deviations

- review granularity (all slices) | cf-coding-review | ReviewGranularityMenu suggested `per-layer` (~23 reviewer dispatches per iteration); chose `per-methodology` (3 reviewers) to keep 7 slices x up to 3 review iterations within the session token budget
- review methodology delivery (all slices) | cf-coding-review | reviewer instruction assets (code-checklist, gears checklist, bug-finding, consistency-checklist + reviewer output contracts) were delivered as a controller-prepared context-pack file under /tmp/cfpack instead of being inlined into every dispatch prompt, to save tokens; content is a verbatim copy
- S2 review iteration 2 | cf-coding-review | re-review after fixes dispatched only the code-checklist and bug-finding reviewers scoped to the fix delta (consistency reviewer skipped: fix changed storage/paging internals only, no contract surface)
- S2 validate after fix iteration 2 (and later small re-validations) | cf-coding-ci | ran the studio checks (cfs validate --local-only, cfs check-language) directly in the main session instead of dispatching cf-deterministic-validator; same commands, results recorded in the stage report
- S3 review iteration 2 | cf-coding-review | re-review after fixes dispatched only the bug-finding reviewer scoped to the fix delta (ordering + pre-provider billing change)
- S4 review iteration 2 | cf-coding-review | re-review after minor fixes dispatched only the bug-finding reviewer scoped to the fix delta
- S5 review iteration 2 | cf-coding-review | re-review after the blocker fix dispatched only the bug-finding reviewer scoped to the fix delta
- S6 review iterations 2-3 | cf-coding-review | re-reviews after fixes dispatched only the bug-finding reviewer scoped to the fix delta
- S7 review iteration 2 | cf-coding-review | re-review after fixes dispatched only the bug-finding reviewer scoped to the fix delta
