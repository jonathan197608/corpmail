#!/usr/bin/env python3
"""CorpMail E2E 用的最小 IMAP4rev1 + SMTP 服务器（仅本机、仅明文）。

为什么不用 Dovecot/Docker：
- 被测客户端只会用配置里的 security=plain 连本机端口（lettre native-tls /
  imap crate 都校验证书，自签证书需要改系统信任库，不适合自动化）
- E2E 需要「可断言的确定性邮箱内容」与「把发出的信落盘做断言」，
  Dovecot 反而更重

实现范围（只覆盖 recv.rs / send.rs / seen.rs 实际用到的命令）：
  IMAP: CAPABILITY / LOGIN / LIST / SELECT / UID SEARCH / UID FETCH /
        UID STORE / NOOP / LOGOUT
  SMTP: EHLO / AUTH PLAIN|LOGIN / MAIL / RCPT / DATA / RSET / QUIT

用法:
  mockmail.py --imap-port 0 --smtp-port 0 --fixtures ./fixtures --outbox /tmp/out
  启动后在 stdout 打印一行 JSON: {"imap": 51234, "smtp": 51235}（端口 0 = 随机）
"""

import argparse
import base64
import email.policy
import json
import os
import re
import socketserver
import sys
import threading
from datetime import datetime
from email.utils import format_datetime
from pathlib import Path

MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
          "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]


def imap_date(dt: datetime) -> str:
    return f"{dt.day:02d}-{MONTHS[dt.month - 1]}-{dt.year}"


def internal_date(dt: datetime) -> str:
    return f"{dt.day:02d}-{MONTHS[dt.month - 1]}-{dt.year} {dt:%H:%M:%S} +0800"


class Mailbox:
    """单邮箱内存存储（uid 从 1 递增，uidvalidity 固定 1）"""

    def __init__(self, name: str):
        self.name = name
        self.mails = []  # {uid, raw: bytes, flags: set[str], date: datetime}
        self.uidvalidity = 1

    def add(self, raw: bytes, flags=None, date=None) -> int:
        uid = len(self.mails) + 1
        self.mails.append({
            "uid": uid,
            "raw": raw,
            "flags": set(flags or []),
            "date": date or datetime.now(),
        })
        return uid

    def by_uid(self, uid: int):
        for m in self.mails:
            if m["uid"] == uid:
                return m
        return None


class Store:
    def __init__(self):
        self.inbox = Mailbox("INBOX")
        self.sent = Mailbox("Sent")
        self.lock = threading.Lock()

    def mailbox(self, name: str) -> Mailbox:
        key = name.strip('"').upper()
        if key in ("INBOX",):
            return self.inbox
        if key in ("SENT", "SENT ITEMS", "已发送", "SENT MESSAGES"):
            return self.sent
        return None


# --------------------------------------------------------------------------
# IMAP
# --------------------------------------------------------------------------

