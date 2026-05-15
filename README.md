# amla-sandbox-core

Rust source for the WebAssembly sandbox runtime that powers
[amla-sandbox](https://pypi.org/project/amla-sandbox/) on PyPI. Each release
of this repository publishes an `amla_sandbox.wasm` binary as a GitHub
Release asset; the `amla-sandbox` Python wheel bundles that artifact at
packaging time.

This repository is the release source. Development happens in
[the amlalabs monorepo](https://github.com/amlalabs/monorepo); this repo is
updated on release.

## Why this is open

The primary consumer of `amla_sandbox.wasm` today is the `amla-sandbox`
Python package. The Rust source is open-sourced so that:

- Anyone deploying agents against this sandbox can audit how isolation,
  capability checks, the virtual filesystem, the JS runtime embedding, and the
  async scheduler are implemented.
- The WebAssembly artifact remains reproducible from source; you do not have
  to trust the prebuilt wheel.
- Future embedders (other host languages, alternative front-ends) have a path
  forward. No non-Python embedder is planned today; the surface is shaped for
  the Python host.

## Build

```sh
cargo build --release --target wasm32-wasip1 -p amla-sandbox
```

The output is `target/wasm32-wasip1/release/amla_sandbox.wasm` (cargo's
default underscore form for the crate name). The published GitHub Release for
each tag attaches this artifact directly under the same name,
`amla_sandbox.wasm`; it is not renamed.

You will need a recent stable Rust toolchain with the `wasm32-wasip1` target:

```sh
rustup target add wasm32-wasip1
```

## Layout

The repository is a pruned cargo workspace. The leaf crate
(`crates/amla-sandbox`) plus the transitive set of internal dependencies it
needs are present; unrelated crates from the monorepo are not. Workspace-level
config (`.cargo/`, `clippy.toml`, `deny.toml`, vendored sources) is included
so the workspace builds standalone.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). Pull requests against this mirror
will be clobbered on next release; please target the monorepo or open an issue
here.

## License

AGPL-3.0-or-later OR BUSL-1.1. See the top-level `LICENSE` file; the same
terms apply to every crate in the workspace.
