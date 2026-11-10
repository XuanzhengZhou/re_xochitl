# CLOUD.md — Self-Hosted Cloud Service Blueprint

> English translation of [`../CLOUD.md`](../CLOUD.md) (Chinese original).

> Replacing the official **Connect subscription cloud** with one server of your own. Four parts:
> what the official cloud offers (with evidence), what can be recovered from the official protocol, how the
> server is designed (implementable as written), and what the device side still has to change.
>
> Evidence conventions: `strings xochitl` = a string scan of the `xochitl` taken off the device (an
> 8 MB ARM binary); `res_xxxxx.qml:N` = a line number in the carved QML tree (the QML tree is not
> distributed with the repository); `net.rs:N` / `sync.rs:N` = existing device-side code in
> `../../rmdev/src/`. **Anything without evidence is marked as speculation.**

---

## 0. At a Glance

1. The official cloud is a storage service that syncs **whole documents (zip) plus a JSON manifest**, not a fine-grained protocol; all endpoints live under `/document-storage/json/2/*`, `/token/json/2/*` and `/convert/v1/handwriting` (`strings xochitl`).
2. The device **already speaks** the self-hosted protocol (four endpoints in `net.rs` plus the three-way comparison in `sync.rs`); a server implementing §3.2 interoperates with it.
3. **No TLS**: single user, device and server on the same Tailscale network, so plain HTTP is enough (WireGuard provides the encryption), and `net.rs` only ever accepted `http://` — nothing to change. See §3.6.
4. **No OCR model**: handwriting conversion is done by letting `dsh` on the server read the image (`read_image`). See §3.5.
5. The only extra feature compared with the official cloud is viewing pages directly in a Web UI. Version history, software updates and third-party integrations are **not implemented**.

---

## 1. What the Official Cloud Actually Provides

| # | Capability | Official implementation | Evidence | Our replacement |
|---|---|---|---|---|
| 1 | **Multi-device sync** | document-level zip upload/download + manifest `rev` | `res_065dcf1.qml:64` "Sync files between your paper tablet, mobile and desktop"; `DocumentSync`, `documentsync_1_5.cpp` (`strings xochitl`) | §3.2 manifest/blob/commit |
| 2 | **Cloud backup** | the same storage, framed as "backup", with a 50-day retention policy | `res_065dcf1.qml:65` "Back up your work"; "Notes will automatically back up in the cloud and sync to the apps for 50 days." (`strings xochitl`) | the same blob storage, no retention (§3.3) |
| 3 | **Version history** | Connect's "file version history", web/App side only | **zero evidence on the device side**: in QML, `History` only appears in the GPL text (`res_062c955.qml`), there is no version-history UI ⇒ **speculation** | **not implemented** (not needed) |
| 4 | **Handwriting to text** | upload ink → cloud OCR (MyScript iink) → page comes back | `res_065dcf1.qml:67` "Convert handwritten notes into typed text"; `/convert/v1/handwriting`, `Recognizer`/`RecognitionJob`, `application/vnd.myscript.jiix`, MyScript licensing statement (`strings xochitl`) | §3.5 lets `dsh` on the server read the image (no OCR model) |
| 5 | **PDF to text** | **explicitly unsupported on the device side**: conversion is only offered for note pages | `res_06678c9.qml:20` "You can only convert note pages to text"; PDFs only get **local** text-layer search (`res_0666bce.qml:435` "Search in PDFs, EPUBs, or your typed text") ⇒ cloud PDF→text is **speculation / unofficial** | **not implemented** (the device already searches PDF text layers locally) |
| 6 | **Send by email** | page rendered to PDF/PNG → sent by the cloud, carries the official signature, not replyable | `res_065dcf1.qml:66` "Share files by email"; `res_067e8cc.qml` "Send by mail"; `rm.network.send.mail`, `/sendmail_`, signature string "Sent from my reMarkable paper tablet... PS: You cannot reply to this email" (`strings xochitl`) | §3.2 `POST /api/v1/mail` |
| 7 | **Phone/desktop viewing** | `my.remarkable.com` web app plus mobile/desktop apps | `frag_0038_0065ac0e.qml:48` "download the mobile and desktop apps from %1"; `/webapp` + `*.my.remarkable.com` (`strings xochitl`); `res_068b63a.qml:188` `NetworkClient.loginUrl` | §3.2 Web UI (read-only browsing + download) |
| 8 | **Screen share (LiveView)** | v1 = direct RFB/VNC; v2 = WebRTC + MQTT signalling | `frag_0081_006bd97e.qml:8` "Present with Screen Share"; `res_067887b.qml:2390` "Accept connection in the desktop app..."; `RFB 003.008`, `src/screenshare/src/*`, `WebRtc::Manager`, `vernemq-%1.cloud.remarkable.engineering` (`strings xochitl`) | existing `rmdev/src/share.rs` (BMP over HTTP, 8000-8009); **no server needed** |
| 9 | **File upload (web/email to device)** | desktop/mobile app uploads a PDF; the device also has a local USB web upload (`http://10.11.99.1`) | `res_06a0e55.qml:292` "You can import documents as PDF files through the desktop or mobile app"; `rm.usb.web` + `POST /upload` (`strings xochitl`); `res_0686a15.qml:130` gives 10.11.99.1. **"Email to device" has no evidence at all ⇒ speculation** | §3.2 `POST /api/v1/import` |
| 10 | **Software update distribution** | separate process `update-engine.service`; xochitl only triggers it over D-Bus | `update-engine.service`, D-Bus name `no.remarkable.update1`, object path `/no/remarkable/update1` (`strings xochitl`); `res_0698460.qml:165` "Check for updates"; beta programme described in `res_0698460.qml` | **not implemented** (not implemented on the device side either) |
| 11 | **Accounts and subscriptions** | one-time pairing code (8 digits) binds the account; Connect subscription; without one, the cloud stops after 50 days of inactivity | `frag_0026_0065294a.qml:26` `maximumLength: 8`; `res_066a452.qml:43` "over 50 days, which is the cloud limit without a Connect subscription"; `res_0686a15.qml:265-267` "Sync without Connect"; `res_068b63a.qml:177` "Enjoy unlimited access to all your notes..." | **all dropped**; only a device token remains (§3.6) |
| 12 | **Cloud storage quota** | subscription claims "unlimited" | `res_068b63a.qml:177` "Enjoy unlimited access to all your notes and documents in the reMarkable cloud". No evidence for concrete quota numbers; `storageAvailable/storageUsed/storageTotal` (`strings xochitl`) look more like a third-party cloud-drive integration and are **presumably** not the rM cloud | quota is ignored; 507 when the disk is full |