class ImapHandler(socketserver.StreamRequestHandler):
    store: Store = None  # 由 main 注入

    def setup(self):
        super().setup()
        self.selected = None
        self.authenticated = False

    def send(self, line: str):
        self.wfile.write((line + "\r\n").encode())
        self.wfile.flush()

    def send_bytes(self, data: bytes):
        self.wfile.write(data)
        self.wfile.flush()

    def handle(self):
        self.send("* OK [CAPABILITY IMAP4rev1] CorpMail E2E mock ready")
        while True:
            raw = self.rfile.readline()
            if not raw:
                return
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            if not line:
                continue
            try:
                if not self.dispatch(line):
                    return
            except Exception as exc:  # noqa: BLE001 - mock 服务器：报错不静默断连
                print(f"[imap] error: {exc}", file=sys.stderr, flush=True)
                self.send(f"* BAD {exc}")
                return

    # -- 命令分发 ----------------------------------------------------------
    def dispatch(self, line: str) -> bool:
        tag, _, rest = line.partition(" ")
        cmd, _, args = rest.partition(" ")
        cmd = cmd.upper()

        if cmd == "CAPABILITY":
            self.send("* CAPABILITY IMAP4rev1")
            self.send(f"{tag} OK CAPABILITY completed")
        elif cmd == "NOOP":
            self.send(f"{tag} OK NOOP completed")
        elif cmd == "LOGIN":
            # LOGIN user pass（密码可能带引号；mock 不校验）
            self.authenticated = True
            self.send(f"{tag} OK LOGIN completed")
        elif cmd == "LIST":
            self.send('* LIST (\\HasNoChildren) "/" "INBOX"')
            self.send('* LIST (\\HasNoChildren \\Sent) "/" "Sent"')
            self.send(f"{tag} OK LIST completed")
        elif cmd == "SELECT" or cmd == "EXAMINE":
            return self.cmd_select(tag, args)
        elif cmd == "UID":
            return self.cmd_uid(tag, args)
        elif cmd == "LOGOUT":
            self.send("* BYE logging out")
            self.send(f"{tag} OK LOGOUT completed")
            return False
        else:
            # 未实现命令一律 OK（ID / ENABLE / NAMESPACE 等可选命令）
            self.send(f"{tag} OK {cmd} completed")
        return True

    def cmd_select(self, tag: str, args: str) -> bool:
        name = args.strip()
        box = self.store.mailbox(name)
        if box is None:
            self.send(f"{tag} NO [NONEXISTENT] unknown mailbox")
            return True
        self.selected = box
        self.send(f"* {len(box.mails)} EXISTS")
        self.send("* 0 RECENT")
        self.send("* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)")
        self.send(f"* OK [UIDVALIDITY {box.uidvalidity}] UIDs valid")
        self.send(f"* OK [UIDNEXT {len(box.mails) + 1}] Predicted next UID")
        self.send(f"{tag} OK [READ-WRITE] SELECT completed")
        return True

    def cmd_uid(self, tag: str, args: str) -> bool:
        sub, _, rest = args.partition(" ")
        sub = sub.upper()
        if sub == "SEARCH":
            return self.uid_search(tag, rest)
        if sub == "FETCH":
            return self.uid_fetch(tag, rest)
        if sub == "STORE":
            return self.uid_store(tag, rest)
        self.send(f"{tag} OK UID {sub} completed")
        return True

    def uid_search(self, tag: str, criteria: str) -> bool:
        box = self.selected
        if box is None:
            self.send(f"{tag} BAD no mailbox selected")
            return True
        crit = criteria.strip()
        uids = [m["uid"] for m in box.mails]
        if crit.upper().startswith("SINCE "):
            token = crit[6:].strip().strip('"')
            since = self.parse_date(token)
            uids = [m["uid"] for m in box.mails if m["date"].date() >= since]
        elif crit.upper() == "ALL":
            pass
        elif re.fullmatch(r"\d+:\*", crit):
            start = int(crit.split(":")[0])
            uids = [u for u in uids if u >= start]
        elif re.fullmatch(r"[\d,:]+", crit):
            want = set()
            for part in crit.split(","):
                if "-" in part:
                    a, b = part.split("-", 1)
                    want.update(range(int(a), int(b) + 1))
                else:
                    want.add(int(part))
            uids = [u for u in uids if u in want]
        self.send("* SEARCH" + "".join(f" {u}" for u in uids))
        self.send(f"{tag} OK SEARCH completed")
        return True

    @staticmethod
    def parse_date(token: str):
        for fmt in ("%d-%b-%Y", "%Y-%m-%d"):
            try:
                return datetime.strptime(token, fmt).date()
            except ValueError:
                continue
        return datetime(1970, 1, 1).date()

    @staticmethod
    def uid_set(text: str, max_uid: int):
        """解析 UID 集合；`*` 取邮箱当前最大 UID（不能展开成十亿个整数）"""
        out = []
        for part in text.split(","):
            part = part.strip()
            if not part:
                continue
            if ":" in part:
                a, b = part.split(":", 1)
                a = int(a)
                b = max_uid if b == "*" else int(b)
                out.extend(range(min(a, b), max(a, b) + 1))
            else:
                out.append(int(part))
        return out

    def uid_fetch(self, tag: str, args: str) -> bool:
        box = self.selected
        if box is None:
            self.send(f"{tag} BAD no mailbox selected")
            return True
        m = re.match(r"(\S+)\s+(?:\((.*)\)|(\S+))$", args.strip())
        if not m:
            self.send(f"{tag} BAD fetch syntax")
            return True
        uids = self.uid_set(m.group(1), max((x['uid'] for x in box.mails), default=0))
        items = (m.group(2) or m.group(3) or "").upper()
        for uid in uids:
            mail = box.by_uid(uid)
            if mail is None:
                continue
            parts = [f"UID {uid}"]
            if "FLAGS" in items:
                flags = " ".join(sorted(mail["flags"])) if mail["flags"] else ""
                parts.append(f"FLAGS ({flags})")
            if "INTERNALDATE" in items:
                parts.append(f'INTERNALDATE "{internal_date(mail["date"])}"')
            if "RFC822.SIZE" in items:
                parts.append(f"RFC822.SIZE {len(mail['raw'])}")
            literal = None
            if "BODY.PEEK[HEADER]" in items or "BODY[HEADER]" in items:
                head = mail["raw"].split(b"\r\n\r\n", 1)[0] + b"\r\n\r\n"
                literal = head
                parts.append("BODY[HEADER]")
            elif "BODY.PEEK[]" in items or "BODY[]" in items or "RFC822" == items:
                literal = mail["raw"]
                parts.append("BODY[]")
            if literal is None:
                self.send(f"* {uid} FETCH ({' '.join(parts)})")
                continue
            prefix = f"* {uid} FETCH ({' '.join(parts)} {{{len(literal)}}}\r\n"
            self.send_bytes(prefix.encode() + literal + b")\r\n")
        self.send(f"{tag} OK FETCH completed")
        return True

    def uid_store(self, tag: str, args: str) -> bool:
        box = self.selected
        if box is None:
            self.send(f"{tag} BAD no mailbox selected")
            return True
        m = re.match(r"(\S+)\s+([+-]?FLAGS(?:\.SILENT)?)\s+\((.*?)\)\s*$", args.strip(), re.I)
        if not m:
            self.send(f"{tag} BAD store syntax")
            return True
        uids = self.uid_set(m.group(1), max((x['uid'] for x in box.mails), default=0))
        op = m.group(2).upper()
        flags = set(m.group(3).split())
        for uid in uids:
            mail = box.by_uid(uid)
            if mail is None:
                continue
            if op.startswith("+"):
                mail["flags"] |= flags
            elif op.startswith("-"):
                mail["flags"] -= flags
            else:
                mail["flags"] = flags
            if ".SILENT" not in op:
                fs = " ".join(sorted(mail["flags"]))
                self.send(f"* {uid} FETCH (UID {uid} FLAGS ({fs}))")
        self.send(f"{tag} OK STORE completed")
        return True


