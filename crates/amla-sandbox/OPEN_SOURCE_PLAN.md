# amla-sandbox-core Rust Open Source Release Plan

## Instructions

After completing each task:

```bash
# Run pre-commit to verify changes
pre-commit run --all-files

# Run Codex review to ensure quality
codex review --uncommitted "Review for bugs, regressions, and code quality issues"

# Stage and commit
git add -A
git commit -m "chore(amla-sandbox-core): <description>"
```

**Quality gates:**

1. Pre-commit hooks must pass (formatting, linting)
2. Codex review must not flag critical issues
3. Tests must pass: `cargo test --all` (Rust) or `uv run pytest` (Python)

## Overview

Prepare the amla-sandbox Rust codebase for public release as `amla-sandbox-core`. This includes the main workspace (7 crates) and the `amla-protocol` workspace (3 crates) it depends on.

**Code size:** ~160K lines of Rust

## Crates

### amlalabs/amla-sandbox-core (7 crates)

| Crate | Description |
|-------|-------------|
| `amla-sandbox` | Main integration crate, WASM exports |
| `amla-js` | QuickJS runtime bindings |
| `amla-shell` | Shell implementation (pipes, builtins) |
| `amla-vfs` | In-memory virtual filesystem |
| `amla-scheduler` | Single-threaded async executor |
| `amla-tools` | Tool catalog with BM25/semantic search |
| `amla-audit` | Structured audit logging |

### amlalabs/amla-protocol (3 crates)

| Crate | Description |
|-------|-------------|
| `amla-protocol` | CBOR message protocol |
| `amla-capabilities` | Capability definitions (ToolCallCap, etc.) |
| `amla-constraints` | Constraint expression language |

## Tasks

### 1. Unify Capability Naming

Standardize capability type names across Python and Rust before open source release.

**Current state:**

| Language | Type Name | Location |
|----------|-----------|----------|
| Rust | `ToolCallCap` | `amla-capabilities/src/lib.rs` |
| Python | `MethodCapability` | `amla_sandbox/capabilities/method.py` |

**Target:** Use `ToolCallCap` everywhere (adopt Rust naming in Python).

**Rust changes:** None needed - already uses `ToolCallCap`.

**Python changes:**

- [x] Rename `MethodCapability` → `ToolCallCap` in `capabilities/method.py`
- [x] Rename file `method.py` → `tool_call.py`
- [x] Update `__init__.py` exports
- [x] Update all imports in `sandbox.py`, `langgraph.py`, `codeact.py`, `bash_tool.py`
- [x] Update all tests (`test_e2e.py`, `test_method.py` → `test_tool_call.py`, `test_integration.py`, etc.)
- [x] Update all examples (~15 files)
- [x] Update README.md

**Rationale:** Unified naming makes the API clearer for open source users. `ToolCallCap` is shorter and matches Rust convention. No deprecation needed - alpha software.

**Verify:**

```bash
# 1. No remaining references to MethodCapability in Python
grep -r "MethodCapability" src/python/packages/amla-sandbox/src/ && echo "FAIL: MethodCapability still referenced" || echo "OK"

# 2. ToolCallCap is exported and importable
cd src/python/packages/amla-sandbox
uv run python -c "from amla_sandbox import ToolCallCap; print('OK: ToolCallCap importable')"

# 3. All Python tests pass
uv run pytest tests/ -v

# 4. Examples still work (spot check)
uv run python examples/capabilities.py
```

### 2. Rename Workspace to amla-sandbox-core

Rename the Rust workspace/repo from `amla-sandbox` to `amla-sandbox-core` to distinguish it from the Python package.

- [x] Rename directory `src/rust/amla-sandbox` → `src/rust/amla-sandbox-core`
- [x] Update `.pre-commit-config.yaml` (many references to `src/rust/amla-sandbox`)
- [x] Update `scripts/release-amla-sandbox.sh` path references
- [x] Update `scripts/tag-amla-sandbox.sh` if needed
- [x] Update any other scripts referencing the old path
- [x] Public repo will be `github.com/amlalabs/amla-sandbox-core`

