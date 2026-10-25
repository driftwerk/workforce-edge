# Contributing to Workforce Edge

Thanks for helping. This page covers how to get a working build, what we
expect from a pull request, and the licence terms contributions are accepted
under.

## Before you start

- For anything larger than a small fix, open an issue first so we can agree
  the approach before you write the code.
- Security problems go through [SECURITY.md](SECURITY.md), never a public
  issue.
- Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).
- Changes to the shared runtime (agent loop, memory, tools) belong in
  [Workforce](https://github.com/driftwerk/workforce); Edge pins it by revision.

## Licence

A contribution is licensed under the Elastic License 2.0, like the rest of the
repository (see the [README](README.md#licence)). By submitting a contribution
you also grant the licensor a perpetual, worldwide, royalty-free, irrevocable
licence to use, modify, sublicense and distribute it under any terms, including
in commercial products and services.

## Development setup

You need Rust (installed from `rust-toolchain.toml` by `rustup`) and Node.js
20+. Build the three UIs once, then the service:

```bash
for ui in widget-ui ui-admin ui-platform; do
  (cd crates/edge/$ui && npm ci && npm run build)
done
cargo build -p workforce-edge
```

## Before you open a pull request

Run what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --no-deps -- -D warnings
cargo test --workspace
cargo deny check licenses
for ui in widget-ui ui-admin ui-platform; do (cd crates/edge/$ui && npx vitest run); done
```

In the pull request:

- Keep it to one change, with tests for the behaviour it adds or fixes.
- Describe what changes for a customer or operator, not just what the code does.
- Don't add a dependency whose licence isn't in `deny.toml`'s allow-list.
- Don't commit credentials, customer data or real names in fixtures; use
  `example.com` hosts and obviously fake identifiers.

CI for pull requests runs on GitHub-hosted runners with read-only permissions
and no secrets. Workflows from first-time contributors need a maintainer's
approval before they run.
