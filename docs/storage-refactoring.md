# Storage lifecycle refactoring

The Rust-default migration centralizes production package selection and shares
the mechanics used by lifecycle readers and qualification fixtures.

## Runtime structure

- `json_tokens` owns validated token boundaries and the 128-container nesting
  limit. Receipt decoding preserves literal discriminator objects and exact
  integers; retention chooses its own typed projection of required fields.
- `accounts` copies UID lookup results before releasing NSS storage. Its callers
  retain their original buffer bounds and diagnostics.
- Recovery preparation admits its command set once, before leases or workers.
  Producer snapshot checks, retained snapshot verification and local restore
  acceptance have named scopes owning their evidence leases. The outer fence
  and preparation lease span export, off-host import and final admission.
- Transition validation proceeds through binding, barrier-path and worker
  validation in the original order. Cutover shares prepared-journal discovery.
- Flake modules import one keyed native-package argument module. Direct imports
  use the native build default; the former production package adapter is removed.
- Native VM fixtures explicitly select interoperability and share bridge
  packaging, descriptor transport and cleanup. Each fixture retains its existing
  command arguments, acceptance filename and native assertions.

## Comparable measurements

Measured with jscpd 5.4.1 against `c730ef9ac3cf09a95f307714ac2ed01649b0d950`,
the receipt-decoder checkpoint, using identical flags for each scope. Inline
Rust tests and Nix fixtures are included.

| Scope | Exact clones before / after | Duplicated lines before / after | Health before / after |
| --- | --- | --- | --- |
| `src/storage` + `src/bin` | 15 / 10 | 1.43% / 0.89% | 55 / 56 |
| `nix` + `flake.nix` | 17 / 7 | 3.67% / 1.27% | 83 / 90 |

Rust aggregate estimated complexity moved from 3674 to 3658. The recovery and
transition validation helpers improve orchestration readability, while their
files' individual complexity estimates increased slightly. No file split is
counted as a complexity reduction. The remaining clones include distinct
validation policies and fixtures whose contracts merit explicit implementation.

Reproduce the comparable scans with:

```sh
jscpd --dashboard --reporters json,console --format rust --no-gitignore src/storage src/bin
jscpd --dashboard --reporters json,console --format nix --formats-exts 'nix:nix' --no-gitignore nix flake.nix
```

All-target/all-feature Cargo compiler diagnostics also yielded zero dead-code
findings. That compiler-backed scan adds a dimension to the health score and is
not compared to the two-dimension scores above. Public API reachability still
requires caller analysis; the scan does not authorize removal of public helpers.

`tests/trace_migration.py` records per-scenario executed Python functions and
lines for the unchanged 174-method corpus. Its reports explicitly retain the
child-interpreter tracing limitation and do not convert executed lines or
semantic similarity into assertion-level Rust parity.
