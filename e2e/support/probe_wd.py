#!/usr/bin/env python3
"""Embedded WebDriver 快速诊断（不经过 wdio，用来判断链路是否通）。

用途：首次接入或升级 Tauri/插件版本后，先用它确认
  1) 应用内 WebDriver 端口是否监听
  2) /session 能否建立
  3) 元素查询与 screenshot 是否真的可用
这三步都通过再跑 wdio，能快速区分「环境问题」与「用例问题」。

用法:
  python3 support/probe_wd.py --bin ../src-tauri/target/debug/corpmail \
      --home artifacts/diag-home --port 4447 --out artifacts/diag.png
"""

import argparse
import base64
import json
import os
import pathlib
import subprocess
import time
import urllib.request


def req(port, method, path, body=None, timeout=25):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}",
        data=data,
        method=method,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(r, timeout=timeout) as resp:
        return json.loads(resp.read().decode())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True, help="带 e2e feature 的应用二进制")
    ap.add_argument("--home", required=True, help="隔离的 HOME（避免污染真实数据）")
    ap.add_argument("--port", type=int, default=4447)
    ap.add_argument("--out", default="artifacts/diag.png")
    args = ap.parse_args()

    home = pathlib.Path(args.home).resolve()
    (home / "Library/Application Support").mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, HOME=str(home), TAURI_WEBDRIVER_PORT=str(args.port))
    proc = subprocess.Popen(
        [args.bin], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True
    )

    try:
        ready = False
        for i in range(25):
            try:
                st = req(args.port, "GET", "/status")
                if st.get("value", {}).get("ready"):
                    print(f"✓ WebDriver ready（{i * 2}s）")
                    ready = True
                    break
            except Exception:
                pass
            time.sleep(2)
        if not ready:
            print("✗ WebDriver 未就绪：应用可能启动失败（看下面日志）")

        sess = req(
            args.port, "POST", "/session",
            {"capabilities": {"alwaysMatch": {"browserName": "tauri"}}},
        )
        sid = sess.get("value", {}).get("sessionId") or sess.get("sessionId")
        print("✓ session:", sid)
        print("  title:", req(args.port, "GET", f"/session/{sid}/title")["value"])

        el = req(
            args.port, "POST", f"/session/{sid}/element",
            {"using": "css selector", "value": "body"},
        )
        print("✓ element:", bool(el.get("value")))

        shot = req(args.port, "GET", f"/session/{sid}/screenshot")
        raw = base64.b64decode(shot["value"])
        pathlib.Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        pathlib.Path(args.out).write_bytes(raw)
        print(f"✓ screenshot: {len(raw)} bytes -> {args.out}")
    finally:
        proc.terminate()
        try:
            log = proc.communicate(timeout=20)[0]
        except Exception:
            proc.kill()
            log = ""
        if log:
            print("--- app log tail ---")
            print(log[-1500:])


if __name__ == "__main__":
    main()
