# Framework deviations

- tests/author stages (all slices) | cf-coding-tests / cf-coding-gen via cf-gears-coding | Dispatched `cf-generate-coder-smart` with a scope larger than its five-source/test-file tier boundary: the router mandates a main-context coder and the CodingAuthorDispatch selection offers no larger code tier, and the gear (SDK crate plus gear crate) cannot be written in five files per slice.
