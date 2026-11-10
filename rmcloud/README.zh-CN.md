# rmcloud —— reMarkable 2 自建云服务（服务端）

[English](README.md) | **中文**

配合设备端 `rmdev` 使用：设备把文档 / 笔迹 / 缩略图同步上来，服务端负责存储、
渲染、OCR、AI 问答和网页浏览。单用户设计，默认只绑 Tailscale 地址。

- 协议与设备端实现对得上的细节：`../docs/CLOUD.md`
- 需求与接口清单：`../docs/SERVER.md`

## 模块划分

| 文件 | 内容 |
|---|---|
| `src/main.rs`、`src/config.rs` | 装配、配置 |
| `src/state.rs` | `App`、库表、rev、token 校验、`call_device` |
| `src/pair.rs` | `POST /api/v1/pair` + `rmcloud pair` 子命令 |
| `src/sync.rs` | manifest / blob / commit / tree / thumb |
| `src/events.rs` | `GET /api/v1/events`（SSE）、`POST /api/v1/device/result` |
| `src/agent_api.rs` | `POST /api/v1/agent/ask`（流式） |
| `src/tex.rs` | `GET /api/v1/tex?f=<latex>&dpi=2` |
| `src/mail.rs`、`src/ocr.rs`、`src/usage.rs` | 发信、转文字、成本 |
| `src/web.rs` | 网页（库浏览 / 档案 / 用量） |

每个模块只暴露一个 `pub fn routes() -> Router<Arc<App>>`，由 `main.rs` `merge`。

## 设计约束

1. **设备端 `net.rs` 是协议权威**。字段名、状态码、错误串必须逐字对齐
   （尤其 `commit` 冲突时 `error` 里要有 `rev_changed`；每个响应都有 `Content-Length`；
   `blob` 幂等）。
2. 除 `/api/v1/health` 和 `/api/v1/pair`，**所有接口**都要校验
   `Authorization: Bearer <token>`，失败回 401。
3. 路径一律**相对**且不许 `..`（`entry.path`、`/blob/{sha}`、`/thumb/...`）。
4. 不引入 TLS、不监听 `0.0.0.0`、不装反向代理——网络边界交给 Tailscale。
5. 数据库用 `rusqlite`（bundled），表在 `state.rs` 建好；加表就追加 `CREATE TABLE`，
   **只加不改**。
6. 代码里不写死密钥 / token，配置从 `App::cfg` 读（见 `src/config.rs`）。
7. 每个模块配一组 `tests/<模块>.rs` 集成测试：用 `axum::serve` 起真端口，
   手写 HTTP 请求（不引入 reqwest）。最低要求 `cargo test` 能跑通。

## 配置

默认读 `/etc/rmcloud.json`（可用 `RMCLOUD_CONFIG` 覆盖），缺字段走默认值。
需要自己改的是这几项：

```json
{
  "bind": "100.64.0.1:8787",
  "local_bind": "127.0.0.1:8787",
  "state": "/srv/rm",
  "dsh": "dsh",
  "dsh_env": "~/.config/rmcloud/web.env"
}
```

`token` 由 `rmcloud pair` 自动写回，不用手填。SMTP 是可选项，凭据只放这个文件，
`chmod 600`，**不要提交到仓库**。

## 构建与部署

```sh
cargo build --release          # 本机先编过
cargo test
# 部署：把二进制和服务单元丢到服务器，systemctl enable --now rmcloud
```

`rmcloud.service` 是参考的 systemd 单元，路径按自己的部署改。

## 依赖的外部服务

- **Tailscale**：设备与服务端在同一个 tailnet 里，服务端只绑 `100.64.0.0/10` 内的地址。
- **dsh**（可选）：AI 问答走 `dsh --profile headless --json --session-id <id> "<prompt>"`，
  输出 NDJSON（`type=text` 是增量）。API key 放在 `dsh_env` 指向的环境文件里，
  服务端只读不写。
- **SMTP / IMAP**（可选）：发信、收信默认关闭，要用再在配置里打开。