**Negative facts (equally important)**:
- The device-side QML has **no** version-history or cloud-trash pages → both are Web/App-side features; do not expect the device to drive them.
- Screen sharing does **not** go through `/document-storage`; it is a separate MQTT + RFB/WebRTC channel, and it is not replicated here.

---

## 2. Facts About the Official Protocol

### 2.1 Environments and hostnames

```
dev.internal.cloud.remarkable.com         server (storage / token / conversion)
qa.internal.cloud.remarkable.com
stage.internal.cloud.remarkable.com
development.my.remarkable.com            web app / account side
qa.my.remarkable.com
staging.my.remarkable.com
service-manager-…-dot-remarkable-…appspot.com     service discovery (App Engine)
vernemq-%1.cloud.remarkable.engineering           MQTT broker (screen-share signalling)
```
(All from `strings xochitl`.)

- **The full production hostname is not recoverable**: the strings only contain the dev/qa/stage sets; production should be `internal.cloud.remarkable.com` (no prefix), but no direct string proves it ⇒ speculation.
- Environment switching goes through `xochitl set-environment` (`set-environment` / `Environment set to` / `Unknown service environment:`).
- **Service discovery**: the device first asks the service manager for a service's hostname, then sends the real request.
  Evidence: `/service/json/`, `/document-storage` (adjacent strings), `Fetching document storage hostname`,
  `Failed to fetch storage hostname`, `ServiceHostname::fetchHostname`, QML module `rm.network.hostnames`.
  The version number and response format after `/service/json/` are **not recoverable**.

### 2.2 Pairing and tokens

| Step | Request | Evidence |
|---|---|---|
| One-time code | the user types an 8-digit code on the device (fixed-width dashed input field) | `frag_0026_0065294a.qml:26` `maximumLength: 8` |
| Exchange for a device token | `POST /token/json/2/device/new`, body fields `code`, `deviceDesc`, `deviceID`, `secret` | the four field names appear adjacent to the endpoint (`strings xochitl`) |
| Device token shape | JWT: `device token has {} .-separated chunks`, carrying an `auth0-userid` claim | `strings xochitl` |
| Exchange for a user token | `POST /token/json/2/user/new`, carrying the device token | same as above |
| Request authentication | `Authorization: Bearer <user token>` (the `Bearer ` constant appears on its own) | same as above |
| Token expiry | 401 → re-fetch the user token; 403 → fail outright; `token error 401/403` | same as above |
| Wrong code | `deviceTokenErrorCode === 409`; message `Invalid One-time-code` | `res_068a5d6.qml` / `strings xochitl` |

The algorithm behind `secret` is **not recoverable**. The third-party `rmapi` uses `sha256(deviceID)`, but that is not evidence from this device ⇒ speculation.
Unpairing a device is `POST /token/json/3/device/delete`; after a successful token exchange the device writes a `got new usertoken: {}` log line.

### 2.3 Document storage API (the substance of official sync)

| Method | Path | Purpose |
|---|---|---|
| GET | `/document-storage/json/2/docs` | fetch the full document manifest |
| POST | `/document-storage/json/2/upload/request` | request an upload URL (response contains `BlobURLPut`) |
| POST | `/document-storage/json/2/delete` | delete a document |
| GET | `BlobURLGet` (absolute URL given in the manifest) | download the document zip |

