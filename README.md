# Workforce Edge

The customer-facing side of [Workforce](https://github.com/driftwerk/workforce):
an AI support agent you embed in your own web app as a chat widget. Your
customers sign in through your identity provider (Keycloak or any OIDC issuer),
and the agent acts on their behalf against your company's MCP server — only with
the tools you allow, and only as that customer.

It runs as its own service with its own Postgres and vector store, isolated
from an internal Workforce hub, and can serve one company or many tenants from
one deployment.

Workforce Edge is **source-available** under the
[Elastic License 2.0](LICENSE): free to self-host and modify, including inside
a commercial organisation; not to be offered to others as a hosted service.
See the [licence FAQ](LICENSE-FAQ.md).

## Quick start

You need Rust (the toolchain in `rust-toolchain.toml` installs itself through
`rustup`), Node.js 20+, Postgres, and Weaviate or a vellev node.

```bash
git clone https://github.com/driftwerk/workforce-edge
cd workforce-edge
for ui in widget-ui ui-admin ui-platform; do
  (cd crates/edge/$ui && npm ci && npm run build)
done
cargo build --release -p workforce-edge

cp config/edge.example.toml edge.toml   # set Postgres, the vector store, the LLM and an agent
./target/release/workforce-edge edge.toml
```

The service listens on `[server] bind` (default `0.0.0.0:8080`). Embed the
widget in your app with the snippet the admin UI shows for each agent.

Edge builds against the Workforce core crates, pinned to one revision in
`Cargo.toml`. To work against a local core checkout, see `.cargo/config.toml`.

## Docker

Every push to `main` publishes `kingsleyh/driftwerk-edge:sha-<commit>` (and
`latest`) to Docker Hub. The image reads its config from
`WORKFORCE_EDGE_CONFIG` (default `/app/edge.toml`).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Report security issues privately as
described in [SECURITY.md](SECURITY.md).

## Licence

Copyright (c) 2026 Kingsley Hendrickse. Licensed under the
[Elastic License 2.0](LICENSE). See [NOTICE](NOTICE) for bundled third-party
data.
