# rmcloud — self-hosted reMarkable 2 cloud service (server side)

**English** | [中文](README.zh-CN.md)

Used together with the device-side `rmdev`: the device syncs documents / ink strokes /
thumbnails up, and the server handles storage, rendering, OCR, AI Q&A and web browsing.
Single-user by design, binds only the Tailscale address by default.

- Protocol details that line up with the device-side implementation: [`../docs/en/CLOUD.md`](../docs/en/CLOUD.md)

## Module layout

| File | Contents |
|---|---|
| `src/main.rs`, `src/config.rs` | assembly, configuration |
| `src/state.rs` | `App`, database tables, rev, token validation, `call_device` |
| `src/pair.rs` | `POST /api/v1/pair` + the `rmcloud pair` subcommand |
| `src/sync.rs` | manifest / blob / commit / tree / thumb |
| `src/events.rs` | `GET /api/v1/events` (SSE), `POST /api/v1/device/result` |
| `src/agent_api.rs` | `POST /api/v1/agent/ask` (streaming) |
| `src/tex.rs` | `GET /api/v1/tex?f=<latex>&dpi=2` |
| `src/mail.rs`, `src/ocr.rs`, `src/usage.rs` | sending mail, text conversion, cost |
| `src/web.rs` | web pages (library browsing / archive / usage) |

Each module exposes exactly one `pub fn routes() -> Router<Arc<App>>`, merged by `main.rs`.

## Design constraints

1. **The device-side `net.rs` is the protocol authority.** Field names, status codes and
   error strings must match it word for word (in particular, on a `commit` conflict the
   `error` field must contain `rev_changed`; every response carries a `Content-Length`;
   `blob` is idempotent).
2. Except for `/api/v1/health` and `/api/v1/pair`, **every endpoint** must validate
   `Authorization: Bearer <token>` and return 401 on failure.
3. Paths are always **relative** and must not contain `..` (`entry.path`, `/blob/{sha}`,
   `/thumb/...`).
4. No TLS, no binding to `0.0.0.0`, no reverse proxy — the network boundary is left to
   Tailscale.
5. The database is `rusqlite` (bundled); the tables are created in `state.rs`. To add a
   table, append a `CREATE TABLE` — **add only, never modify**.
6. Never hardcode secrets / tokens in the code; configuration is read from `App::cfg`
   (see `src/config.rs`).
7. Every module gets a matching set of integration tests in `tests/<module>.rs`: start a
   real port with `axum::serve` and hand-write the HTTP requests (no reqwest). The minimum
   bar is that `cargo test` passes.

## Configuration

Reads `/etc/rmcloud.json` by default (override with `RMCLOUD_CONFIG`); missing fields fall
back to their defaults. These are the items you are likely to change:

```json
{
  "bind": "100.64.0.1:8787",
  "local_bind": "127.0.0.1:8787",
  "state": "/srv/rm",
  "dsh": "dsh",
  "dsh_env": "~/.config/rmcloud/web.env"
}
```

`token` is written back automatically by `rmcloud pair`; you do not fill it in by hand.
SMTP is optional; its credentials belong only in this file, with `chmod 600`, and **must
not be committed to the repository**.

## Build and deploy

```sh
cargo build --release          # make sure it builds locally first
cargo test
# deploy: copy the binary and the service unit to the server, then systemctl enable --now rmcloud
```

`rmcloud.service` is a reference systemd unit; adjust the paths to your own deployment.

## External services

- **Tailscale**: the device and the server share one tailnet, and the server only binds an
  address inside `100.64.0.0/10`.
- **dsh** (optional): AI Q&A goes through
  `dsh --profile headless --json --session-id <id> "<prompt>"`, which emits NDJSON
  (`type=text` carries the incremental output). The API key lives in the environment file
  pointed to by `dsh_env`; the server only reads it, never writes it.
- **SMTP / IMAP** (optional): sending and receiving mail are off by default; turn them on
  in the configuration when you need them.
