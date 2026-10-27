# Workforce Edge

The customer-facing side of [Workforce](https://github.com/driftwerk/workforce):
an AI support agent you embed in your own web app as a chat widget. Your
customers sign in through your identity provider (Keycloak or any OIDC issuer),
and the agent acts on their behalf against your company's MCP servers — only with
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

An agent acts against one primary MCP server (`[agents.embed.mcp]`) and,
optionally, more servers alongside it (`[[agents.embed.mcp_servers]]`). Each
server has its own URL, credential, auth mode, token audience, allowlist and
write tiers. A server that is not `required` being down only removes its tools
for that turn. One that times out or cannot be reached (connection refused,
DNS, TLS, connect timeout) also sits out the next `unavailable_skip_secs`
seconds (default 60, 0 never skips). One that answers with an error, or whose
host resolves to an address the outbound policy refuses, is retried on the
next turn. A server whose URL cannot be used (missing, for example an unset
`${VAR}`, which expands to an empty string; not a URL; or refused by the
outbound policy as written) fails startup, a git sync or an admin save when it
is `required`. Otherwise the agent is built without it, and the edge logs it
once by server id and counts `edge_mcp_server_excluded_total`. It returns with
the next rebuild that carries a usable URL: a repo change, an admin save, or a
restart (an environment variable is read only at start). Under `public`, an
admin save refuses such a URL with a 422 naming the field. Under `public`,
only a required server's host is resolved at build; any other server's host is
resolved when it is reached, so a DNS failure is an outage, not a config
fault. The startup discovery of a server that is not `required` runs in the
background, so it never holds boot or an admin save. Give each server its own
token-exchange `audience`, so a token minted for one server is not accepted by
another. `config/edge.example.toml` describes every key. `edge.toml` seeds agents on first boot only, so an
existing install adds servers through the admin UI, the admin API or its git
config.

Edge builds against the Workforce core crates, pinned to one revision in
`Cargo.toml`. To work against a local core checkout, see `.cargo/config.toml`.

## Metrics

With `[metrics] enabled = true`, the edge serves Prometheus metrics at
`GET /metrics`. A scraper must send `Authorization: Bearer <bearer_token>`.
The token is at least 32 characters, and the endpoint is never served without
one. With `[metrics]` absent or disabled, the route does not exist and nothing
is recorded.

Every label value comes from configuration or a fixed set, so the number of
series is bounded by the config. Labels never carry a customer, a token, a
tool's arguments or a URL.

- `tenant`, `agent` and `server` are ids from the config. Every series except
  `edge_mcp_config_refusals_total` also carries `tenant`, because agent ids are
  unique only within a tenant. A single-tenant install's tenant and the
  primary server are both `default`.
- `tool` is an allowlisted name.
- `audience` is a configured audience, or `none` for an exchange that asks for
  no audience.

| Metric | Type | Labels |
|---|---|---|
| `edge_mcp_discovery_total` | counter | `tenant`, `agent`, `server`, `phase` (`startup`, `lazy`, `refresh`), `outcome` (`ok`, `failed`, `timed_out`, `unreachable`) |
| `edge_mcp_discovery_duration_seconds` | histogram | `tenant`, `agent`, `server`, `phase` |
| `edge_mcp_tools_cache_total` | counter | `tenant`, `agent`, `server`, `result` (`hit`, `miss`, `stale`) |
| `edge_mcp_tool_calls_total` | counter | `tenant`, `agent`, `server`, `tool`, `outcome` (`ok`, `tool_error`, `unreachable`, `transport_error`, `timeout`) |
| `edge_mcp_tool_call_duration_seconds` | histogram | `tenant`, `agent`, `server`, `tool` |
| `edge_mcp_server_skipped_total` | counter | `tenant`, `agent`, `server`. A turn skipped a server inside its skip window. |
| `edge_mcp_skip_window_entered_total` | counter | `tenant`, `agent`, `server`. A startup discovery or turn start that timed out or could not reach the server opened its skip window. |
| `edge_mcp_server_excluded_total` | counter | `tenant`, `agent`, `server`, `reason` (`missing_url`, `invalid_url`, `refused_url`). A build left out a server that is not required because its URL cannot be used. |
| `edge_mcp_token_exchanges_total` | counter | `tenant`, `agent`, `audience`, `outcome` (`ok`, `failed`) |
| `edge_mcp_token_exchange_duration_seconds` | histogram | `tenant`, `agent`, `audience` |
| `edge_mcp_config_refusals_total` | counter | `reason`, one of `missing_primary`, `invalid_id`, `reserved_id`, `duplicate_id`, `invalid_prefix`, `empty_allowlist`, `invalid_grant`, `outside_prefix`, `invalid_audience`, `skip_on_required`, `name_collision` |

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
