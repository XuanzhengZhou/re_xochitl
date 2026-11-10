# CLOUD.md —— 自建云服务施工图

> 把官方 **Connect 订阅云**换成自己一台服务器。本文四部分：官方有什么（带证据）、官方协议能挖到什么、
> 我们的服务端怎么设计（可直接照着实现）、设备端还要改哪里。
>
> 证据约定：`strings xochitl` = 对从设备取出的 `xochitl`（8 MB ARM 二进制）做字符串扫描；
> `res_xxxxx.qml:N` = carve 出来的 QML 树里的行号（QML 树未随仓库分发）；
> `net.rs:N` / `sync.rs:N` = `../rmdev/src/` 的既有设备端代码。**没有证据的一律标「推测」。**

---

## 0. 结论速览

1. 官方云 = **按文档（zip）+ JSON 清单**同步的存储服务，不是细粒度协议；端点全在
   `/document-storage/json/2/*`、`/token/json/2/*`、`/convert/v1/handwriting`（`strings xochitl`）。
2. 设备端**已经能跑**自建协议（`net.rs` 4 个接口 + `sync.rs` 三路比较），服务器照 §3.2 实现即可对接。
3. **不做 TLS**：单人自用，设备和服务器都在同一个 Tailscale 网里，明文 HTTP 够（加密由 WireGuard 给），
   而 `net.rs` 本来就只认 `http://`——正好不用改。见 §3.6。
4. **不跑 OCR 模型**：手写转文字直接让服务器上的 `dsh` 读图（`read_image`），见 §3.5。
5. 我们比官方多做的只有一样：Web UI 直接看图。版本历史、软件更新、三方集成**不做**。

---

## 1. 官方云到底提供什么

| # | 能力 | 官方实现要点 | 证据 | 我们的替代 |
|---|---|---|---|---|
| 1 | **多设备同步** | 文档级 zip 上传/下载 + 清单 rev | `res_065dcf1.qml:64`「Sync files between your paper tablet, mobile and desktop」；`DocumentSync`、`documentsync_1_5.cpp`（`strings xochitl`） | §3.2 manifest/blob/commit |
| 2 | **云端备份** | 同上存储，配「备份」话术 + 50 天保留策略 | `res_065dcf1.qml:65`「Back up your work」；「Notes will automatically back up in the cloud and sync to the apps for 50 days.」（`strings xochitl`） | 同一套 blob 存储，无保留期（§3.3） |
| 3 | **版本历史** | 官方 Connect 的「file version history」，只在网页/App 侧 | **设备端零证据**：QML 里 `History` 只出现在 GPL 正文（`res_062c955.qml`），无版本历史 UI ⇒ **推测** | **不做**（你不需要） |
| 4 | **手写转文字** | 上传墨迹 → 云 OCR（MyScript iink）→ 回页 | `res_065dcf1.qml:67`「Convert handwritten notes into typed text」；`/convert/v1/handwriting`、`Recognizer`/`RecognitionJob`、`application/vnd.myscript.jiix`、MyScript 授权声明（`strings xochitl`） | §3.5 让服务器上的 `dsh` 读图（不跑 OCR 模型） |
| 5 | **PDF 转文字** | **设备端明确不支持**：转换只对笔记页开放 | `res_06678c9.qml:20`「You can only convert note pages to text」；PDF 只有**本地**文字层搜索（`res_0666bce.qml:435`「Search in PDFs, EPUBs, or your typed text」）⇒ 云端 PDF→文本 **推测/非官方** | **不做**（设备端已能本地搜 PDF 文字层） |
| 6 | **发邮件** | 页面渲染成 PDF/PNG → 云代发，带官方签名，不可回复 | `res_065dcf1.qml:66`「Share files by email」；`res_067e8cc.qml`「Send by mail」；`rm.network.send.mail`、`/sendmail_`、签名串「Sent from my reMarkable paper tablet... PS: You cannot reply to this email」（`strings xochitl`） | §3.2 `POST /api/v1/mail` |
| 7 | **手机/桌面端查看** | `my.remarkable.com` web app + 移动/桌面 App | `frag_0038_0065ac0e.qml:48`「download the mobile and desktop apps from %1」；`/webapp` + `*.my.remarkable.com`（`strings xochitl`）；`res_068b63a.qml:188` `NetworkClient.loginUrl` | §3.2 Web UI（只读浏览 + 下载） |
| 8 | **屏幕共享（LiveView）** | v1 = RFB/VNC 直连；v2 = WebRTC + MQTT 信令 | `frag_0081_006bd97e.qml:8`「Present with Screen Share」；`res_067887b.qml:2390`「Accept connection in the desktop app...」；`RFB 003.008`、`src/screenshare/src/*`、`WebRtc::Manager`、`vernemq-%1.cloud.remarkable.engineering`（`strings xochitl`） | 已有 `rmdev/src/share.rs`（BMP over HTTP，8000-8009），**不需要服务器** |
| 9 | **文件上传（网页/邮件到设备）** | 桌面/移动 App 上传 PDF；设备本机还有 USB web 上传（`http://10.11.99.1`） | `res_06a0e55.qml:292`「You can import documents as PDF files through the desktop or mobile app」；`rm.usb.web` + `POST /upload`（`strings xochitl`）；`res_0686a15.qml:130` 给出 10.11.99.1。**「邮件发到设备」无任何证据 ⇒ 推测** | §3.2 `POST /api/v1/import` |
| 10 | **软件更新分发** | 独立进程 `update-engine.service`，xochitl 只通过 D-Bus 触发 | `update-engine.service`、D-Bus 名 `no.remarkable.update1`、对象路径 `/no/remarkable/update1`（`strings xochitl`）；`res_0698460.qml:165`「Check for updates」；Beta 计划 `res_0698460.qml` 内文 | **不做**（设备端也没实现） |
| 11 | **账号与订阅** | 一次性配对码（8 位）绑账号；Connect 订阅；无订阅 50 天不活跃即停云 | `frag_0026_0065294a.qml:26` `maximumLength: 8`；`res_066a452.qml:43`「over 50 days, which is the cloud limit without a Connect subscription」；`res_0686a15.qml:265-267`「Sync without Connect」；`res_068b63a.qml:177`「Enjoy unlimited access to all your notes...」 | **全部砍掉**，只有设备 token（§3.6） |
| 12 | **云存储配额** | 订阅版宣称「unlimited」 | `res_068b63a.qml:177`「Enjoy unlimited access to all your notes and documents in the reMarkable cloud」。具体配额数字**无证据**；`storageAvailable/storageUsed/storageTotal`（`strings xochitl`）更像第三方网盘集成，**推测**不是 rM 云 | 不管配额，磁盘满时 507 |

