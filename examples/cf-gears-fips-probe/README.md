# cf-gears-fips-probe

Outbound-HTTPS smoke for the toolkit / toolkit-http FIPS path.

Boots the **same crypto stack** that `cf-gears-example-server` uses
(`toolkit::bootstrap::init_crypto_provider` → corecrypto on macOS+fips,
Windows CNG on Windows+fips, AWS-LC FIPS on Linux+fips, AWS-LC default
otherwise), constructs a `toolkit_http::HttpClient`, makes one `GET`, and
prints what was negotiated.

The point of this binary is to give you a **fast, copy-pasteable smoke** to
verify a FIPS build end-to-end without spinning up cf-gears-example-server or writing
ad-hoc test code.

## What it verifies

| Layer | How | Where |
|---|---|---|
| Build | `cargo build` succeeds with `--features fips` | local |
| Linkage | `otool -L` shows only Apple frameworks (no `aws-lc-fips`) | `otool -L target/.../cf-gears-fips-probe` |
| Runtime crypto provider | log line `FIPS-140-3 crypto provider installed (...)` | probe stdout |
| Provider selection | self-identification in `[1]` step | probe stdout |
| Wire-level FIPS | `given_cipher_suites` from `howsmyssl.com` | probe `[3]` step + heuristics |
| Cert validation path | local self-signed cert is rejected | local s_server smoke |
| Loaded dylibs (runtime) | `vmmap` shows `libcorecrypto.dylib`, no `libaws_lc_fips` | external `vmmap <pid>` |

## Build

```sh
# Non-FIPS baseline (default)
cargo build -p cf-gears-fips-probe

# FIPS-conformant build
cargo build -p cf-gears-fips-probe --features fips
```

## Verify 1 — wire-level FIPS-conformance against a real TLS endpoint