**Note:** The `amla-sandbox` *crate* name stays the same so the WASM binary remains `amla_sandbox.wasm`.

**Verify:**

```bash
# 1. Directory renamed
ls src/rust/amla-sandbox-core/Cargo.toml && echo "OK: Directory renamed" || echo "FAIL"

# 2. Old directory gone
! ls src/rust/amla-sandbox/Cargo.toml 2>/dev/null && echo "OK: Old dir removed" || echo "FAIL"

# 3. No remaining references to old path in scripts
grep -r "src/rust/amla-sandbox[^-]" scripts/ .pre-commit-config.yaml && echo "FAIL: Old path referenced" || echo "OK"

# 4. Rust builds
cd src/rust/amla-sandbox-core && cargo check

# 5. WASM builds
cargo build --release --target wasm32-wasip1 -p amla-sandbox
ls target/wasm32-wasip1/release/amla_sandbox.wasm && echo "OK: WASM binary exists"
```

### 3. Prepare amla-protocol as Separate Repo

Keep amla-protocol as a separate workspace for independent release.

- [x] Ensure amla-protocol workspace is self-contained in `src/rust/amla-protocol`
- [x] Verify amla-sandbox-core depends on amla-protocol via path in monorepo
- [x] Add README, LICENSE to amla-protocol
- [ ] Create CI workflow for amla-protocol (deferred - needs GitHub repo first)

**Dependency strategy:**

- **In monorepo:** Use path dependencies for fast iteration

  ```toml
  amla-protocol = { path = "../amla-protocol/crates/amla-protocol" }
  ```

- **In public release:** Release script rewrites to git dependencies (see Task 10)

  ```toml
  amla-protocol = { git = "https://github.com/amlalabs/amla-protocol", tag = "vX.Y.Z" }
  ```

**Result:** Two independent Rust workspaces that can be released separately.

**Verify:**

```bash
# 1. amla-protocol builds independently
cd src/rust/amla-protocol
cargo check --all
cargo test --all

# 2. amla-sandbox-core builds with protocol dependency
cd src/rust/amla-sandbox-core
cargo check --all

# 3. Both have required files
for repo in amla-protocol amla-sandbox-core; do
  for f in README.md LICENSE; do
    test -f "src/rust/$repo/$f" && echo "OK: $repo/$f" || echo "FAIL: $repo/$f"
  done
done
```

### 4. Metadata Updates

Update all `Cargo.toml` files:

- [x] **Repository URL**: Changed to `https://github.com/amlalabs/amla-sandbox-core` and `https://github.com/amlalabs/amla-protocol`
- [x] **Author email**: Changed to `souvik@amlalabs.com`
- [x] **License**: Changed Rust workspace crates to `AGPL-3.0-or-later OR BUSL-1.1`

**License:** Use `AGPL-3.0-or-later OR BUSL-1.1` for Rust workspace crates. The
`amla-vm-guest-rootfs` crate additionally carries `GPL-2.0-only` Linux
kernel material.

Current state:

- Workspace default: `AGPL-3.0-or-later OR BUSL-1.1`
- amla-vm-guest-rootfs: `(AGPL-3.0-or-later OR BUSL-1.1) AND GPL-2.0-only`

Action: Keep Rust crate manifests aligned with the workspace default, with
explicit exceptions only for third-party or kernel-derived material.

**Verify:**