**负事实（同样重要）**：
- 设备端 QML 里**没有**「版本历史」「云端回收站」页面 → 这两项是 Web/App 侧功能，别指望设备驱动。
- 屏幕共享**不走** `/document-storage`，是独立的 MQTT + RFB/WebRTC 通道，我们不复刻。

---

## 2. 官方协议的事实

### 2.1 环境与主机名

```
dev.internal.cloud.remarkable.com        服务端（存储/令牌/转换）
qa.internal.cloud.remarkable.com
stage.internal.cloud.remarkable.com
development.my.remarkable.com            web app / 账号侧
qa.my.remarkable.com
staging.my.remarkable.com
service-manager-…-dot-remarkable-…appspot.com    服务发现（App Engine）
vernemq-%1.cloud.remarkable.engineering          MQTT broker（屏幕共享信令）
```
（全部来自 `strings xochitl`。）

- **生产环境的完整主机名挖不到**：字符串里只有 dev/qa/stage 三套；生产应该是
  `internal.cloud.remarkable.com`（无前缀），但没有直接字符串能证明 ⇒ 推测。
- 环境切换走 `xochitl set-environment`（`set-environment` / `Environment set to` / `Unknown service environment:`）。
- **服务发现**：设备先向服务管理器要某个服务的 hostname，再发业务请求。
  证据：`/service/json/`、`/document-storage`（相邻字符串）、`Fetching document storage hostname`、
  `Failed to fetch storage hostname`、`ServiceHostname::fetchHostname`、QML 模块 `rm.network.hostnames`。
  `/service/json/` 后面的版本号/响应格式**挖不到**。

### 2.2 配对与令牌

| 步骤 | 请求 | 证据 |
|---|---|---|
| 一次性码 | 用户在设备输入 8 位码（虚线固定宽输入框） | `frag_0026_0065294a.qml:26` `maximumLength: 8` |
| 换设备令牌 | `POST /token/json/2/device/new`，body 字段 `code`、`deviceDesc`、`deviceID`、`secret` | 四个字段名与端点相邻（`strings xochitl`） |
| 设备令牌形态 | JWT：`device token has {} .-separated chunks`，内含 `auth0-userid` 声明 | `strings xochitl` |
| 换用户令牌 | `POST /token/json/2/user/new`，带设备令牌 | 同上 |
| 请求鉴权 | `Authorization: Bearer <user token>`（`Bearer ` 常量独立出现） | 同上 |
| 令牌失效 | 401 → 重取 user token；403 → 直接报错；`token error 401/403` | 同上 |
| 码错了 | `deviceTokenErrorCode === 409`；文案 `Invalid One-time-code` | `res_068a5d6.qml` / `strings xochitl` |

`secret` 的算法**挖不到**。第三方 `rmapi` 用 `sha256(deviceID)`，但那不是本机证据 ⇒ 推测。
解绑设备是 `POST /token/json/3/device/delete`；换令牌成功后设备会写 `got new usertoken: {}` 日志。

### 2.3 文档存储 API（这是官方同步的实质）

| 方法 | 路径 | 作用 |
|---|---|---|
| GET | `/document-storage/json/2/docs` | 拉全量文档清单 |
| POST | `/document-storage/json/2/upload/request` | 申请上传 URL（响应含 `BlobURLPut`） |
| POST | `/document-storage/json/2/delete` | 删文档 |
| GET | `BlobURLGet`（清单里给的绝对 URL） | 下载文档 zip |

清单条目的字段名（全部来自 `strings xochitl` 的相邻字符串）：
`ID`、`VissibleName`（**官方拼写错误，两个 i**）、`Parent`、`Type`、`Version`、`ModifiedClient`、
`Bookmarked`、`Pinned`、`Deleted`、`lastModified`、`hash`、`size`、`broadcast`、`generation`、`children`。
- 小写别名也存在：`vissibleName`、`parent`、`bookmarked`（同一段字符串）。
- 时间格式常量：`yyyy-MM-ddTHH:mm:ss.zzZ`（ISO-8601 毫秒 + Z）。
- 清单是**数组**（`entries in reply`、`Received list entry with no ID`、`Reply contains entry without id`），
  不是 `{entries:[...]}` 包装 ⇒ 由这些字符串推断。
- `Type` 只有两种合法值（`DocumentType`/`CollectionType`），服务端不认识就报
  `we did not get valid type from the server`。

