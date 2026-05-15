# Secrets: amla-sandbox-core mirror release workflow

This document covers credentials consumed by
[`.github/workflows/release.yml`](./.github/workflows/release.yml) in
`amlalabs/amla-sandbox-core`.

The workflow builds `amla_sandbox.wasm` for `wasm32-wasip1`, generates a
SLSA build-provenance attestation for it, and attaches the wasm + `.sha256`
to a GitHub Release on this mirror. The downstream `amla-sandbox` mirror
fetches that release asset during its own publish run.

**There are no user-provided secrets to configure.** The workflow uses only
the Actions-provided `GITHUB_TOKEN` and OIDC. The rest of this document is
about the GitHub repo settings that must be in place for those to work.

Upstream of this workflow: the monorepo orchestrator pushes the `vX.Y.Z` tag
that triggers it. See [`../../../.github/SECRETS.md`](../../../.github/SECRETS.md)
for the PAT (`MIRROR_PUSH_TOKEN_AMLA_SANDBOX_CORE`) used for that push.

## Overview

| Name                   | Type                        | Purpose                                                          |
| ---------------------- | --------------------------- | ---------------------------------------------------------------- |
| `GITHUB_TOKEN`         | Auto-provided by Actions    | Create the GitHub Release; upload `amla_sandbox.wasm` assets.    |
| (no secret)            | OIDC `id-token: write`      | Sign the SLSA build-provenance attestation.                      |
| (no secret)            | `attestations: write`       | Submit the signed attestation to GitHub's attestation store.     |

## `GITHUB_TOKEN`

Auto-provided per job run. The `build-wasm` job declares:

```yaml
permissions:
  id-token: write
  attestations: write
  contents: write
```

`contents: write` is what makes `gh release create` / `gh release upload`
work. The token's scope is bounded to the run; you do not configure it
yourself.

### Repo prerequisites

Two equivalent ways to make `contents: write` available at the job level:

1. **Recommended (used here)**: leave repo defaults strict and let each
   workflow's per-job `permissions:` block widen as needed. This file's
   workflow does exactly that. Operator action: none.

2. **Permissive fallback**:
   **Settings -> Actions -> General -> Workflow permissions**:
   "Read and write permissions". This grants every workflow in the repo
   write access by default and is broader than necessary. Only use this if
   you also want unrelated workflows in the same repo to be able to push
   commits or create releases without their own `permissions:` block.

If the "Create or update GitHub Release" step fails with:

```
HTTP 403: Resource not accessible by integration
```

then `Settings -> Actions -> General -> Workflow permissions` is set
to "Read repository contents permissions" **and** the per-job
`permissions: contents: write` line was edited out. Restore the per-job
block (preferred) or flip the repo setting (broader).

### Scope of access

The Actions-issued `GITHUB_TOKEN`:

- **Can**: create GitHub Releases and upload release assets in
  `amlalabs/amla-sandbox-core` for the duration of the job. This workflow
  does not push commits or create tags; the upstream monorepo orchestrator
  is what pushes the `vX.Y.Z` tag that triggers this workflow.
- **Cannot**: touch any other repo. Cannot publish to package registries.
  Cannot read org-level secrets. Cannot persist beyond job end.

## OIDC `id-token: write` and `attestations: write`

The `Attest build provenance for amla_sandbox.wasm` step uses
[`actions/attest-build-provenance@v1`](https://github.com/actions/attest-build-provenance).

That action:

1. Requests an OIDC token from the Actions identity provider (needs
   `id-token: write`). The token carries claims about the workflow, the
   repo, the ref, and the runner.
2. Signs a SLSA provenance statement over the wasm artifact using Sigstore.
3. Submits the signed attestation to GitHub's attestation store (needs
   `attestations: write`).

There is no value to set for either; both are declared in the workflow's
`permissions:` block. Repo prerequisites:

- **Settings -> Actions -> General -> Workflow permissions** must allow
  `id-token: write` (default-allowed).
- The org must not have disabled OIDC issuance for the repo.

### Verifying an attestation after release

Anyone can verify the wasm was built by this exact workflow on this exact
repo with:

```bash
gh attestation verify amla_sandbox.wasm \
  --owner amlalabs \
  --repo amla-sandbox-core
```

This is the integrity check the downstream `amla-sandbox` mirror relies on
(in addition to the SHA256 hash check it does explicitly in its workflow).

Reference docs:
<https://docs.github.com/en/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds>
<https://github.com/actions/attest-build-provenance>

### Failure modes

| Symptom in `build-wasm` job                                              | Cause                                                       |
| ------------------------------------------------------------------------ | ----------------------------------------------------------- |
| `Error: Resource not accessible by integration` on attest step           | Job `permissions:` missing `id-token: write` or `attestations: write`. |
| `Error: OIDC token retrieval failed`                                     | OIDC disabled at org level. Re-enable in org Actions policies. |
| `Error: failed to create release` HTTP 403                               | `contents: write` missing from job permissions (see above). |
| `gh release view ... not found` then `release create` fails              | A previous partial run left orphan state. Delete the orphan release in the UI, re-run. |

## Rotation

Nothing to rotate. `GITHUB_TOKEN` is minted fresh per job; OIDC tokens are
short-lived (minutes) and not stored. No long-lived credential exists.

The only way to "compromise" this pipeline is to compromise the repo itself
(push access to `amla-sandbox-core` or modify the workflow file). The
upstream PAT for pushing the mirror is documented at
[`../../../.github/SECRETS.md`](../../../.github/SECRETS.md) and is what you
rotate to contain that risk.

## Branch protection setup

The monorepo orchestrator pushes new `main` commits and `vX.Y.Z` tags into
`amlalabs/amla-sandbox-core` on every release using the
`MIRROR_PUSH_TOKEN_AMLA_SANDBOX_CORE` PAT documented in the parent
[`.github/SECRETS.md`](../../../.github/SECRETS.md). The push bypasses code
review by design: this repo is a read-only release artifact and the
authoritative review happens upstream in the monorepo PR.

If branch protection is enabled on `main` (or tag protection on `v*`), the
PAT identity must be allowed to bypass the relevant checks or the
orchestrator push will fail.

Two equivalent ways to make this work:

1. **Recommended for a true release mirror**: leave `main` and `v*` with
   no branch protection. No human ever pushes to this repo directly, so
   protection adds no value, only operational friction.

2. **If branch protection is required by org policy**: in
   **Settings -> Branches -> Branch protection rules -> `main`**, add the
   bot user that owns `MIRROR_PUSH_TOKEN_AMLA_SANDBOX_CORE` to
   **"Allow specified actors to bypass required pull requests"**. If you
   also use **Settings -> Tags -> Tag protection rules** with a `v*`
   pattern, add the same bot user there too.

Symptom of misconfiguration: orchestrator logs show

```
remote: error: GH006: Protected branch update failed for refs/heads/main.
```

or, for tag-protection,

```
remote: error: GH013: Tag protection rule violated for v1.2.3.
```

Fix by adding the PAT identity to the bypass list, or by removing the
protection rule entirely.