```bash
cd src/rust/amla-sandbox-core

# 1. All Cargo.toml files have correct license
for f in Cargo.toml crates/*/Cargo.toml; do
  grep -Eq 'license(\.workspace)? = true|license = "(\()?AGPL-3.0-or-later OR BUSL-1.1' "$f" && echo "OK: $f" || echo "FAIL: $f missing expected license"
done

# 2. All have correct author email
grep -r "souvik@amlalabs.com" crates/*/Cargo.toml | wc -l
# Should match number of crates

# 3. No references to old repo URL
grep -r "amlalabs/monorepo" . && echo "FAIL: Old repo URL found" || echo "OK"

# 4. LICENSE file exists and points to the license choices
head -1 LICENSE  # Should say "# Amla Licensing"
```

### 5. Clippy Lint Audit

**Goal:** Reduce the number of `#[allow(...)]` clippy lints in each crate's `Cargo.toml` to improve code quality before open source release.

**Current state:** Many crates have extensive clippy allow lists (e.g., `amla-shell` has 20+ allows).

**Process:** Use subagents for each crate to maintain proper context:

```
For each crate in [amla-sandbox, amla-js, amla-shell, amla-vfs, amla-scheduler, amla-tools, amla-audit, amla-protocol, amla-capabilities, amla-constraints]:

1. Launch a subagent with prompt:
   "Review the clippy lints in crates/<crate>/Cargo.toml [lints.clippy] section.
    For each `allow`, determine if it can be removed by fixing the underlying code.
    Priority: Remove allows that hide real issues. Keep allows that are intentional
    (e.g., FFI patterns, WASM-specific code, test readability).
    Make the fixes and update Cargo.toml. Run `cargo clippy -p <crate>` to verify."

2. After each crate, commit: "refactor(<crate>): reduce clippy allows"
```

**Crates to audit (in order):**

- [x] `amla-scheduler` (minimal allows - 5 workspace defaults)
- [x] `amla-audit` (no allows - uses workspace defaults)
- [x] `amla-vfs` (8 allows - intentional)
- [x] `amla-tools` (no allows - uses workspace defaults)
- [x] `amla-protocol` (no allows - uses workspace defaults)
- [x] `amla-capabilities` (no allows - uses workspace defaults)
- [x] `amla-constraints` (no allows - uses workspace defaults)
- [x] `amla-js` (8 allows - FFI/WASM necessary)
- [x] `amla-shell` (33 allows - shell/jq parsing edge cases)
- [x] `amla-sandbox` (8 allows - WASM exports necessary)

**Status:** All crates pass `cargo clippy --all -- -D warnings`. Current allows are intentional suppressions for FFI, WASM, and parser-specific patterns.

**Verify (per crate):**

```bash
# After each crate audit:
cargo clippy -p <crate> -- -D warnings
cargo test -p <crate>
```

**Verify (final):**

```bash
# All crates pass strict clippy
cargo clippy --all -- -D warnings

# Count remaining allows (should be reduced)
grep -r "allow\." crates/*/Cargo.toml | wc -l
```

### 6. Documentation

- [x] Create top-level `README.md` for the workspace
- [x] Review existing crate READMEs for accuracy
- [x] Add `CONTRIBUTING.md`
- [x] Add `SECURITY.md`
- [x] Verify rustdoc builds cleanly: `cargo doc --no-deps` (7 minor warnings for internal links, no errors)

**Verify:**

```bash
cd src/rust/amla-sandbox-core

# 1. README exists and has content
test -s README.md && echo "OK: README exists" || echo "FAIL"

# 2. Required files exist
for f in CONTRIBUTING.md SECURITY.md LICENSE; do
  test -f "$f" && echo "OK: $f" || echo "FAIL: $f missing"
done

# 3. Rustdoc builds without warnings
cargo doc --no-deps 2>&1 | grep -i warning && echo "FAIL: Doc warnings" || echo "OK"

# 4. Each crate README references correct repo
grep -l "amla-sandbox-core" crates/*/README.md | wc -l
```

### 7. Code Review

- [x] Check for hardcoded secrets/credentials - **None found**
- [x] Check for internal URLs - **Updated repo URLs to public**
- [x] Review TODO/FIXME comments for sensitive info - **Only in test examples, OK**
- [x] Check for any proprietary algorithms that shouldn't be released - **None**
- [x] Verify all dependencies are open source compatible - **cargo deny licenses OK**