**传输单位是整篇文档的 zip**：`.zip`、`.part`、`.downloading`、`.uploading`、`Could not unpack`、
`Downloaded document was corrupt.`、`Failed to pack`。zip 内文件名模板：`%1.rm`（页墨迹）、
`%1-metadata.json`（文档元数据，均在 `DocumentUploader::PackFileConfig` 附近）；另有一组 payload
列表 `.metadata`/`.content`/`.pagedata`（`documentsync_1_5.cpp` 附近，日志 `got payload file %s`）。
- 「只改元数据就不传文件」：`Only metadata modified, skipping file upload`；下载先落 `.part` 再解包
  （`Error during download of file`、`Not enough free space to download`）。

### 2.4 同步协议版本与通知

- 两个实现 + 一个包装：`DocumentSync_1_0`、`DocumentSync_1_5`、`DocumentSyncProtocolWrapper`
  （`documentsync_1_5.cpp`、`documentsyncprotocolwrapper.cpp`）。
- 切换是运行时协商的：`Requesting sync protocol:`、`current is`、`now switching sync protocol...`、
  `Sync 1.5 is up and running`、`set protocolFlag to %d`、`Sync::ProtocolFlag`、`requestProtocol`。
  **协议号是 `1.0` / `1.5`，没有 `sync15`/`sync31` 这种字符串。**
- 1.5 的语义：`full-sync` / `entry-sync` 两种模式，按 `.metadata`/`.content`/`.pagedata` 逐文件对账，
  冲突走 `updateConflictingFiles` + `processSyncConflict`（「have {} conflicts in {:p}」）→
  **官方本身就是「文件级 + 冲突留副本」**，和 `sync.rs` 同构。周期常量
  `SYNC_ACTIVE_PERIOD_MS`/`SYNC_REFRESH_INACTIVE_MS`，`generation` 单调计数。
- 通知：MQTT（paho，`src/mqtt/src/pahomqtt/client.cpp`），服务名 `/notifications`，日志
  `Connecting to MQTT broker...`。topic 名、端口、是否走 WebSocket **挖不到**。

### 2.5 手写转文字

- 端点：`POST /convert/v1/handwriting`（`strings xochitl`，在 `src/handwritingconversion/src/recognizer.cpp` 附近）。
- 请求是 multipart：`QHttpMultiPart` + `QHttpPart`（`strings xochitl` 的 Qt 符号）。
- 响应类型：`application/vnd.myscript.jiix`（MyScript iink 的 JIIX 格式）。
- 引擎是 MyScript 授权（`a registered trademark of MyScript. Our handwriting conversion application
  includes software licensed by MyScript.`）；支持 **33 种语言**（`hwcInfoString`，`frag_0081_006bd97e.qml`）。
- 设备端把哪些页送上去由 `hwcanalyzer` 决定（`src/handwritingconversion/src/hwcanalyzer.cpp`），
  识别结果 `RecognitionJob::applyFormattedText` 落成 **新页**（`res_066b1b0.qml:79`「Converted text added as new page」）。
- 请求体的具体字段（stylus 坐标？SVG？PNG？）**挖不到**。二进制里有
  `image xlink:href="data:image/png;base64,%5"` 这种 SVG 内嵌 PNG 模板，但是 SVG 导出用的，不能证明是 OCR 载荷。

### 2.6 邮件

- `rm.network.send.mail`、端点字符串 `/sendmail_`、邮箱正则
  `\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b`、签名 HTML
  `Sent from my reMarkable paper tablet<br>Get yours at www.remarkable.com<br><br>PS: You cannot reply to this email<br>`。
- 请求体字段、是否 multipart、附件格式（PDF/PNG）**挖不到**。

### 2.7 屏幕共享

- v1：**真 VNC**。协议握手串 `RFB 003.008`，实现 `src/screenshare/src/compat/rfbserverprotocol.cpp` 与
  `rfbclientprotocol.cpp`，日志 `RfbScreenShareServer started listening on port`。
- v2：WebRTC（`src/webrtc/src/webrtc.cpp`，`WebRtc::Manager::createPeerConnection(QString,QString)`，
  `Logical error while sending data on WebRTC datachannel`）+ MQTT 信令
  （`ScreenShareClient::initMqtt`，broker 模板 `vernemq-%1.cloud.remarkable.engineering`）。
- 对端是桌面 App（`Accept connection in the desktop app...`），不是浏览器。

### 2.8 软件更新

- `update-engine.service` + D-Bus `no.remarkable.update1` / `/no/remarkable/update1`；
  xochitl 只负责「启停 update-engine」「读状态」（`update-engine status:`、`Failed to stop update-engine`）。
- 更新 URL / 版本清单格式在 `update-engine` 里，**本机没分析那个二进制** ⇒ 挖不到。
  （常说的 `updates.cloud.remarkable.com` 在 xochitl 里没有字符串，不要当事实写。）

### 2.9 明确挖不到的（不要编）

1. 生产环境主机名的完整拼法（只有 dev/qa/stage 三套）、`/service/json/` 的版本号与响应 JSON。
2. `/sendmail_` 与 `/convert/v1/handwriting` 的请求体字段（只知道后者是 multipart、回 JIIX）。
3. MQTT 的 topic 命名、端口、鉴权（只看得到 paho + broker 主机模板）。
4. 清单里 `hash` 的算法（推测是 zip 的 sha256，无证据）、`secret` 的算法（推测 `sha256(deviceID)`）。
5. 更新清单的 URL 与格式（在 update-engine 里）、版本历史/回收站接口（设备端无任何痕迹）。

---

## 3. 我们的服务端设计

### 3.1 部署形态（单用户 + Tailscale，不要反向代理）

```
rM2 (net.rs) ──HTTP/1.1 over Tailscale──▶ rmcloud :8080（只绑 100.64.0.1）
                                            ├─ /api/v1/*   配对 / 同步 / 读图 / 邮件
                                            └─ /            Web UI
```

