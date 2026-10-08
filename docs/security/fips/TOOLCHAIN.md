# FIPS Toolchain

Any environment that builds `aws-lc-fips-sys` (local development, Docker,
CI) must have the following tools installed in addition to the Rust toolchain.
The versions below are derived from
[AWS-LC-FIPS 3.0.x](https://github.com/aws/aws-lc/tree/fips-2024-09-27).

| Tool | Minimum version | Recommended | Notes |
|------|-----------------|-------------|-------|
| **Go** | ≥ 1.17.13 | **1.21.x** | Builds AWS-LC integrity-test tooling; not used for application code. |
| **CMake** | 3.0 | latest | — |
| **Perl** | recent | latest | AWS-LC code-generation scripts. |
| **C/C++ compiler** | C++11 (GCC 4.1.3+ / Clang) | — | System default is typically sufficient. |

## Go version policy

Go is a hard build-time dependency of `aws-lc-fips-sys`, required to compile
AWS-LC's FIPS integrity-test tooling. It is **not** used for application code.

We standardize on **Go 1.21.x** across all build environments:

- **CI** - `actions/setup-go` pinned to `go-version: '1.21.x'`.
- **Docker** - base images should install Go 1.21.x.
- **Local dev** - any Go ≥ 1.21 works; avoid the bleeding-edge `stable`
  release to stay consistent with CI.

**When to bump**: only when AWS-LC upstream raises its minimum Go version or
a security fix in Go itself is relevant to the FIPS build tooling.

## References

- [AWS-LC BUILDING.md (FIPS branch)](https://github.com/aws/aws-lc/blob/fips-2024-09-27/BUILDING.md) - Go 1.17.13+
- [AWS-LC BUILDING.md (mainline)](https://github.com/aws/aws-lc/blob/main/BUILDING.md) - Go 1.20+
- [aws-lc-fips-sys README](https://github.com/aws/aws-lc-rs/blob/main/aws-lc-fips-sys/README.md)
- CI workflows: `.github/workflows/ci.yml`, `.github/workflows/api_contracts.yml`