**Verify:**

```bash
cd src/rust/amla-sandbox-core

# 1. No TODO/FIXME with sensitive info
grep -rn "TODO\|FIXME" crates/ --include="*.rs" | head -20
# Review output manually for sensitive content

# 2. No hardcoded secrets patterns
grep -rn "password\|secret\|api_key\|token" crates/ --include="*.rs" | grep -v "test\|example\|doc" || echo "OK"

# 3. All deps are open source (cargo-deny)
cargo deny check licenses
```

### 8. CI/CD

- [x] Create GitHub Actions workflow for:
  - `cargo check`
  - `cargo test`
  - `cargo clippy`
  - `cargo fmt --check`
  - `cargo doc`
  - `wasm build` (amla-sandbox-core only)
- [x] Set up dependabot for dependency updates
- [ ] Add branch protection rules (manual step after repo creation)

**Verify:**

```bash
# 1. Workflow file exists and is valid YAML
cat .github/workflows/ci.yml | head -20

# 2. Workflow includes all required jobs
grep -E "cargo (check|test|clippy|fmt|doc)" .github/workflows/ci.yml

# 3. dependabot.yml exists
test -f .github/dependabot.yml && echo "OK" || echo "FAIL"
```

### 9. Build Verification

- [x] Verify native build: `cargo build --release` - **OK**
- [x] Verify WASM build: `cargo build --release --target wasm32-wasip1` - **13MB binary**
- [x] Run full test suite: `cargo test --all` - **All tests pass**
- [x] Run clippy: `cargo clippy --all -- -D warnings` - **Clean**

**Verify:**

```bash
cd src/rust/amla-sandbox-core

# 1. Native build succeeds
cargo build --release
echo "Native build: OK"

# 2. WASM build succeeds
cargo build --release --target wasm32-wasip1 -p amla-sandbox
ls -lh target/wasm32-wasip1/release/amla_sandbox.wasm

# 3. All tests pass
cargo test --all

# 4. Clippy clean
cargo clippy --all -- -D warnings

# 5. Format check
cargo fmt --all -- --check
```

### 10. Release Process

> **Historical note:** This section describes the original release flow that
> triggered on `amla-sandbox-X.Y.Z` tags and shelled out to
> `scripts/release-amla-sandbox.sh`. The live release pipeline is now the
> orchestrated `release-v*` flow defined in `.github/workflows/release.yml`,
> which pushes to the `amla-sandbox-core` and `amla-sandbox` mirrors. The
> shell scripts and the `release-amla-sandbox.yml` workflow referenced below
> have been removed.

Update `scripts/release-amla-sandbox.sh` for unified releases:

- [x] Remove WASM binary from Python GitHub repo (amlalabs/amla-sandbox) - **Script updated**
- [x] Add step to prepare and push Rust source to amlalabs/amla-sandbox-core - **Script updated**
- [x] Add step to prepare and push Rust source to amlalabs/amla-protocol - **Script updated**
- [x] **Rewrite path dependencies to git dependencies in released code:**
  - In monorepo: `amla-protocol = { path = "../amla-protocol/crates/amla-protocol" }`
  - In release: `amla-protocol = { git = "https://github.com/amlalabs/amla-protocol", tag = "vX.Y.Z" }`
- [x] PyPI release workflow still includes WASM binary - **Unchanged**
- [ ] All three repos tagged with same version on release - **Manual step after GitHub repos created**

**Note:** Full release process testing requires actual GitHub repos to verify git dependencies.

**Dependency rewriting:**

```bash
# In release script, after copying amla-sandbox-core:
# Replace path dependencies with git dependencies

sed -i 's|path = ".*amla-protocol.*"|git = "https://github.com/amlalabs/amla-protocol", tag = "v'"$VERSION"'"|g' \
    "$RELEASE_DIR/amla-sandbox-core/Cargo.toml"

# Similar for any crate-level Cargo.toml files that reference amla-protocol
```