| 项 | 选择 | 理由 |
|---|---|---|
| 主进程 | **Rust 单二进制** `rmcloud`（axum + tokio）或 **Python 3.12 + FastAPI**（服务器已装 3.12.3） | 设备真会调的接口只有 6 个，哪个顺手用哪个 |
| 反向代理 | **不要** | Tailscale 已经加密，Caddy/Nginx 只会多一层故障和证书麻烦 |
| 端口 | 8080，只绑 `100.64.0.1` | 公网网卡上一个口都不开 |
| 目录 | `/srv/rm`（0700，属主 `rmcloud`）、配置 `/etc/rmcloud.toml` | 见 §3.3 |
| systemd | `rmcloud.service` **一个** unit，`Restart=always` | 没有 OCR worker 了（§3.5） |
| 依赖 | 无数据库、无 Redis、无 Docker | 状态就是一个 `state.json` + 目录 |

```ini
# /etc/systemd/system/rmcloud.service
[Unit]
Description=reMarkable self-hosted cloud
After=network-online.target tailscaled.service
Wants=tailscaled.service

[Service]
User=rmcloud
ExecStart=/usr/local/bin/rmcloud --config /etc/rmcloud.toml
Restart=always
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/srv/rm

[Install]
WantedBy=multi-user.target
```

> 服务器实测：没有 nginx / caddy / docker，也别装。`8080` 在本机已有进程占用（127.0.0.1），
> 我们绑 Tailscale IP 的 8080 不冲突；真撞了换 8090。

### 3.2 接口契约（**与 `net.rs` 逐字节对齐**，新增接口单独标注）

统一：JSON 用 UTF-8；时间戳**毫秒整数**；设备接口鉴权 `Authorization: Bearer <token>`；
出错一律 `{"error":"<snake_case>"}`；响应必须带 `Content-Length` 或 `Transfer-Encoding: chunked`，
并且**允许 `Connection: close`**（`net.rs:143` 每请求都 close）。

| # | 方法 | 路径 | 鉴权 | 谁在用 |
|---|---|---|---|---|
| 1 | POST | `/api/v1/pair` | 无 | 配对（`net.rs:315`） |
| 2 | GET | `/api/v1/health` | 无 | 探活（`net.rs:357`） |
| 3 | GET | `/api/v1/manifest` | Bearer | 拉清单（`net.rs:363`） |
| 4 | POST | `/api/v1/blob` | Bearer + `X-Sha256` | 传文件（`net.rs:372`） |
| 5 | GET | `/api/v1/blob/{sha256}` | Bearer | 取文件（`net.rs:405`） |
| 6 | POST | `/api/v1/commit` | Bearer | 提交改动（`net.rs:422`） |
| 7 | POST | `/api/v1/ocr` | Bearer | 转文字 → 由 dsh 读图（`net.rs:450`，§3.5） |
| 8 | POST | `/api/v1/mail` | Bearer | 发邮件（`net.rs:466`） |
| 9 | GET | `/api/v1/tree` | Bearer | **新增**：Web UI 文档树 |
| 10 | GET | `/api/v1/thumb/{uuid}/{page}.png` | Bearer | **新增**：Web UI 缩略图 |
| 11 | GET | `/api/v1/download/{uuid}` | Bearer | **新增**：原始 PDF/EPUB/打包下载 |
| 12 | POST | `/api/v1/import` | Bearer | **新增**：PDF/EPUB 推进设备库 |
| ~~13~~ | ~~GET~~ | ~~`/api/v1/update/latest`~~ | — | **不做**（软件更新砍掉） |
| ~~14~~ | ~~GET~~ | ~~`/dl/{name}`~~ | — | **不做** |

#### 1) 配对

```
POST /api/v1/pair
{"code":"12345678","device_name":"reMarkable 2","protocol":1}
200 {"device_id":"d-3f2a9c11","token":"<64 hex>","server_time":1700000000000}
401 {"error":"bad_code"}          // 码错/过期/已用
429 {"error":"too_many_attempts"} // 同一 IP 连续失败
```
- 配对码用 `rmcloud pair` 生成：8 位十进制、TTL 10 分钟、**用一次作废**，打印到 stdout。
- `protocol` 服务端只做记录（当前客户端固定发 `1`）。
- `device_name` 可空；服务端按 `<device_id>` 建目录。
- **不走用户系统**：没有注册、没有密码、没有邮箱验证。

#### 2) 健康检查

```
GET /api/v1/health
200 {"ok":true,"rev":1234,"devices":2}
```
不带鉴权（`net.rs:357` 不传 token），所以**不能**在里面暴露路径信息。

#### 3) 清单

```
GET /api/v1/manifest
200 {
  "rev": 1234,
  "entries": [
    {"path":"a1b2.metadata","sha256":"<64 hex>","size":412,"mtime":1700000000000},
    {"path":"a1b2/p1.rm","sha256":"<64 hex>","size":743688,"mtime":1700000000000},
    {"path":"a1b2.thumbnails/p1.png","sha256":"<64 hex>","size":9120,"mtime":1700000000000},
    {"path":"c3d4.metadata","deleted":true,"mtime":1699999999000}
  ]
}
```
服务端规则：
- 一条记录要么有 `sha256`+`size`+`mtime`，要么 `deleted:true`+`mtime`。**不要**两种字段混发。
- `path` 必须是相对路径，不能以 `/` 开头、不能含 `..` 或空段（客户端会整条丢弃，`sync.rs:320`）。
- 路径里出现 `.local` 段或 `.tmp` 后缀的条目客户端直接跳过（`sync.rs:314`）——服务器干脆别存。
- 全量返回，不做 `?since=`（`net.rs` 不传；官方清单本来也是全量）。
- 优先带 `Content-Length`（客户端也认 chunked，但无论如何会把整个 body 读进内存，1 GB 设备经不起大库）。

