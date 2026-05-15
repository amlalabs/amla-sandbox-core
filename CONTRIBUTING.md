# Contributing

Thanks for your interest in contributing to amla-sandbox-core.

## This is a release mirror

This repository is the public release source for the Rust workspace that
produces the `amla_sandbox.wasm` artifact bundled inside the
[amla-sandbox](https://pypi.org/project/amla-sandbox/) Python package.
Development happens upstream in
[amlalabs/monorepo](https://github.com/amlalabs/monorepo). On each release the
relevant crates are re-extracted from the monorepo and force-pushed here.

That means:

- Pull requests opened against this repository will be clobbered on the next
  release. We will not silently lose your work; if a PR has merit, a
  maintainer will copy it into the monorepo and credit you in the resulting
  commit message. But please assume the surface area here is read-only.
- Issues are welcome here. Bug reports, build problems on supported hosts,
  audit findings, and questions about the WASM artifact all belong here.
- Code changes are welcome as PRs against the monorepo. Link to the relevant
  monorepo path (e.g. `src/rust/crates/amla-sandbox/...`) in your description.

## Reporting issues

For build problems, please include the Rust toolchain version, your operating
system, and the full command and error output. For audit or security findings,
see `SECURITY.md` in the monorepo for the disclosure policy.

## Development checks

If you are opening a PR against the upstream monorepo (the normal route for
code changes, as described above), the monorepo uses a `pre-commit`
configuration that runs a set of hooks on every commit, including
`actionlint` for GitHub Actions workflow files. Please ensure all hooks
pass before opening the PR.

From the monorepo root:

```bash
pre-commit install            # one-time, wires the git hook
pre-commit run --all-files    # run every hook across the whole tree
```

CI runs the same set on every PR. Anything that fails locally will fail
there too. External contributors who only see this mirror repository do
not need to install pre-commit; the hooks live with the source.

## License

By contributing you agree your contribution will be licensed under
AGPL-3.0-or-later OR BUSL-1.1, matching the rest of the workspace.