**Release order matters:**

1. First: Push amla-protocol and tag it
2. Then: Update amla-sandbox-core Cargo.toml to reference the tag
3. Then: Push amla-sandbox-core and tag it
4. Finally: Build WASM and release Python package

**Release flow:**

```
Tag: amla-sandbox-X.Y.Z
        │
        ├─→ 1. amlalabs/amla-protocol (GitHub) - tag first
        │       └── Rust protocol crates
        │
        ├─→ 2. amlalabs/amla-sandbox-core (GitHub) - references protocol via git tag
        │       └── Rust sandbox crates (Cargo.toml rewritten)
        │
        ├─→ 3. Build WASM binary (from monorepo, not public repos)
        │
        ├─→ 4. amlalabs/amla-sandbox (GitHub)
        │       └── Python source only (no WASM)
        │
        └─→ 5. PyPI: amla-sandbox
                └── Python source + WASM binary (preferred install)
```

**Note:** No crates.io publishing - Rust code is consumed as WASM, not as a Rust dependency.

**Verify:**

```bash
# 1. Release script runs without error (dry run)
./scripts/release-amla-sandbox.sh --help

# 2. Script handles dependency rewriting
grep -E "sed.*amla-protocol|git.*amla-protocol" scripts/release-amla-sandbox.sh

# 3. Test release to staging directory
./scripts/release-amla-sandbox.sh -v 0.0.0-test -o /tmp/release-test

# 4. Verify path deps are rewritten in staged release
grep -r "path.*amla-protocol" /tmp/release-test/amla-sandbox-core/ && echo "FAIL: path deps remain" || echo "OK"
grep "git.*amla-protocol" /tmp/release-test/amla-sandbox-core/Cargo.toml && echo "OK: git dep found" || echo "FAIL"
```

**Local build test (without pushing):**

Since we can't push to GitHub to test, verify the rewritten code can build by temporarily pointing to a local path that simulates the public repo structure:

```bash
# 1. Copy amla-protocol to a temp location (simulating public repo)
cp -r src/rust/amla-protocol /tmp/amla-protocol-public

# 2. Copy amla-sandbox-core to temp location
cp -r src/rust/amla-sandbox-core /tmp/amla-sandbox-core-public

# 3. Rewrite Cargo.toml to use the temp path (simulating git clone)
cd /tmp/amla-sandbox-core-public
sed -i 's|path = ".*amla-protocol.*"|path = "/tmp/amla-protocol-public/crates/amla-protocol"|g' Cargo.toml
# Repeat for amla-capabilities, amla-constraints if needed

# 4. Verify it builds with the "external" dependency
cargo check --all

# 5. Clean up
rm -rf /tmp/amla-protocol-public /tmp/amla-sandbox-core-public
```

**Note:** Full integration test requires pushing to GitHub. The local test above validates the dependency structure is correct.

### 11. Update Python README

Update `src/python/packages/amla-sandbox/README.md`:

- [x] Add link to `github.com/amlalabs/amla-sandbox-core` (Rust source)
- [x] Add link to `github.com/amlalabs/amla-protocol` (protocol crates)
- [x] Add "Building from source" section with instructions for GitHub installs
- [x] Clarify that PyPI is the preferred install method

**Verify:**

```bash
cd src/python/packages/amla-sandbox

# 1. README links to Rust repos
grep "amla-sandbox-core" README.md && echo "OK: sandbox-core linked" || echo "FAIL"
grep "amla-protocol" README.md && echo "OK: protocol linked" || echo "FAIL"

# 2. Building from source section exists
grep -i "building from source\|build.*source" README.md && echo "OK" || echo "FAIL"

# 3. PyPI preference mentioned
grep -i "pypi\|pip install" README.md | head -3
```