#### 4) 传文件 / 5) 取文件

```
POST /api/v1/blob
Authorization: Bearer <t>
X-Sha256: <64 hex>
Content-Type: application/octet-stream
Content-Length: 743688
<原始字节>
204                                    // 成功（已存在也 204，天然幂等）
400 {"error":"sha_mismatch"}           // 头里的 sha 与 body 实算不符
413 {"error":"too_large"}              // > 64 MB
```
```
GET /api/v1/blob/{sha256}
200 <原始字节>  Content-Type: application/octet-stream
404 {"error":"no_blob"}
```
- 服务端**必须**重算 sha256 校验（设备按 `X-Sha256` 命名，不校验就可能存错内容）。
- 服务端上限 64 MB；客户端自己 32 MB 就拒了（`sync.rs:22`），永远撞不到 413。
- `Range` 不做。设备一次读全量。

#### 6) 提交改动

```
POST /api/v1/commit
{"base_rev":1234,
 "puts":[{"path":"a1b2.metadata","sha256":"<64 hex>","mtime":1700000000000}],
 "dels":[{"path":"c3d4.metadata","mtime":1700000000500}]}
200 {"rev":1235,"conflicts":[]}
200 {"rev":1235,"conflicts":["a1b2.metadata"]}   // 服务器认为自己的更新，拒绝了这条 put
409 {"error":"rev_changed","rev":1240}
```
- `conflicts` 是 **path 字符串数组**（`net.rs:440-445` 按字符串解析）。不要放对象。
- 409 的 `error` 字段必须**字面包含** `rev_changed`：客户端用 `e.contains("rev_changed")` 判断
  （`sync.rs:202`），返回 `"rev_conflict"` 就走不进重试分支。
- 一条 commit 里 `puts`/`dels` 不能出现同一个 path（`400 {"error":"dup_path"}`）。
- `rev` 只在 commit 成功时 +1；manifest 不改 rev。
- 空 `puts`+空 `dels` 也允许，返回当前 rev，不加。

#### 7) OCR

```
POST /api/v1/ocr
{"image_blob":"<64 hex>","mode":"handwriting","lang":"en"}

200 {"text":"line one\nline two",
     "lines":[{"text":"line one","bbox":[40,100,900,140],"conf":0.93}],
     "engine":"dsh-read"}
503 {"error":"engine_busy"}
422 {"error":"no_ink"}          // 整页空白
404 {"error":"no_blob"}         // image_blob 没先上传
```
- **客户端只发三个字段**（`net.rs:452`）：`image_blob`、`mode`、`lang`。
  `page_width`/`page_height`/`dpi` 都**不要做成必填**；旧文档里写了，但客户端没发。
- `mode` ∈ `handwriting` | `printed`；`lang` ∈ `en` | `zh` | `auto`（未知值按 `auto`）。
- 客户端只读 `text`（`net.rs:462`），`lines` 给 Web UI 和以后做「可点选文字」用。
- 超时 120 s（`net.rs:456`）。超了就断开，客户端报 `read failed`。

#### 8) 邮件

```
POST /api/v1/mail
{"to":["a@b.c"],"subject":"Notes","body":"...",
 "attachments":[{"blob":"<64 hex>","filename":"page-3.pdf","mime":"application/pdf"}]}

200 {"ok":true}
400 {"error":"bad_address"}
413 {"error":"too_large"}
502 {"error":"smtp_failed","detail":"550 5.7.1 ..."}   // detail 原样回给设备，便于排查
```
- 附件同样先走 `POST /api/v1/blob`，这里只传引用（避免 base64 膨胀 4/3）。
- 服务端用 `msmtp`/`lettre` 发；发件人固定，正文尾部不要加官方那行签名（那是官方的）。
- 附件 ≤ 20 MB，最多 5 个。

#### 9–11) Web UI 读接口（新增，设备不调用）

```
GET /api/v1/tree
200 {"rev":1234,
     "docs":[{"uuid":"a1b2","name":"Meeting","type":"DocumentType","parent":"",
              "deleted":false,"pages":3,"modified":1700000000000,
              "has_pdf":false,"thumb":"a1b2.thumbnails/p1.png"}]}
```
- 服务端**自己解析** `.metadata`/`.content`（格式见附录 A），不是让设备算。
- `GET /api/v1/thumb/{uuid}/{page}.png`：直接吐设备上传的 `<uuid>.thumbnails/<page>.png`
  （`sync.rs:333` 已把 `.thumbnails` 纳入同步，**服务器不需要渲染 `.rm`**）；缺失回 404 让 Web UI 画灰块。
- `GET /api/v1/download/{uuid}`：有 `<uuid>.pdf`/`<uuid>.epub` 吐原文件；否则把
  `<uuid>.metadata`+`content`+`pagedata`+`<uuid>/*.rm` 打成 zip。

#### 12) 文档导入（新增）

```
POST /api/v1/import
Content-Type: multipart/form-data; boundary=...
file=@book.pdf        # 文件部分，filename 即 visibleName
parent=<uuid>        # 可选表单字段，缺省根目录

200 {"uuid":"e5f6","pages":128,"rev":1235}
415 {"error":"unsupported_type"}     // 只收 application/pdf、application/epub+zip
413 {"error":"too_large"}            // 单文件 ≤ 200 MB
```
服务端做的事（**等同一次普通 commit**，设备下次同步自动拉下来）：
1. `uuid = uuid4`；上传字节按 `<uuid>.pdf` / `<uuid>.epub` 存 blob；`pdfinfo`/epub 解析页数 N。
2. 生成 `<uuid>.metadata`（`type:"DocumentType"`、`parent`、`visibleName`=文件名、`lastModified`=now、
   `deleted:false`、`version:1`）和 `<uuid>.pagedata`（N 行 `Blank`）。
