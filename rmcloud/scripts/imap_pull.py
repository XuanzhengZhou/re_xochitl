#!/usr/bin/env python3
"""从 163 IMAP 拉「还没处理过」的邮件，把结果以 JSON 数组打到 stdout。

给 rmcloud 的收信模块（`cloud/src/inbox.rs`）用：Rust 侧只做「落 blob / 写 mirror /
记账」，不引 IMAP/MIME 库，脏活都在这儿（只用标准库）。

    imap_pull.py --host imap.163.com --port 993 --user U --pass P \
                 --state ~/rmcloud/inbox-state.json [--outdir /tmp/rm-inbox-<pid>]

规则：
  * 用 UID 游标：`<state>` 里记已处理的最大 UID，下次只拉更大的 UID（`BODY.PEEK[]`
    不改已读标记，所以游标是唯一依据）。
  * 正文只要 text/plain；没有就退到 text/html 去标签。
  * 附件 ≤ 20 MB，写到 `--outdir`（默认 `/tmp/rm-inbox-<pid>`），JSON 里给绝对路径。
  * 一次最多 20 封。
  出错：stderr 打原因、exit 1（stdout 不吐半截 JSON）。
"""

import argparse
import email
import html
import json
import os
import re
import sys
import tempfile
import time
from datetime import datetime, timezone
from email.header import decode_header
from email.utils import parseaddr, parsedate_to_datetime

# 20 MB：太大就不落盘（设备也没法看），但要留个日志。
MAX_ATTACHEMENT = 20 * 1024 * 1024
MAX_MAILS = 20

# 163 强制要求客户端先发 ID，否则 SELECT 会报
# `Unsafe Login. Please contact kefu@188.com`。参数按 RFC 2971 的字段列表给。
ID_ARGS = (
    '("name" "rmcloud" "version" "1.0" '
    '"vendor" "rmcloud" "support-email" "rmcloud@localhost")'
)


def log(msg):
    print(f"[imap_pull] {msg}", file=sys.stderr)


# ---------------------------------------------------------------- 文本解码

def decode_bytes(raw, charset):
    """按 charset 解，解不了就依次退到 utf-8 / gb18030（163 上老中文信常见）。"""
    if not raw:
        return ""
    for cs in (charset, "utf-8", "gb18030", "big5", "latin-1"):
        if not cs:
            continue
        try:
            return raw.decode(cs)
        except (LookupError, UnicodeDecodeError):
            continue
    return raw.decode("utf-8", "replace")


def decode_header_value(value):
    """RFC 2047 头（`=?UTF-8?B?...?=`）→ 普通字符串。"""
    if not value:
        return ""
    out = []
    for text, charset in decode_header(value):
        if isinstance(text, bytes):
            out.append(decode_bytes(text, charset or "utf-8"))
        else:
            out.append(text)
    return "".join(out).strip()


def html_to_text(fragment):
    """够用的去标签：<script>/<style> 整段丢，<br>/<p> 变换行，其余标签抹成空格。"""
    if not fragment:
        return ""
    fragment = re.sub(r"(?is)<(script|style)[^>]*>.*?</\1>", " ", fragment)
    fragment = re.sub(r"(?i)<br\s*/?>", "\n", fragment)
    fragment = re.sub(r"(?i)</(p|div|tr|li|h[1-6])>", "\n", fragment)
    fragment = re.sub(r"<[^>]+>", " ", fragment)
    fragment = html.unescape(fragment)
    fragment = re.sub(r"[ \t\r\f\v]+", " ", fragment)
    fragment = re.sub(r"\n\s*\n\s*\n+", "\n\n", fragment)
    return fragment.strip()


def part_text(part):
    return decode_bytes(part.get_payload(decode=True), part.get_content_charset())


def body_of(msg):
    """正文：优先 text/plain，没有就把 text/html 去标签。"""
    plain, rich = [], []
    for part in msg.walk():
        if part.is_multipart():
            continue
        ctype = (part.get_content_type() or "").lower()
        disposition = (part.get("Content-Disposition") or "").lower()
        if part.get_filename() or "attachment" in disposition:
            continue
        if ctype == "text/plain":
            plain.append(part_text(part))
        elif ctype == "text/html":
            rich.append(part_text(part))
    text = "\n".join(p for p in plain if p.strip())
    if text.strip():
        return text.strip()
    return html_to_text("\n".join(rich))


def date_of(msg):
    """`Date:` → 带时区的 ISO 8601；解析不了就用当前时间（宁可有值别缺字段）。"""
    try:
        when = parsedate_to_datetime(msg.get("Date"))
        if when is None:
            raise ValueError("empty date")
        if when.tzinfo is None:
            when = when.replace(tzinfo=timezone.utc)
        return when.isoformat()
    except Exception:
        return datetime.now().astimezone().isoformat()


# ---------------------------------------------------------------- 附件

def safe_name(raw, index):
    """附件名：只留 basename，去掉控制字符/引号/路径分隔符，别让 `..` 溜出去。"""
    base = (raw or "").replace("\\", "/").rsplit("/", 1)[-1].strip()
    base = "".join(c for c in base if c >= " " and c not in '"')
    base = base.lstrip(".").strip()
    if not base:
        base = f"attachment-{index}"
    return base[:120]