## Public Repository Structure

### amlalabs/amla-sandbox-core (Rust sandbox)

```
amla-sandbox-core/
├── .github/
│   └── workflows/
│       └── ci.yml
├── crates/
│   ├── amla-sandbox/      # Main WASM exports
│   ├── amla-js/           # QuickJS bindings
│   ├── amla-shell/        # Shell implementation
│   ├── amla-vfs/          # Virtual filesystem
│   ├── amla-scheduler/    # Async executor
│   ├── amla-tools/        # Tool catalog
│   └── amla-audit/        # Audit logging
├── Cargo.toml (workspace)
├── Cargo.lock
├── README.md
├── LICENSE
├── CONTRIBUTING.md
├── SECURITY.md
└── NOTICES (third-party licenses)
```

### amlalabs/amla-protocol (Rust protocol)

```
amla-protocol/
├── .github/
│   └── workflows/
│       └── ci.yml
├── crates/
│   ├── amla-protocol/     # CBOR message protocol
│   ├── amla-capabilities/ # Capability definitions
│   └── amla-constraints/  # Constraint DSL
├── Cargo.toml (workspace)
├── Cargo.lock
├── README.md
├── LICENSE
├── CONTRIBUTING.md
└── SECURITY.md
```

## Versioning & Distribution Strategy

**Unified versioning:** All repos share the same version. The live pipeline
triggers on `release-vX.Y.Z` tags in this monorepo (see
`.github/workflows/release.yml`); the original `amla-sandbox-X.Y.Z` tag
prefix described below is deprecated. A single release tag triggers release of:

- `amlalabs/amla-sandbox` (Python)
- `amlalabs/amla-sandbox-core` (Rust sandbox)
- `amlalabs/amla-protocol` (Rust protocol)

**Distribution:**

| Install Method | WASM Binary | Notes |
|----------------|-------------|-------|
| `pip install amla-sandbox` (PyPI) | ✅ Included | Preferred method, works out of the box |
| `pip install git+.../amla-sandbox` (GitHub) | ❌ Not included | Must build Rust separately |

**GitHub installs require manual WASM build:**

```bash
# 1. Build the Rust WASM binary
git clone https://github.com/amlalabs/amla-sandbox-core
cd amla-sandbox-core
cargo build --release --target wasm32-wasip1 -p amla-sandbox

# 2. Set environment variable
export AMLA_WASM_PATH=/path/to/amla-sandbox-core/target/wasm32-wasip1/release/amla_sandbox.wasm

# 3. Install Python package
pip install git+https://github.com/amlalabs/amla-sandbox
```

**Rationale:**

- Keeps GitHub repos clean (no large binaries)
- PyPI is the preferred install method for most users
- Advanced users who install from GitHub can build from source

## Timeline

| Phase | Tasks | Est. Effort |
|-------|-------|-------------|
| 1 | Unify capability naming (`ToolCallCap`) | 1-2 hours |
| 2 | Rename workspace to amla-sandbox-core | 30 min |
| 3 | Prepare amla-protocol as separate repo | 1 hour |
| 4 | Metadata updates (license, email, URLs) | 30 min |
| 5 | Clippy lint audit (10 crates via subagents) | 3-4 hours |
| 6 | Documentation | 2 hours |
| 7 | Code review | 30 min |
| 8 | CI/CD setup | 1 hour |
| 9 | Build verification | 30 min |
| 10 | Release process updates | 1 hour |
| 11 | Update Python README | 30 min |

**Total estimate:** ~11-13 hours

## Notes

- WASM binary is bundled in PyPI package only, not in GitHub repos
- QuickJS source is vendored in `amla-js/quickjs/` (MIT licensed)
- Model2Vec embeddings model bundled in `amla-tools` (~8MB, MIT licensed)
- Python package code remains MIT licensed; Rust workspace crates are
  AGPL-3.0-or-later OR BUSL-1.1.