3. 生成 `<uuid>.content`：`fileType:"pdf"`（或 `"epub"`）、`cPages.pages[]`、`pageCount:N`、
   `sizeInBytes:"<字符串>"`、PDF 还要 `redirectionPageMap`。
4. **不生成 `.rm`**（附录 A：没有 `.rm` 的页就是空白页，设备照样显示 PDF 页）。
5. 事务里写进 `state.entries`，`rev += 1`。

#### 13–14) 更新分发

```
<!-- 更新分发不做：设备端没有 update_check()，也用不到。 -->

/srv/rm/                       0700 root:rmcloud
├── state.json                 {"rev":1235,"entries":{ "<path>": {"sha256","size","mtime"} | {"deleted":true,"mtime"} }}
├── devices.json               {"d-3f2a9c11":{"token_sha256":"...","name":"reMarkable 2","paired_at":...,"last_seen":...}}
├── blobs/ab/ab12…（64 hex）   内容寻址，原样字节，永不删除（版本历史要用）
├── tmp/                       上传落盘区（同分区，保证 rename 原子）
├── ocr/<sha256>.json          OCR 结果缓存（同图同 lang 直接回）
└── web/                       Web UI 静态文件（也可以编译进二进制）
```

- **内容寻址**：`sha256(file bytes)`，前 2 位分桶。同一个 blob 多路径引用是常态
  （冲突副本、版本历史），别做「一个 blob 一个路径」。
- **tombstone**：`dels` 只把 entry 换成 `{"deleted":true,"mtime":...}`，**不删 blob**。
  真删由 `rmcloud gc`（人工/定时，保留最近 90 天被引用的 blob）执行。
- **版本历史不做**：commit 只写 `state.json`，不保留历史。要回滚只能靠备份。


### 3.4 冲突策略：设备优先，冲突留副本

**权威语义在 `sync.rs:242` 的 `plan()`**，服务器只做「记账 + 拒绝覆盖」，不参与决策：

| 本地 | 服务器 | 基线 | 设备动作（`plan()`） | 服务器看到什么 |
|---|---|---|---|---|
| 无 | 有 | 有且 = 服务器 | `DelRemote` | commit 里一条 `dels` |
| 无 | 有 | 无 / 服务器更新 | `Download` | 只 GET blob |
| 有 | 无 | 有且 = 本地 | `DelLocal` | 什么都不发 |
| 有 | 无 | 无 / 本地改过 | `Upload` | `POST blob` + put |
| 有 | 有 | 相同 sha | `Nothing` | 无 |
| 有 | 有 | 基线 = 服务器 | `Download` | 覆盖本地 |
| 有 | 有 | 基线 = 本地 | `Upload` | put |
| 有 | 有 | 三边全不同 / 无基线 | **`Conflict`** | 先 GET 服务器副本 → 写 `<rel>.server-conflict-<UTC>` → 再 put |

服务器必须保证的四条：

1. **rev 闸门**：`base_rev != state.rev` → 409。这是唯一的并发串行化机制，
   客户端收到会重拉清单、重算、重试一次（`sync.rs:55-72`）；连续两次 409 就报错放弃。
2. **mtime 只用来挡「回退写入」**：`puts` 里若存在**活跃** entry 且 `entry.mtime > put.mtime`，
   不改 entry、把 path 放进 `conflicts` 返回。客户端会把该 path 从基线剔除并报
   `server kept its newer copy`（`sync.rs:197-200`）。
3. **put 优先于 tombstone**：`entries[path]` 是墓碑时，`put` 直接覆盖（复活），**不**比 mtime。
   理由：墓碑的 mtime 是「删除时刻」，永远比被复活文件的真实 mtime 新，一比就永久拒绝复活，
   而三路比较已经在设备侧判过「该复活」。rev 闸门已经保证这条 put 是基于最新状态算出来的。
4. **服务器永不生成冲突副本**：副本是设备本地文件（`sync.rs:442`
   `<原路径>.server-conflict-<yyyymmdd-HHMMSS>`，UTC），它随后作为**普通新文件**同步上来。
   Web UI 要把这类文件显示出来，别当成脏数据过滤掉。

补充：同一 commit 内先 `puts` 后 `dels`；`dels` 的 mtime 是**客户端提交时刻**（`sync.rs:165` 的
`now_ms()`），不是文件 mtime；put 引用的 blob 不存在时**仍然记账**（清单指向空 blob，下载 404 会让
客户端报 `1 file(s) failed` 且不写基线，下一轮重试）——宁可留痕也别丢记录。

### 3.5 手写转文字：直接让 dsh 读图（**不跑 OCR 模型**）

**决定：不装 tesseract / TrOCR / PaddleOCR，也不跑本地 VLM。**
你服务器上现成的 `~/rm2/ask-ink.sh` 已经把「图 + 问题 → dsh」跑通了；
`dsh` 的 `read_image` 就是识别器，对手写/混排/公式比任何开源 OCR 都好，还省掉三个模型和推理栈。

**接口对设备完全不变**（`net.rs` 一行不用改）：