Manifest entry field names (all from adjacent strings in `strings xochitl`):
`ID`, `VissibleName` (**an official typo, two i's**), `Parent`, `Type`, `Version`, `ModifiedClient`,
`Bookmarked`, `Pinned`, `Deleted`, `lastModified`, `hash`, `size`, `broadcast`, `generation`, `children`.
- Lowercase aliases exist too: `vissibleName`, `parent`, `bookmarked` (same string block).
- Time-format constant: `yyyy-MM-ddTHH:mm:ss.zzZ` (ISO-8601 milliseconds + Z).
- The manifest is an **array** (`entries in reply`, `Received list entry with no ID`, `Reply contains entry without id`),
  not an `{entries:[...]}` wrapper ⇒ inferred from those strings.
- `Type` has only two legal values (`DocumentType`/`CollectionType`); if the server sends anything else the device reports
  `we did not get valid type from the server`.

**The unit of transfer is a zip of the whole document**: `.zip`, `.part`, `.downloading`, `.uploading`, `Could not unpack`,
`Downloaded document was corrupt.`, `Failed to pack`. Filename templates inside the zip: `%1.rm` (page ink),
`%1-metadata.json` (document metadata), both near `DocumentUploader::PackFileConfig`; there is also a set of payload
names `.metadata`/`.content`/`.pagedata` (near `documentsync_1_5.cpp`, log `got payload file %s`).
- "Metadata-only changes skip the file upload": `Only metadata modified, skipping file upload`; downloads land in `.part` first and are then unpacked
  (`Error during download of file`, `Not enough free space to download`).

### 2.4 Sync protocol versions and notifications

- Two implementations plus a wrapper: `DocumentSync_1_0`, `DocumentSync_1_5`, `DocumentSyncProtocolWrapper`
  (`documentsync_1_5.cpp`, `documentsyncprotocolwrapper.cpp`).
- The switch is negotiated at runtime: `Requesting sync protocol:`, `current is`, `now switching sync protocol...`,
  `Sync 1.5 is up and running`, `set protocolFlag to %d`, `Sync::ProtocolFlag`, `requestProtocol`.
  **The protocol numbers are `1.0` / `1.5`; there are no strings like `sync15`/`sync31`.**
- Semantics of 1.5: two modes, `full-sync` / `entry-sync`, reconciling file by file over `.metadata`/`.content`/`.pagedata`;
  conflicts go through `updateConflictingFiles` + `processSyncConflict` ("have {} conflicts in {:p}") →
  **the official protocol is itself "file-level, conflicts kept as copies"**, structurally the same as `sync.rs`. Period constants
  `SYNC_ACTIVE_PERIOD_MS`/`SYNC_REFRESH_INACTIVE_MS`; `generation` is a monotonically increasing counter.
- Notifications: MQTT (paho, `src/mqtt/src/pahomqtt/client.cpp`), service name `/notifications`, log line
  `Connecting to MQTT broker...`. Topic names, port, and whether it uses WebSocket are **not recoverable**.

### 2.5 Handwriting to text

- Endpoint: `POST /convert/v1/handwriting` (`strings xochitl`, near `src/handwritingconversion/src/recognizer.cpp`).
- The request is multipart: `QHttpMultiPart` + `QHttpPart` (Qt symbols in `strings xochitl`).
- Response type: `application/vnd.myscript.jiix` (MyScript iink's JIIX format).
- The engine is licensed from MyScript (`a registered trademark of MyScript. Our handwriting conversion application
  includes software licensed by MyScript.`); it supports **33 languages** (`hwcInfoString`, `frag_0081_006bd97e.qml`).
- Which pages get sent is decided by `hwcanalyzer` (`src/handwritingconversion/src/hwcanalyzer.cpp`); the recognition
  result is written by `RecognitionJob::applyFormattedText` as a **new page** (`res_066b1b0.qml:79` "Converted text added as new page").
- The concrete request-body fields (stylus coordinates? SVG? PNG?) are **not recoverable**. The binary contains an
  SVG template with an embedded PNG, `image xlink:href="data:image/png;base64,%5"`, but that is used for SVG export and
  does not prove the OCR payload looks like it.

### 2.6 Email

- `rm.network.send.mail`, endpoint string `/sendmail_`, email regex
  `\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b`, signature HTML
  `Sent from my reMarkable paper tablet<br>Get yours at www.remarkable.com<br><br>PS: You cannot reply to this email<br>`.
- The request-body fields, whether it is multipart, and the attachment format (PDF/PNG) are **not recoverable**.

### 2.7 Screen sharing

- v1: **real VNC**. Handshake string `RFB 003.008`, implementations `src/screenshare/src/compat/rfbserverprotocol.cpp` and
  `rfbclientprotocol.cpp`, log `RfbScreenShareServer started listening on port`.
- v2: WebRTC (`src/webrtc/src/webrtc.cpp`, `WebRtc::Manager::createPeerConnection(QString,QString)`,
  `Logical error while sending data on WebRTC datachannel`) + MQTT signalling
  (`ScreenShareClient::initMqtt`, broker template `vernemq-%1.cloud.remarkable.engineering`).
- The peer is the desktop app (`Accept connection in the desktop app...`), not a browser.

### 2.8 Software updates

- `update-engine.service` + D-Bus `no.remarkable.update1` / `/no/remarkable/update1`;
  xochitl only starts/stops update-engine and reads its status (`update-engine status:`, `Failed to stop update-engine`).
- The update URL and version-manifest format live inside `update-engine`, and **that binary has not been analysed** ⇒ not recoverable.
  (The often-quoted `updates.cloud.remarkable.com` has no string in xochitl — do not record it as fact.)

### 2.9 Explicitly not recoverable (do not invent)

1. The exact spelling of the production hostname (only the dev/qa/stage sets exist), and the version number and response JSON of `/service/json/`.
2. The request-body fields of `/sendmail_` and `/convert/v1/handwriting` (all that is known is that the latter is multipart and answers in JIIX).
3. MQTT topic naming, port and authentication (only paho and the broker host template are visible).
4. The algorithm for `hash` in the manifest (presumably sha256 of the zip, no evidence) and for `secret` (presumably `sha256(deviceID)`).
5. The update manifest URL and format (they live in update-engine), and the version-history/trash endpoints (no trace on the device side).

---

## 3. Server Design

### 3.1 Deployment shape (single user + Tailscale, no reverse proxy)

```
rM2 (net.rs) ──HTTP/1.1 over Tailscale──▶ rmcloud :8080 (bound to 100.64.0.1 only)
                                            ├─ /api/v1/*   pairing / sync / image read / mail
                                            └─ /            Web UI
```

| Item | Choice | Rationale |
|---|---|---|
| Main process | **single Rust binary** `rmcloud` (axum + tokio) or **Python 3.12 + FastAPI** | only six endpoints are ever called by the device; use whichever is handier |
| Reverse proxy | **none** | Tailscale already encrypts; Caddy/Nginx only add a failure mode and certificate hassle |
| Port | 8080, bound to `100.64.0.1` only | not a single port is opened on the public interface |
| Directories | `/srv/rm` (0700, owned by `rmcloud`), config `/etc/rmcloud.toml` | see §3.3 |
| systemd | **one** unit, `rmcloud.service`, `Restart=always` | there is no OCR worker any more (§3.5) |
| Dependencies | no database, no Redis, no Docker | all state is one `state.json` plus the directory tree |

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

> The deployment target has no nginx / caddy / docker, and none should be installed. Bind `8080` on the
> Tailscale address; if the port is already taken, use `8090`.

### 3.2 API contract (**byte-for-byte alignment with `net.rs`**, new endpoints marked)

Conventions: JSON is UTF-8; timestamps are **integer milliseconds**; device endpoints authenticate with
`Authorization: Bearer <token>`; errors are always `{"error":"<snake_case>"}`; responses must carry
`Content-Length` or `Transfer-Encoding: chunked`, and **must tolerate `Connection: close`**
(`net.rs:143` closes on every request).

| # | Method | Path | Auth | Used by |
|---|---|---|---|---|
| 1 | POST | `/api/v1/pair` | none | pairing (`net.rs:315`) |
| 2 | GET | `/api/v1/health` | none | liveness (`net.rs:357`) |
| 3 | GET | `/api/v1/manifest` | Bearer | fetch manifest (`net.rs:363`) |
| 4 | POST | `/api/v1/blob` | Bearer + `X-Sha256` | upload blob (`net.rs:372`) |
| 5 | GET | `/api/v1/blob/{sha256}` | Bearer | download blob (`net.rs:405`) |
| 6 | POST | `/api/v1/commit` | Bearer | commit changes (`net.rs:422`) |
| 7 | POST | `/api/v1/ocr` | Bearer | text conversion → `dsh` reads the image (`net.rs:450`, §3.5) |
| 8 | POST | `/api/v1/mail` | Bearer | send mail (`net.rs:466`) |
| 9 | GET | `/api/v1/tree` | Bearer | **new**: Web UI document tree |
| 10 | GET | `/api/v1/thumb/{uuid}/{page}.png` | Bearer | **new**: Web UI thumbnails |
| 11 | GET | `/api/v1/download/{uuid}` | Bearer | **new**: raw PDF/EPUB or bundled download |
| 12 | POST | `/api/v1/import` | Bearer | **new**: push a PDF/EPUB into the device library |
| ~~13~~ | ~~GET~~ | ~~`/api/v1/update/latest`~~ | — | **not implemented** (software updates dropped) |
| ~~14~~ | ~~GET~~ | ~~`/dl/{name}`~~ | — | **not implemented** |

#### 1) Pairing

```
POST /api/v1/pair
{"code":"12345678","device_name":"reMarkable 2","protocol":1}
200 {"device_id":"d-3f2a9c11","token":"<64 hex>","server_time":1700000000000}
401 {"error":"bad_code"}          // wrong / expired / already used
429 {"error":"too_many_attempts"} // repeated failures from the same IP
```
- Pairing codes are generated by `rmcloud pair`: 8 decimal digits, 10-minute TTL, **single use**, printed to stdout.
- The server only records `protocol` (the current client always sends `1`).
- `device_name` may be empty; the server creates a directory per `<device_id>`.
- **No user system**: no sign-up, no password, no email verification.

#### 2) Health check

```
GET /api/v1/health
200 {"ok":true,"rev":1234,"devices":2}
```
Unauthenticated (`net.rs:357` sends no token), so it **must not** expose any path information.

#### 3) Manifest

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
Server rules:
- A record has either `sha256`+`size`+`mtime` or `deleted:true`+`mtime`. **Do not** mix the two field sets.
- `path` must be relative: no leading `/`, no `..`, no empty segments (the client drops the whole entry, `sync.rs:320`).
- Entries whose path contains a `.local` segment or a `.tmp` suffix are skipped by the client (`sync.rs:314`) — the server should simply not store them.
- Always return the full set; no `?since=` (`net.rs` never sends one, and the official manifest is full anyway).
- Prefer `Content-Length` (the client accepts chunked, but it reads the whole body into memory either way, and a 1 GB device cannot afford a large library).

#### 4) Upload / 5) Download

```
POST /api/v1/blob
Authorization: Bearer <t>
X-Sha256: <64 hex>
Content-Type: application/octet-stream
Content-Length: 743688
<raw bytes>
204                                    // success (204 even if the blob already exists — idempotent by nature)
400 {"error":"sha_mismatch"}           // header sha does not match the computed body hash
413 {"error":"too_large"}              // > 64 MB
```
```
GET /api/v1/blob/{sha256}
200 <raw bytes>  Content-Type: application/octet-stream
404 {"error":"no_blob"}
```
- The server **must** recompute sha256 and verify it (the device names blobs by `X-Sha256`, so skipping the check can store the wrong content).
- The server caps at 64 MB; the client rejects anything over 32 MB itself (`sync.rs:22`), so 413 is never reached in practice.
- `Range` is not supported. The device always reads the whole blob.

#### 6) Commit

```
POST /api/v1/commit
{"base_rev":1234,
 "puts":[{"path":"a1b2.metadata","sha256":"<64 hex>","mtime":1700000000000}],
 "dels":[{"path":"c3d4.metadata","mtime":1700000000500}]}
200 {"rev":1235,"conflicts":[]}
200 {"rev":1235,"conflicts":["a1b2.metadata"]}   // the server considers its own copy newer and rejected this put
409 {"error":"rev_changed","rev":1240}
```
- `conflicts` is an **array of path strings** (`net.rs:440-445` parses it as strings). Do not put objects in it.
- The `error` field of a 409 must **literally contain** `rev_changed`: the client checks `e.contains("rev_changed")`
  (`sync.rs:202`), so a `"rev_conflict"` payload never enters the retry branch.
- `puts`/`dels` of one commit must not contain the same path (`400 {"error":"dup_path"}`).
- `rev` only increments on a successful commit; the manifest does not change rev.
- Empty `puts`+`dels` is allowed and returns the current rev without incrementing it.

#### 7) OCR

```
POST /api/v1/ocr
{"image_blob":"<64 hex>","mode":"handwriting","lang":"en"}

200 {"text":"line one\nline two",
     "lines":[{"text":"line one","bbox":[40,100,900,140],"conf":0.93}],
     "engine":"dsh-read"}
503 {"error":"engine_busy"}
422 {"error":"no_ink"}          // the whole page is blank
404 {"error":"no_blob"}         // image_blob was never uploaded
```
- **The client sends only three fields** (`net.rs:452`): `image_blob`, `mode`, `lang`.
  `page_width`/`page_height`/`dpi` must **not** be made mandatory; an older document listed them, but the client does not send them.
- `mode` ∈ `handwriting` | `printed`; `lang` ∈ `en` | `zh` | `auto` (unknown values are treated as `auto`).
- The client only reads `text` (`net.rs:462`); `lines` is for the Web UI and for selectable text later on.
- Timeout 120 s (`net.rs:456`). When it expires the connection drops and the client reports `read failed`.

#### 8) Mail

```
POST /api/v1/mail
{"to":["a@b.c"],"subject":"Notes","body":"...",
 "attachments":[{"blob":"<64 hex>","filename":"page-3.pdf","mime":"application/pdf"}]}

200 {"ok":true}
400 {"error":"bad_address"}
413 {"error":"too_large"}
502 {"error":"smtp_failed","detail":"550 5.7.1 ..."}   // detail is passed back to the device verbatim for troubleshooting
```
- Attachments go through `POST /api/v1/blob` first as well; only the reference is sent here (avoids a 4/3 base64 expansion).
- The server sends with `msmtp`/`lettre`; the sender is fixed, and the official signature line must not be appended to the body (that one is theirs).
- Attachments ≤ 20 MB, at most 5.

#### 9–11) Web UI read endpoints (new, never called by the device)

```
GET /api/v1/tree
200 {"rev":1234,
     "docs":[{"uuid":"a1b2","name":"Meeting","type":"DocumentType","parent":"",
              "deleted":false,"pages":3,"modified":1700000000000,
              "has_pdf":false,"thumb":"a1b2.thumbnails/p1.png"}]}
```
- The server **parses** `.metadata`/`.content` itself (the four-file device document format) instead of making the device compute things.
- `GET /api/v1/thumb/{uuid}/{page}.png`: serves the `<uuid>.thumbnails/<page>.png` the device uploaded
  (`sync.rs:333` already includes `.thumbnails` in sync, so **the server never renders `.rm`**); return 404 when missing and let the Web UI draw a grey block.
- `GET /api/v1/download/{uuid}`: serves `<uuid>.pdf`/`<uuid>.epub` as-is when present; otherwise zips
  `<uuid>.metadata` + `content` + `pagedata` + `<uuid>/*.rm`.

#### 12) Document import (new)

```
POST /api/v1/import
Content-Type: multipart/form-data; boundary=...
file=@book.pdf        # file part; filename is the visibleName
parent=<uuid>        # optional form field, defaults to the root

200 {"uuid":"e5f6","pages":128,"rev":1235}
415 {"error":"unsupported_type"}     // only application/pdf and application/epub+zip are accepted
413 {"error":"too_large"}            // single file ≤ 200 MB
```
What the server does (**equivalent to an ordinary commit**, so the device pulls it on its next sync):
1. `uuid = uuid4`; store the uploaded bytes as a blob under `<uuid>.pdf` / `<uuid>.epub`; get the page count N from `pdfinfo` or the epub parser.
2. Generate `<uuid>.metadata` (`type:"DocumentType"`, `parent`, `visibleName` = filename, `lastModified` = now,
   `deleted:false`, `version:1`) and `<uuid>.pagedata` (N lines of `Blank`).
3. Generate `<uuid>.content`: `fileType:"pdf"` (or `"epub"`), `cPages.pages[]`, `pageCount:N`,
   `sizeInBytes:"<string>"`, and for PDFs also `redirectionPageMap`.
4. **Generate no `.rm`** (in the four-file format a page without `.rm` is a blank page; the device still renders the PDF page).
5. Write into `state.entries` inside one transaction, `rev += 1`.

#### 13–14) Update distribution

```
<!-- Update distribution is not implemented: the device has no update_check(), and it is not needed. -->

/srv/rm/                       0700 root:rmcloud
├── state.json                 {"rev":1235,"entries":{ "<path>": {"sha256","size","mtime"} | {"deleted":true,"mtime"} }}
├── devices.json               {"d-3f2a9c11":{"token_sha256":"...","name":"reMarkable 2","paired_at":...,"last_seen":...}}
├── blobs/ab/ab12… (64 hex)    content-addressed, raw bytes, never deleted (version history would need them)
├── tmp/                       upload staging area (same filesystem, so rename is atomic)
├── ocr/<sha256>.json          OCR result cache (same image + same lang answers directly)
└── web/                       Web UI static files (may also be compiled into the binary)
```

- **Content addressing**: `sha256(file bytes)`, bucketed by the first 2 characters. One blob referenced from several paths is
  normal (conflict copies, version history); do not assume one blob per path.
- **Tombstones**: `dels` only replaces the entry with `{"deleted":true,"mtime":...}`; **blobs are not deleted**.
  Real deletion is done by `rmcloud gc` (manual or scheduled, keeping blobs referenced within the last 90 days).
- **No version history**: a commit only writes `state.json` and keeps no history. Rolling back means restoring a backup.


### 3.4 Conflict policy: device wins, conflicts kept as copies

**The authoritative semantics live in `plan()` at `sync.rs:242`**; the server only keeps the books and refuses overwrites — it makes no decisions:

| Local | Server | Base | Device action (`plan()`) | What the server sees |
|---|---|---|---|---|
| none | present | present and = server | `DelRemote` | one `dels` in the commit |
| none | present | absent / server newer | `Download` | only a GET of the blob |
| present | none | present and = local | `DelLocal` | nothing sent |
| present | none | absent / modified locally | `Upload` | `POST blob` + put |
| present | present | same sha | `Nothing` | nothing |
| present | present | base = server | `Download` | local copy overwritten |
| present | present | base = local | `Upload` | put |
| present | present | all three differ / no base | **`Conflict`** | GET the server copy → write `<rel>.server-conflict-<UTC>` → then put |

Four guarantees the server must provide:

1. **rev gate**: `base_rev != state.rev` → 409. This is the only concurrency serialization mechanism; on receiving it the
   client re-fetches the manifest, recomputes and retries once (`sync.rs:55-72`); two 409s in a row make it fail and give up.
2. **mtime only blocks stale writes**: if an **active** entry exists for a `put` and `entry.mtime > put.mtime`, the entry is
   left untouched and the path is returned in `conflicts`. The client drops that path from its baseline and reports
   `server kept its newer copy` (`sync.rs:197-200`).
3. **put beats tombstone**: when `entries[path]` is a tombstone, the `put` overwrites it (resurrection) and mtime is **not**
   compared. Reason: a tombstone's mtime is the deletion time, always newer than the real mtime of the resurrected file, so
   comparing them would permanently reject resurrection, whereas the three-way comparison on the device has already decided
   that resurrection is due. The rev gate already guarantees that this put was computed from the latest state.
4. **The server never creates conflict copies**: the copy is a local file on the device (`sync.rs:442`,
   `<original path>.server-conflict-<yyyymmdd-HHMMSS>`, UTC) which then syncs up as an **ordinary new file**.
   The Web UI must display such files instead of filtering them out as junk.

Also: within one commit `puts` are applied before `dels`; a `dels` mtime is the **client commit time**
(`now_ms()` at `sync.rs:165`), not the file mtime; when a `put` references a blob that does not exist the entry is
**still recorded** (the manifest points at a missing blob, downloading it 404s, the client reports `1 file(s) failed`
and does not write its baseline, then retries next round) — leaving a trace beats losing a record.

### 3.5 Handwriting to text: let dsh read the image (**no OCR model**)

**Decision: install no tesseract / TrOCR / PaddleOCR and run no local VLM.**
`~/rm2/ask-ink.sh`, already present on the server, has "image + question → dsh" working;
`read_image` in `dsh` is the recognizer and beats any open-source OCR on handwriting, mixed layout and formulas,
while removing three models and an inference stack.

**The API is unchanged from the device's point of view** (`net.rs` needs no edit):

```
POST /api/v1/ocr
{"image_blob":"<64 hex>","mode":"handwriting","lang":"en"}

200 {"text":"line one\nline two","lines":[],"engine":"dsh-read"}
503 {"error":"engine_busy"}      // dsh timed out / failed / queue full
422 {"error":"no_ink"}           // foreground < 0.2%, blank page
404 {"error":"no_blob"}          // the blob was not PUT first
```

Server implementation (about 60 lines):

1. Find the file in `blobs/` by `image_blob`; **accept both BMP and PNG magic**
   (the device currently sends 24-bit BMP, 1404×1872 ≈ 7.9 MB; a future 1-bit PNG is about 30–80 KB — accept both).
2. Copy it to `~/rm2/cache/page-<sha>.png` (convert BMP once with Pillow before handing it to dsh; dsh is steadier on PNG).
3. Run dsh (copy the environment setup from `ask-ink.sh`):
   ```sh
   export PATH=/home/user/.nvm/versions/node/v24.14.1/bin:$PATH
   set -a; . "$HOME/.dsh/web.env"; set +a
   timeout 180 dsh --profile headless --json "$PROMPT"
   ```
   `PROMPT` is fixed (**do not** pass the user's question in; recognition must be deterministic):
   ```
   This is an image of one page of handwritten notes: <absolute path>
   Read it with read_image and convert it to plain text, line by line.
   Output only the body text: no explanation, no Markdown code fences, do not invent characters you cannot read.
   Write formulas as inline LaTeX, align tables with spaces.
   ```
4. Take the text of the last `result` entry in the `--json` stream; if there is none, fall back to the whole stdout.
   Strip ``` fences and trim whitespace.
5. The `engine` field is always `dsh-read`, and `lines` returns an **empty array** (dsh gives no bbox; do not invent one).

**Verified** with `~/rm2/sample.png` (one real page of handwriting containing Chinese cursive + English + C++ code +
a definite integral) fed straight to dsh; the model output is reproduced verbatim below — the Chinese lines are the
transcribed handwriting itself, not this note's prose:

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

Even `∃ ξ` and the integral bounds came out right, for **15.6k tokens** in total (about one page). Conclusion: the
approach holds; no OCR engine needs to be considered again.

**How to read the result**: the last line of the `--json` stream is `{"type":"final","text":"…"}`; take its `text`
directly (do not concatenate the intermediate `{"type":"text"}` deltas — those are streaming fragments).

Operational notes:

- **Serial**: run only one dsh at a time (`flock cache/dsh.lock`); queue depth > 3 answers `503`.
- **Timeout**: dsh caps at 180 s, the device at 120 s (`net.rs:456`) — **the device timeout is shorter than the server's**,
  so the server must write results to `cache/ocr-<sha>.txt` and hit that cache when the device retries (timeout retries are common).
- **Cache key**: `sha256(image_blob)`; the same page is not re-run through dsh.
- **Failure**: dsh exits non-zero → `503`, and the first 500 characters of stderr go to the server log (never back to the device).
- **Blank page**: compute the foreground pixel ratio first; below 0.2% answer `422` straight away instead of wasting a dsh run.

> If batch or offline recognition is ever really needed, add an engine then; the API does not change.

### 3.6 Security: Tailscale network + one token, **no TLS**

Precondition: **a single user**, and the device and server are on the same Tailscale network (the server is `100.64.0.1`).

- **Bind the Tailscale address only**: `8080` listens on `100.64.0.1` only (or `0.0.0.0` plus a firewall that only admits `tailscale0`).
  Not a single port is opened on the public interface.
- **Transport**: plain HTTP, with encryption provided by WireGuard (which is what Tailscale runs on).
  `net.rs:102-105` only supports `http://`, so not a single line of code needs to change.
- **One token**: generated once by `rmcloud pair` and written into the device's `server.json`; the server stores only
  `sha256(token)` and compares in constant time. No expiry, no rotation, no `unpair` subcommand — to change it, delete both
  files and pair again.
- **Pairing code**: 8 decimal digits, 10-minute TTL, single use. This guards against the extreme case of somebody else also
  being on the Tailscale network.
- **Disk**: `/srv/rm` at 0700; logs never print the token (not even a prefix); `state.json` is backed up daily.
- **Do not build**: Caddy/Nginx, 443, self-signed certificates, `update-ca-certificates`, rustls, per-device tokens, rate limiting.
  If public exposure ever becomes necessary, revisit this decision.

### 3.7 Implementation order

**Day 1 (4–6 hours; smallest working loop: pairing + sync + viewing pages in the Web UI)**
1. `rmcloud` skeleton: atomic `state.json` writes + `flock` + the `rmcloud pair` subcommand.
2. Implement #1 `pair`, #2 `health`, #3 `manifest`, #4/#5 `blob`, #6 `commit` (leave mtime/conflicts out for now).
3. Web UI: one `index.html` + `GET /api/v1/tree` + `GET /api/v1/thumb/...` (serving the device's thumbnails directly).
4. Start the service under systemd, bound to the Tailscale IP.
5. **Acceptance**: `rmdev --pair http://100.64.0.1:8080 12345678` → `rmdev --sync-once` prints up/down counts;
   a second run must print `0/0/0`; opening `http://100.64.0.1:8080/` in a browser shows document cards and page thumbnails;
   renaming a document on the device is visible in the Web UI within 20 s.

**Day 2 (half a day: conflict semantics + image reading)**
6. Add the §3.4 mtime rules and the `conflicts` array; write `tests/commit.rs` (all three sides differ, tombstone resurrection, 409 retry).
7. `POST /api/v1/ocr`: wire up dsh per §3.5 (first get a single `dsh ... read_image` command working by hand, then wrap it in HTTP).
   **Acceptance**: open a handwritten page on the device → share panel `Convert to text` → the text shows up on the page;
   tapping the same page again (cache hit) must return within 1 s.
8. `POST /api/v1/mail` (msmtp + App Password); on failure put the raw SMTP text into `detail`.
   **Acceptance**: `Send by email` delivers a mail with the page as PDF.

**Day 3 (optional, as needed)**
9. `POST /api/v1/import`: upload a PDF/EPUB from the web into the device library (a very useful single-user "put books onto the device").
10. `rmcloud gc` (clear orphan blobs) + a daily `rsync` backup to another disk.
11. After 5 minutes of disconnection and a reconnect, the device catches up on sync by itself (this mostly tests the device side).

**Not implemented**: version history/rollback, software update distribution, Google Drive/Dropbox/OneDrive integration, multiple devices, public HTTPS.

## 4. What Has to Change on the Device Side

| File : function | Current state | What to do |
|---|---|---|
| `net.rs:102` `split_url` | only accepts `http://` | **no change**: plain HTTP over Tailscale is enough (§3.6) |
| `net.rs:21` `Config` | `url/token/device_id/last_rev/mail_to` | no change (no `tls_pin` needed) |
| `net.rs:372` `put_blob` | hand-rolled the HTTP request (to add `X-Sha256`) | extract `request_with_headers()`, otherwise every new header means copying the code again (**change**) |
| `net.rs:450` `ocr` | sends the 24-bit BMP straight as a blob (7.9 MB/page) | bandwidth is not a concern over Tailscale, **leave it for now**; to save traffic, encode a 1-bit PNG with `zune_png::Encoder` (about 30–80 KB) |
| `net.rs:363/422` `manifest`+`commit` | already implemented as `rev/entries[]` / `base_rev+puts+dels+conflicts` | no change (**done**; the server has to align with it) |
| `net.rs` (nothing) | no update check | **not implemented** |
| `sync.rs:242` `plan()` | complete three-way comparison, covered by 13 assertions | no change; **it is the only authority on conflict semantics** (**done**) |
| `sync.rs:22` `MAX_FILE` | 32 MB | does not conflict with the server's 64 MB; if large PDFs ever need syncing, raise both together (keep) |
| `sync.rs:442` `conflict_path` | `<rel>.server-conflict-<UTC>` | no change; this document already follows it (**done**) |
| `main.rs:5243` `start_ocr` | hardcodes `"handwriting","en"` | **no change**: dsh detects the language itself, the server ignores `lang` |
| `main.rs:5266` `insert_ocr_text` | inserts the text lines at the bottom of the **current page** | the official app **adds a new page** (`res_066b1b0.qml:79` "Converted text added as new page"); at minimum add undo (**change**) |
| `main.rs:5305` `start_sync` | manual trigger only | wire up `Sync before sleep` (`main.rs:6585` is currently a pure preference that nobody reads), plus auto-sync when returning to the library or the screen turns off (**change**) |
| `main.rs:5613` `start_pair` | only stores url+code | **no change** |
| `main.rs:5492` `send_mail_now` | single attachment, default recipient stored in `Config.mail_to` | the official app allows multiple recipients (`res_06af7c6.qml` "Use space bar to add email address"); the current behaviour is enough (optional) |
| `share.rs:31` `start` | 8000–8009 BMP stream | leave it. The official app uses RFB/WebRTC, which is not replicated (**done**) |
| `src/doc/rm.rs` | `.rm` read/write | leave it. The server **never** creates `.rm`; a blank page simply has no `.rm` (constraint) |

---

## 5. Differences from the Earlier Design (code is authoritative)

| Item | Earlier design | Actual code | Decision |
|---|---|---|---|
| OCR request fields | had `page_width/page_height` | `net.rs:452` sends only 3 fields | the server treats them as **optional**, defaulting to 1404×1872 |
| Conflict copy naming | `<name> (conflict <time>).rmdoc` | `sync.rs:442` `<rel>.server-conflict-<yyyymmdd-HHMMSS>` | code wins; the document was changed |
| `conflicts` element | unspecified | `net.rs:440` parses it as a string array | fixed as an array of path strings |
| Single-file limit | 64 MB | device 32 MB (`sync.rs:22`) | server accepts 64 MB, device rejects above 32 MB |
| `?since=` incremental manifest | "optional optimisation" | `net.rs` never sends it | dropped, not implemented |
| Update endpoint | already designed | **not** implemented on the device side | **not implemented** |
| Version history | `history/<rev>.json` | no device-side dependency | **not implemented** |
| Screen sharing | ports 8000–8009 | `share.rs:31` agrees | kept |
| `state` shape | a single `tree.json` | — | changed to `state.json` (one table of rev + entries) |

Still valid from the earlier notes: the four-file device document format, `pageCount == cPages.pages.len()`,
`sizeInBytes` being a string, and `.content` timestamps being `"cur:sub"`. These are the foundation of the import
endpoint; do not change them.
