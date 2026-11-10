# ldk-server-gui

A web dashboard and (optional) control panel for an [ldk-server](https://github.com/lightningdevkit/ldk-server) node.

Shows:

- **Node info** — alias, node ID, network, chain tip, sync staleness, listening/announced addresses
- **Balances** — total, on-chain spendable, lightning, anchor reserve
- **Channels** — status, counterparty, capacity, outbound/inbound liquidity with a balance bar
- **Peers** — connected state and addresses
- **Recent payments** — time, direction, kind, amount, fee, status
- **Live activity feed** — node events (payments, channel state changes, splices) as they happen,
  plus an audit trail of control-panel actions

## Control panel

With `--enable-actions`, a **Control** page at `/control` can invoke node commands — a curated
subset of what `ldk-server-cli` can do:

- **Payments** — pay BOLT11 invoices, BOLT12 offers, keysend (spontaneous) payments
- **Receive** — create BOLT11 invoices
- **On-chain** — generate receive addresses, send on-chain (fixed amount or all funds)
- **Channels** — open, cooperative close, force close, splice in/out, update forwarding fees/CLTV
- **Peers** — connect (with optional persistence), disconnect
- **Forwarding stats** — tracking mode, forwarded payments history, per-channel stats
  (also available read-only under `/api/stats/*` without actions enabled)

### Action token

Because the GUI holds the node's **admin macaroon**, every mutating request must carry a random
action token that the GUI generates on each start and prints to the console:

```
actions enabled — control panel: http://127.0.0.1:3540/control
action token (enter it on the control page): 3116c922…
```

A new token at every start is awkward for a service that restarts. With `--action-token-file
<path>` the token is read from a file instead (at least 24 characters, for example from
`openssl rand -hex 32`, mode 600). It then stays the same across restarts and is never printed.

The control page asks for it once and stores it in the browser's `localStorage`; it is sent as the
`X-Action-Token` header. This blocks CSRF ( cross-origin pages can't set custom headers), but the
overall threat model is still "localhost-only": **do not bind the GUI to a non-loopback address
when actions are enabled** unless you add proper authentication in front of it.

## Liveness model

The backend polls the node's gRPC API every `--poll-interval-secs` (default 5s) *and* subscribes to
the node's event stream (`SubscribeEvents`). Every received event — and every successful control-panel
action — triggers an immediate snapshot refresh. Updates are pushed to browsers over Server-Sent
Events (`/api/stream`), so the page updates without reloading.

## Usage

```bash
cargo build --release
./target/release/ldk-server-gui --config /path/to/ldk-server-config.toml
```

Then open <http://127.0.0.1:3540>.

Connection settings resolve exactly like `ldk-server-cli`: base URL, admin macaroon, and the
server's TLS certificate are read from the ldk-server config file and its storage directory by
default. Overrides:

| Flag | Purpose | Default |
|------|---------|---------|
| `--base-url` | gRPC endpoint of the node | from config |
| `--macaroon` | hex admin macaroon | `admin.macaroon` from the node's storage dir |
| `--tls-cert` | server TLS certificate (PEM) | `tls.crt` from the node's storage dir |
| `--config` | ldk-server config file | `~/.ldk-server/config.toml` |
| `--listen` | dashboard bind address | `127.0.0.1:3540` |
| `--poll-interval-secs` | snapshot poll interval | `5` |
| `--enable-actions` | enable the `/control` panel (prints a per-start action token) | off |
| `--action-token-file` | read the action token from this file instead (needs `--enable-actions`) | random per start |

## Versions

Every page ends with a line showing what is running: this dashboard, and the node it talks to
(ldk-server, the ldk-node inside it, and the kyoto inside that), each with its version, git branch
and commit. `+uncommitted` means it was built from a tree with uncommitted changes.

The dashboard records its own at build time (`build.rs`). The node reports the other three in the
`ldk_server_build_info` metric of its `/metrics`, recorded by ldk-server's `build.rs` from the
checkouts it is compiled against, so the node needs its metrics enabled. `/api/versions` returns the
same as JSON.

## Building

The dashboard builds on its own: `ldk-server-client` comes from
`https://github.com/randomlogin/ldk-server`, branch `cbd-chain-source` (set in `Cargo.toml`), at the
exact commit `Cargo.lock` pins. `cargo build --release` needs network access to fetch it, and `git`.

Move to a newer client with `cargo update -p ldk-server-client`. To pin a commit in `Cargo.toml`,
replace `branch = "cbd-chain-source"` with `rev = "<commit>"`.

The node it talks to is a separate build. The one this dashboard was made for is ldk-server branch
`cbd-chain-source` with ldk-node branch `cbf-stats` and kyoto branch `mutinynet-stats` (all in the
`randomlogin` forks); the version footer shows what a running node was built from.