```
POST /api/v1/ocr
{"image_blob":"<64 hex>","mode":"handwriting","lang":"en"}

200 {"text":"line one\nline two","lines":[],"engine":"dsh-read"}
503 {"error":"engine_busy"}      // dsh 超时/失败/队列满
422 {"error":"no_ink"}           // 前景 < 0.2%，空白页
404 {"error":"no_blob"}          // 没先 PUT blob
```

服务端实现（约 60 行）：

1. 按 `image_blob` 从 `blobs/` 找到文件；**认 BMP 和 PNG 两种 magic**
   （设备现在发 24 位 BMP，1404×1872 ≈ 7.9 MB；以后压成 1-bit PNG 约 30–80 KB，两种都收）。
2. 拷到 `~/rm2/cache/page-<sha>.png`（BMP 用 Pillow 转一次再交给 dsh，dsh 对 PNG 更稳）。
3. 跑 dsh（照 `ask-ink.sh` 的环境准备抄）：
   ```sh
   export PATH=/home/user/.nvm/versions/node/v24.14.1/bin:$PATH
   set -a; . "$HOME/.dsh/web.env"; set +a
   timeout 180 dsh --profile headless --json "$PROMPT"
   ```
   `PROMPT` 固定（**不要**把用户问题传进去，识别要确定性）：
   ```
   这是一页手写笔记的图片：<绝对路径>
   请用 read_image 读它，逐行转成纯文本。
   只输出正文：不要解释、不要 Markdown 代码块、不要臆造看不清的字。
   公式写成 LaTeX 行内形式，表格用空格对齐。
   ```
4. 从 `--json` 流里取最后一条 `result` 的文本；取不到就退化成 stdout 全文。
   去 ``` 围栏、去首尾空白。
5. 响应 `engine` 固定 `dsh-read`，`lines` 返回**空数组**（dsh 不给 bbox，不许编）。

**已验证（2026-11-08，在 `/home/user`）**：拿 `~/rm2/sample.png`（一页真实手写，
含中文连笔 + 英文 + C++ 代码 + 定积分）直接喂给 dsh，输出：

```
这是一段手写中文测试（包含连笔）

The quick brown fox jump over
the lazy dog.

# include <iostream>
int main() {
    std::cout << "这是一段测试代码" << endl;
}

∫_a^b f(x, g(y)) dx

若 f(x) ∈ C[a,b]，则 ∃ ξ ∈ [a,b]，s.t.
∫_a^b f(x)dx = (b-a)f(ξ)
```

连 `∃ ξ` 和积分上下限都对，全程 **15.6k tokens**（约 1 页）。结论：这条路成立，
不用再考虑 OCR 引擎。

**取结果的方式**：`--json` 流最后一行是 `{"type":"final","text":"…"}`，
直接取它的 `text`（别去拼中间的 `{"type":"text"}` 增量，那是流式分片）。

运维要点：

- **串行**：同一时刻只跑一个 dsh（`flock cache/dsh.lock`）；排队 > 3 回 `503`。
- **超时**：dsh 180 s 上限，设备侧 120 s（`net.rs:456`）——**设备超时比服务器短**，
  所以服务器要把结果写进 `cache/ocr-<sha>.txt`，设备重试时直接命中缓存（超时重试很常见）。
- **缓存键**：`sha256(image_blob)`，同一页重发不再跑 dsh。
- **失败**：dsh 非零退出 → `503`，stderr 前 500 字写服务器日志（不回给设备）。
- **空页**：先算前景像素占比，< 0.2% 直接 `422`，别浪费一次 dsh。

> 真需要批量/离线识别时再回头加引擎，接口不用变。

### 3.6 安全：Tailscale 内网 + 单 token，**不做 TLS**

前提：**只有你一个人用**，设备和你都在同一个 Tailscale 网里（服务器 `100.64.0.1`）。

- **只绑 Tailscale 地址**：`8080` 只听 `100.64.0.1`（或 `0.0.0.0` + 防火墙只放行 `tailscale0`）。
  公网网卡上一个端口都不开。
- **传输**：明文 HTTP，加密由 WireGuard 提供（Tailscale 底层就是它）。
  设备端 `net.rs:102-105` 只支持 `http://`，这样正好不用改一行代码。
- **一个 token**：`rmcloud pair` 生成一次，写进设备 `server.json`；服务器只存 `sha256(token)`，
  常数时间比较。没有过期、没有轮换、没有 `unpair` 子命令——要换就删两边文件重配对。
- **配对码**：8 位十进制，TTL 10 min，一次性。防的是"别人也在你 Tailscale 网里"这种极端情况。
- **磁盘**：`/srv/rm` 0700；日志不打印 token（连前缀都不打）；`state.json` 每日备份。
- **不要做**：Caddy/Nginx、443、自签证书、`update-ca-certificates`、rustls、多设备 token、限流。
  以后真想开公网，再回来看这一节的旧方案（git 历史里有）。

### 3.7 实施顺序

**Day 1（4–6 小时，最小闭环：配对 + 同步 + Web UI 看页面）**
1. `rmcloud` 骨架：`state.json` 原子写 + `flock` + `rmcloud pair` 子命令。
2. 实现 #1 `pair`、#2 `health`、#3 `manifest`、#4/#5 `blob`、#6 `commit`（先不做 mtime/conflicts）。
3. Web UI：一个 `index.html` + `GET /api/v1/tree` + `GET /api/v1/thumb/...`（直接吐设备的缩略图）。
4. systemd 起服务，绑 Tailscale IP。
5. **验收**：`rmdev --pair http://100.64.0.1:8080 12345678` → `rmdev --sync-once` 打印 up/down；
   再跑一次必须 `0/0/0`；浏览器打开 `http://100.64.0.1:8080/` 能看到文档卡片和页面缩略图；
   设备上改个名字 → 20 s 内 Web UI 变。