# --------------------------------------------------------------------------
# SMTP
# --------------------------------------------------------------------------

class SmtpHandler(socketserver.StreamRequestHandler):
    store: Store = None
    outbox: Path = None

    def setup(self):
        super().setup()
        self.mail_from = None
        self.rcpts = []

    def send(self, line: str):
        self.wfile.write((line + "\r\n").encode())
        self.wfile.flush()

    def handle(self):
        self.send("220 corpmmail-e2e ESMTP ready")
        while True:
            raw = self.rfile.readline()
            if not raw:
                return
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            up = line.upper()
            if up.startswith("EHLO") or up.startswith("HELO"):
                self.send("250-corpmmail-e2e")
                self.send("250-AUTH PLAIN LOGIN")
                self.send("250 SIZE 52428800")
            elif up.startswith("AUTH PLAIN"):
                if line.strip().upper() == "AUTH PLAIN":
                    self.rfile.readline()  # 客户端随后单行发送 base64
                self.send("235 2.7.0 Authentication successful")
            elif up.startswith("AUTH LOGIN"):
                self.send("334 VXNlcm5hbWU6")
                self.rfile.readline()
                self.send("334 UGFzc3dvcmQ6")
                self.rfile.readline()
                self.send("235 2.7.0 Authentication successful")
            elif up.startswith("MAIL FROM"):
                self.mail_from = line
                self.rcpts = []
                self.send("250 2.1.0 OK")
            elif up.startswith("RCPT TO"):
                addr = line.split("<", 1)[1].split(">", 1)[0] if "<" in line else ""
                # 失败链路注入：fail@ 开头的收件人一律 550 拒收，发送端走完整
                # 的 failed 状态机（不污染配置、不依赖停机）。成功链路不受影响。
                if addr.lower().startswith("fail"):
                    self.send("550 5.1.1 mailbox unavailable（E2E 注入失败链路）")
                else:
                    self.rcpts.append(line)
                    self.send("250 2.1.5 OK")
            elif up == "DATA":
                self.send("354 End data with <CR><LF>.<CR><LF>")
                chunks = []
                while True:
                    chunk = self.rfile.readline()
                    if not chunk:
                        return
                    if chunk in (b".\r\n", b".\n"):
                        break
                    if chunk.startswith(b".."):
                        chunk = chunk[1:]
                    chunks.append(chunk)
                self.save(b"".join(chunks))
                self.send("250 2.0.0 OK queued")
            elif up == "RSET":
                self.mail_from, self.rcpts = None, []
                self.send("250 2.0.0 OK")
            elif up == "QUIT":
                self.send("221 2.0.0 Bye")
                return
            else:
                self.send("250 2.0.0 OK")

    def save(self, raw: bytes):
        out = self.outbox
        out.mkdir(parents=True, exist_ok=True)
        name = f"sent-{datetime.now():%Y%m%d-%H%M%S-%f}.eml"
        (out / name).write_bytes(raw)
        # 发件箱也留一份：客户端「已发送」同步用例可直接断言
        with self.store.lock:
            self.store.sent.add(raw, flags=[], date=datetime.now())
        print(f"[smtp] captured {name} ({len(raw)} bytes)", flush=True)


class ThreadedServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


def load_fixtures(store: Store, fixtures: Path):
    files = sorted(fixtures.glob("*.eml")) if fixtures.is_dir() else []
    for idx, path in enumerate(files):
        raw = path.read_bytes()
        # fixture 文件名前缀数字用于固定 INTERNALDATE 间隔，保证列表排序稳定
        date = datetime(2026, 11, 1 + idx, 10, 0, 0)
        store.inbox.add(raw, flags=["\\Seen"] if idx % 2 else [], date=date)
    print(f"[mock] loaded {len(files)} fixture mails", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--imap-port", type=int, default=0)
    ap.add_argument("--smtp-port", type=int, default=0)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--fixtures", default=str(Path(__file__).parent / "fixtures"))
    ap.add_argument("--outbox", default=str(Path(__file__).parent / "outbox"))
    args = ap.parse_args()

    store = Store()
    load_fixtures(store, Path(args.fixtures))
    outbox = Path(args.outbox)

    ImapHandler.store = store
    SmtpHandler.store = store
    SmtpHandler.outbox = outbox

    imap = ThreadedServer((args.host, args.imap_port), ImapHandler)
    smtp = ThreadedServer((args.host, args.smtp_port), SmtpHandler)
    threading.Thread(target=imap.serve_forever, daemon=True).start()
    threading.Thread(target=smtp.serve_forever, daemon=True).start()

    print(json.dumps({
        "imap": imap.server_address[1],
        "smtp": smtp.server_address[1],
        "uidvalidity": store.inbox.uidvalidity,
        "mails": len(store.inbox.mails),
    }), flush=True)

    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