[howsmyssl.com](https://www.howsmyssl.com/) inspects the `ClientHello` and
echoes back what the client offered (cipher suites, named groups,
post-quantum support, etc.) as JSON. This is **the** canonical external
oracle: a server that has no idea what crypto module you used internally
but can definitively tell you whether you offered any non-FIPS algorithms
on the wire.

### FIPS build

```sh
cargo run -p cf-gears-fips-probe --features fips -- \
  --url https://www.howsmyssl.com/a/check
```

Expected `given_cipher_suites`:

```
TLS_AES_256_GCM_SHA384
TLS_AES_128_GCM_SHA256
TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384
TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384
TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256
TLS_EMPTY_RENEGOTIATION_INFO_SCSV   # standard marker, not a real cipher
```

Expected `given_named_groups`:

```
secp256r1
secp384r1
```

Expected JSON flags:

```
"post_quantum_key_agreement": false
"unknown_cipher_suite_supported": false
```

The probe also prints:

```
✓ No ChaCha20 in ClientHello cipher_suites
```

**If you see ChaCha20-Poly1305, x25519, or X25519MLKEM768 in this list, the
FIPS build is broken** — the provider is offering non-FIPS algorithms on
the wire.

### Non-FIPS baseline (for contrast)

```sh
cargo run -p cf-gears-fips-probe -- \
  --url https://www.howsmyssl.com/a/check
```

You should see ChaCha20-Poly1305 variants, `x25519`, `X25519MLKEM768`, and
`post_quantum_key_agreement: true`. The probe prints:

```
⚠ ChaCha20 cipher offered by ClientHello — NOT FIPS-conformant
```

This is the textbook diff between FIPS and non-FIPS profiles.

## Verify 2 — local smoke against `openssl s_server`

For an offline FIPS-build smoke you can drive a handshake against a
locally-spawned openssl TLS server:

```sh
# Generate a self-signed cert if you don't have one already
openssl req -x509 -newkey rsa:2048 -nodes \
  -keyout /tmp/key.pem -out /tmp/cert.pem \
  -days 1 -subj '/CN=localhost' -addext 'subjectAltName=DNS:localhost'

# Start an s_server (TLS 1.3, FIPS-approved suite only)
openssl s_server \
  -cert /tmp/cert.pem -key /tmp/key.pem \
  -accept 8443 -www -quiet \
  -tls1_3 -ciphersuites TLS_AES_256_GCM_SHA384 &

# Drive a handshake through the toolkit FIPS stack
cargo run -p cf-gears-fips-probe --features fips -- --url https://localhost:8443/
```

Expected:

```
[1] crypto provider installed
    build profile: FIPS-enabled
    target_os: macos → Apple corecrypto       # or "Linux → AWS-LC FIPS"
[2] toolkit-http client ready
[3] GET https://localhost:8443/
Error: HTTP request failed
  Caused by: invalid peer certificate: Other(OtherError(CaUsedAsEndEntity))
```

The error is **correct** behaviour — `openssl s_server` presents a
self-signed cert as both leaf and CA, which `rustls-native-certs` refuses.
The point of this smoke is that the **handshake reached cert validation**:
that proves AEAD framing, HKDF/PRF, ECDH, and signature verification all
composed correctly through Apple corecrypto.

If you want a clean success instead, run the unit-level handshake tests
from the underlying provider crate:

```sh
cargo test -p cf-gears-rustls-corecrypto-provider --test handshake_smoke
```

Those use a custom `AcceptAnyServerCert` verifier so they actually complete
the handshake; three TLS 1.3 + TLS 1.2 suites are covered.

## Verify 3 — binary linkage (compile-time)

```sh
otool -L target/debug/cf-gears-fips-probe
```

On macOS + `--features fips`, you should see **only** Apple frameworks
plus `libSystem` / `libiconv`. In particular:

| Symbol | macOS + fips | Linux + fips |
|---|---|---|
| `Security.framework` | ✓ (linked) | — |
| `CoreFoundation.framework` | ✓ (linked) | — |
| `libaws_lc_fips_*.dylib` | **must be absent** | ✓ (this is the FIPS module on Linux) |

If `libaws_lc_fips_*.dylib` appears in the macOS+fips linkage, the
workspace-feature-unification shim (`cf-gears-rustls-fips-shim`) has regressed —
file a bug.

## Verify 4 — loaded dylibs (runtime)

Run the probe with a long-running target to keep the process alive long
enough to snap `vmmap`. The simplest: point it at a server that returns
slowly, or just run the local s_server smoke and `vmmap` the probe pid
while it's mid-handshake.

```sh
# In one shell:
cargo run -p cf-gears-fips-probe --features fips -- --url https://www.howsmyssl.com/a/check

# In another shell, while it's running:
vmmap <pid> | grep -E 'corecrypto|aws_lc|Security\.framework'
```

Expected:

```
__TEXT   /usr/lib/system/libcorecrypto.dylib              ← Apple FIPS module, active
__TEXT   /System/Library/Frameworks/Security.framework/.../Security
__TEXT   /System/Library/PrivateFrameworks/MessageSecurity.framework/.../MessageSecurity
```

No `libaws_lc_fips`, no `libcrypto`, no `libssl`, no `libring`.

## Caveats — what this probe does NOT prove

- It does **not** constitute a CMVP/NIST FIPS 140-3 certification audit.
  Certification is a process involving an accredited laboratory; this
  binary just gives you fast engineering-grade confidence that the build
  is wired correctly.
- The probe verifies the **client** path. Server-side TLS is intentionally
  unsupported in `cf-gears-rustls-corecrypto-provider` (cf-gears terminates
  HTTPS at the reverse proxy in production).
- On macOS the FIPS claim is only valid for **macOS versions whose
  corecrypto cert covers the running OS** — verify the current cert at
  <https://csrc.nist.gov/projects/cryptographic-gear-validation-program/validated-modules/search>.

## Verify on Windows

Windows handshake verification requires running on a real Windows host;
the workspace `check-windows-fips` Makefile target only proves the build
graph composes (cross-compile from Linux/macOS catches type / cfg /
feature regressions but never executes Windows code).

### Prerequisites

1. **Enable Windows system-wide FIPS mode.** Group Policy path:

   *Computer Configuration → Windows Settings → Security Settings →
   Local Policies → Security Options → "System cryptography: Use FIPS
   compliant algorithms for encryption, hashing, and signing" → Enabled*

   Or directly via registry (admin shell):

   ```powershell
   reg add HKLM\System\CurrentControlSet\Control\Lsa\FipsAlgorithmPolicy /v Enabled /t REG_DWORD /d 1 /f
   ```

   **Reboot.** The CNG runtime only reads this flag at boot.

2. **Confirm the flag** after reboot:

   ```powershell
   reg query HKLM\System\CurrentControlSet\Control\Lsa\FipsAlgorithmPolicy /v Enabled
   ```

   Expected: `Enabled    REG_DWORD    0x1`.

### Positive smoke

```powershell
cargo run -p cf-gears-fips-probe --features fips -- --url https://www.howsmyssl.com/a/check
```

Expected `[1]` step output:

```
[1] crypto provider installed
    build profile: FIPS-enabled
    target_os: windows -> Windows CNG (FIPS-approved set)
```

Expected wire-level (`given_cipher_suites`): only the six AES-GCM
suites and `TLS_EMPTY_RENEGOTIATION_INFO_SCSV`; no ChaCha20.
Expected `given_named_groups`: `secp256r1`, `secp384r1` only.
Probe heuristic prints `[OK] No ChaCha20 in ClientHello cipher_suites`.

### Negative test — fail-closed on FIPS-mode disabled

Disable Windows FIPS mode:

```powershell
reg add HKLM\System\CurrentControlSet\Control\Lsa\FipsAlgorithmPolicy /v Enabled /t REG_DWORD /d 0 /f
```

Reboot, then rerun the probe:

```powershell
cargo run -p cf-gears-fips-probe --features fips -- --url https://www.howsmyssl.com/a/check
```

Expected: process exits with an error containing
`SystemFipsModeNotEnabled` and the Group-Policy remediation hint. The
HTTP `GET` never executes — `init_crypto_provider` refuses before the
client is built.

This is the **fail-closed** path: rather than silently install a CNG
provider that would route through non-Approved algorithms, the bootstrap
refuses to start. Same posture as Microsoft documents in
<https://learn.microsoft.com/en-us/windows/security/threat-protection/fips-140-validation>.

### Linkage (Windows-specific)

```powershell
dumpbin /imports target\x86_64-pc-windows-msvc\debug\cf-gears-fips-probe.exe | findstr /i "bcrypt ncrypt aws"
```

Expected: `bcrypt.dll` appears (CNG primitives: `BCryptGenRandom`,
`BCryptGetFipsAlgorithmMode`, AES-GCM, ECDH, signature verify, hash,
HMAC). `ncrypt.dll` is **not** expected — `rustls-cng-crypto` is a
client-side TLS provider and uses BCrypt exclusively, not the NCrypt
key-storage surface. **`libaws_lc_fips*.dll` must be absent** — if it
appears, the `rustls-fips-shim` Windows exclusion has regressed.

The same check from a Linux/macOS host that produced the cross-compiled
binary, without `dumpbin`:

```sh
strings target/x86_64-pc-windows-msvc/debug/cf-gears-fips-probe.exe \
  | grep -iE '^(bcrypt|ncrypt|aws.?lc.?fips)\.dll$' | sort -u
# expected output:
# bcrypt.dll
```

## Adding new verification cases

The probe's `--url` argument accepts any HTTPS URL, so you can use it
against any TLS endpoint that's instructive (e.g. badssl.com sub-domains
to test rejection of weak ciphers, or your own staging endpoints to
verify FIPS-conformance against real corporate-CA-signed certs).