**Day 2（半天：冲突语义 + 读图）**
6. 补 §3.4 的 mtime 规则与 `conflicts` 数组；写 `tests/commit.rs`（三边全不同、墓碑复活、409 重试）。
7. `POST /api/v1/ocr`：按 §3.5 接 dsh（先手动跑通 `dsh ... read_image` 一条命令，再包成 HTTP）。
   **验收**：设备上打开一页手写 → 分享面板 `Convert to text` → 文本出现在页面上；
   同一页再点一次（走缓存）应在 1 s 内返回。
8. `POST /api/v1/mail`（msmtp + App Password），失败把 SMTP 原文放进 `detail`。
   **验收**：`Send by email` 收到带页面 PDF 的邮件。

**Day 3（可选，按需要做）**
9. `POST /api/v1/import`：Web 上传 PDF/EPUB 进设备库（单用户很有用的"把书塞进设备"）。
10. `rmcloud gc`（清孤儿 blob）+ 每日 `rsync` 备份到另一块盘。
11. 断网 5 分钟后恢复，设备能自动补同步（这条主要验设备侧）。

**不做**：版本历史/回滚、软件更新分发、Google Drive/Dropbox/OneDrive 集成、多设备、公网 HTTPS。

## 4. 设备端要配合改的地方

| 文件 : 函数 | 现状 | 要做什么 |
|---|---|---|
| `net.rs:102` `split_url` | 只认 `http://` | **不改**：Tailscale 明文够用（§3.6） |
| `net.rs:21` `Config` | `url/token/device_id/last_rev/mail_to` | 不改（不需要 `tls_pin`） |
| `net.rs:372` `put_blob` | 手写了一遍 HTTP 请求（为了加 `X-Sha256`） | 抽成 `request_with_headers()`，否则每次加头都要复制一遍（**要改**） |
| `net.rs:450` `ocr` | 把 24 位 BMP 直接当 blob 传（7.9 MB/页） | 走 Tailscale 时不心疼带宽，**先不改**；想省流量再用 `zune_png::Encoder` 编 1-bit PNG（约 30–80 KB） |
| `net.rs:363/422` `manifest`+`commit` | 已按 `rev/entries[]`/`base_rev+puts+dels+conflicts` 实现 | 无需改（**已完成**，是服务器要对齐它） |
| `net.rs`（无） | 没有更新检查 | **不做** |
| `sync.rs:242` `plan()` | 三路比较完整，13 条断言覆盖 | 无需改；**它是冲突语义的唯一权威**（**已完成**） |
| `sync.rs:22` `MAX_FILE` | 32 MB | 和服务端 64 MB 不冲突；若以后要同步大 PDF，两边一起调（保持） |
| `sync.rs:442` `conflict_path` | `<rel>.server-conflict-<UTC>` | 无需改；文档已按它写（**已完成**） |
| `main.rs:5243` `start_ocr` | 写死 `"handwriting","en"` | **不用改**：dsh 自己判语言，`lang` 字段服务器忽略 |
| `main.rs:5266` `insert_ocr_text` | 把文本行插到**当前页**底部 | 官方是「**新增一页**」（`res_066b1b0.qml:79`「Converted text added as new page」）；至少加个 undo（**要改**） |
| `main.rs:5305` `start_sync` | 只有手动触发 | 接 `Sync before sleep`（`main.rs:6585` 现在只是个纯偏好，没人读）、以及退到库/息屏时自动同步（**要改**） |
| `main.rs:5613` `start_pair` | 只存 url+code | **不改** |
| `main.rs:5492` `send_mail_now` | 单附件、默认收件人存在 `Config.mail_to` | 官方可多收件人（`res_06af7c6.qml`「Use space bar to add email address」）；当前够用（可选） |
| `share.rs:31` `start` | 8000–8009 BMP 流 | 不动。官方是 RFB/WebRTC，我们不复刻（**已完成**） |
| `src/doc/rm.rs` | `.rm` 读写 | 不动。服务器**不**造 `.rm`，空页就是没有 `.rm`（约束） |

---

## 5. 与早期设计稿的差异（以代码为准）

| 项 | 早期设计稿 | 实际代码 | 决定 |
|---|---|---|---|
| OCR 请求字段 | 有 `page_width/page_height` | `net.rs:452` 只发 3 个字段 | 服务器把它们当**可选**，缺省 1404×1872 |
| 冲突副本命名 | `<name> (conflict <时间>).rmdoc` | `sync.rs:442` `<rel>.server-conflict-<yyyymmdd-HHMMSS>` | 以代码为准，文档改掉 |
| `conflicts` 元素 | 未写明 | `net.rs:440` 当字符串数组 | 明确为 path 字符串数组 |
| 单文件上限 | 64 MB | 设备 32 MB（`sync.rs:22`） | 服务器收 64 MB，设备自己 32 MB |
| `?since=` 增量清单 | 「可选优化」 | `net.rs` 从不带 | 删掉，不做 |
| 更新接口 | 已设计 | 设备端**没有**实现 | **不做** |
| 版本历史 | `history/<rev>.json` | 无设备端依赖 | **不做** |
| 屏幕共享 | 端口 8000–8009 | `share.rs:31` 一致 | 保留 |
| `state` 形状 | 单 `tree.json` | — | 改为 `state.json`（rev + entries 一张表） |

旧文档**仍然有效**的：设备文件四件套格式（附录 A）、`pageCount == cPages.pages.len()`、
`sizeInBytes` 是字符串、`.content` 时间戳是 `"cur:sub"`。这些是导入接口的地基，别改。