def save_attachment(part, outdir, index):
    """保存一个附件；超过 20 MB 或写盘失败返回 None。"""
    payload = part.get_payload(decode=True) or b""
    size = len(payload)
    if size > MAX_ATTACHEMENT:
        log(f"附件 {part.get_filename()} {size} 字节超过 20 MB，跳过")
        return None
    name = safe_name(part.get_filename(), index)
    dst = os.path.join(outdir, name)
    if os.path.exists(dst):
        stem, ext = os.path.splitext(name)
        dst = os.path.join(outdir, f"{stem}-{index}{ext}")
        name = os.path.basename(dst)
    try:
        os.makedirs(outdir, exist_ok=True)
        with open(dst, "wb") as fh:
            fh.write(payload)
    except OSError as exc:
        log(f"附件 {name} 写盘失败：{exc}")
        return None
    return {"filename": name, "path": os.path.abspath(dst), "size": size}


def attachments_of(msg, outdir):
    items = []
    for index, part in enumerate(msg.walk()):
        if part.is_multipart():
            continue
        disposition = (part.get("Content-Disposition") or "").lower()
        if not part.get_filename() and "attachment" not in disposition:
            continue
        saved = save_attachment(part, outdir, index)
        if saved:
            items.append(saved)
    return items


# ---------------------------------------------------------------- IMAP

def send_id(conn):
    """163 的 `ID` 命令：login 之后、SELECT 之前发。

    用 imaplib 的 `xatom`（扩展命令入口）而不是 `_simple_command`：
    py3.14 的 `_command` 会拿命令名查 `Commands` 表，ID 不在表里就 KeyError；
    `xatom` 会先把 ID 按当前状态注册进去，再按普通命令发。
    """
    typ, data = conn.xatom("ID", ID_ARGS)
    conn._untagged_response(typ, data, "ID")
    if typ != "OK":
        raise RuntimeError(f"ID 命令被拒（{typ}）：{data}")


def load_state(path):
    try:
        with open(path, "r", encoding="utf-8") as fh:
            state = json.load(fh)
    except FileNotFoundError:
        return 0
    except (OSError, ValueError) as exc:
        raise RuntimeError(f"读 state 失败（{path}）：{exc}")
    try:
        return int(state.get("last_uid", 0))
    except (TypeError, ValueError):
        return 0


def save_state(path, last_uid):
    parent = os.path.dirname(os.path.abspath(path))
    os.makedirs(parent, exist_ok=True)
    tmp = f"{path}.tmp"
    with open(tmp, "w", encoding="utf-8") as fh:
        json.dump({"last_uid": int(last_uid), "updated": int(time.time())}, fh)
    os.replace(tmp, path)


def fetch_raw(conn, uid):
    """`BODY.PEEK[]`：拿全文但不把信标成已读。"""
    typ, data = conn.uid("FETCH", str(uid), "(BODY.PEEK[])")
    if typ != "OK":
        raise RuntimeError(f"FETCH UID {uid} 失败：{typ} {data}")
    for item in data or []:
        if isinstance(item, tuple) and len(item) > 1 and isinstance(item[1], bytes):
            return item[1]
    return b""


def pull(args, outdir):
    import imaplib  # 放这儿：--help / import 失败能给出干净的错误

    conn = imaplib.IMAP4_SSL(args.host, args.port)
    try:
        conn.login(args.user, args.password)
        send_id(conn)
        typ, data = conn.select("INBOX", readonly=True)
        if typ != "OK":
            raise RuntimeError(f"SELECT INBOX 失败：{typ} {data}")

        last_uid = load_state(args.state)
        typ, data = conn.uid("SEARCH", None, f"UID {last_uid + 1}:*")
        if typ != "OK":
            raise RuntimeError(f"UID SEARCH 失败：{typ} {data}")
        found = b" ".join(x for x in (data or []) if isinstance(x, bytes))
        # `UID N:*` 在 N 大于最大 UID 时也会返回最后一封，所以必须再筛一次。
        uids = sorted(u for u in (int(x) for x in found.split()) if u > last_uid)
        if len(uids) > MAX_MAILS:
            log(f"本次待处理 {len(uids)} 封，只取前 {MAX_MAILS} 封")
            uids = uids[:MAX_MAILS]

        mails = []
        newest = last_uid
        for uid in uids:
            newest = max(newest, uid)
            try:
                msg = email.message_from_bytes(fetch_raw(conn, uid))
                from_addr = parseaddr(decode_header_value(msg.get("From")))[1]
                mails.append(
                    {
                        "uid": uid,
                        "from": from_addr or decode_header_value(msg.get("From")),
                        "subject": decode_header_value(msg.get("Subject")),
                        "date": date_of(msg),
                        "text": body_of(msg),
                        "attachments": attachments_of(msg, outdir),
                    }
                )
            except Exception as exc:  # 一封解析不了不能拖垮整批
                log(f"UID {uid} 解析失败，跳过：{exc}")

        save_state(args.state, newest)
        return mails
    finally:
        try:
            conn.logout()
        except Exception:
            pass


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="拉 163 收件箱的新邮件，输出 JSON（rmcloud 收信模块用）"
    )
    parser.add_argument("--host", default="imap.163.com")
    parser.add_argument("--port", type=int, default=993)
    parser.add_argument("--user", required=True, help="完整邮箱地址")
    parser.add_argument("--pass", dest="password", required=True, help="163 授权码")
    parser.add_argument("--state", required=True, help="UID 游标文件（JSON）")
    parser.add_argument("--outdir", default=None, help="附件临时目录")
    args = parser.parse_args(argv)

    outdir = args.outdir or os.path.join(tempfile.gettempdir(), f"rm-inbox-{os.getpid()}")
    try:
        os.makedirs(outdir, exist_ok=True)
        mails = pull(args, outdir)
    except Exception as exc:
        log(f"失败：{exc}")
        return 1
    sys.stdout.write(json.dumps(mails, ensure_ascii=False))
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
