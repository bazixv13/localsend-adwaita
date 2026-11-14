#!/usr/bin/env python3
"""Protocol probes used by scripts/check-runtime.sh.

Each command drives the running Flatpak over the LocalSend v2 protocol on
loopback and/or inspects a dbus-monitor log of org.gtk.Notifications.

Commands:
  progress <port>          start and stream a 700 kB transfer
  abandon  <port>          ask for a session and never upload anything
  text     <port> <text>   send one text message (as LocalSend peers do: preview only)
  verify-progress <log>    assert one reused id, low-priority progress, one result
  verify-abandon <log>     assert the idle cleanup reported a failure
  summary <log>            print the notifications seen in the log

Exit status is non-zero when an assertion fails.
"""
import http.client
import json
import re
import ssl
import sys
import time
import uuid

TIMEOUT = 30
CHUNK = 35_000
CHUNK_DELAY = 0.1


def _ctx():
    ctx = ssl.create_default_context()
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    return ctx


def _conn(port):
    return http.client.HTTPSConnection("127.0.0.1", port, context=_ctx(), timeout=TIMEOUT)


def _prepare(port, alias, files):
    info = {
        "alias": alias,
        "version": "2.2",
        "deviceModel": "probe",
        "deviceType": "desktop",
        "fingerprint": "probe-" + uuid.uuid4().hex[:8],
        "port": port,
        "protocol": "http",
        "download": False,
    }
    body = json.dumps({"info": info, "files": files}).encode()
    conn = _conn(port)
    conn.request(
        "POST", "/api/localsend/v2/prepare-upload", body, {"Content-Type": "application/json"}
    )
    resp = conn.getresponse()
    payload = resp.read()
    return resp.status, (json.loads(payload) if payload else {})


def _upload(port, session, file_id, token, data, chunk=CHUNK, delay=CHUNK_DELAY):
    conn = _conn(port)
    conn.putrequest(
        "POST", f"/api/localsend/v2/upload?sessionId={session}&fileId={file_id}&token={token}"
    )
    conn.putheader("Content-Type", "application/octet-stream")
    conn.putheader("Content-Length", str(len(data)))
    conn.endheaders()
    for start in range(0, len(data), chunk):
        conn.send(data[start : start + chunk])
        if delay:
            time.sleep(delay)
    return conn.getresponse().status


def _binary(size, name="probe.bin"):
    return {"f1": {"id": "f1", "fileName": name, "size": size,
                   "fileType": "application/octet-stream"}}


def cmd_progress(port, name="probe.bin"):
    size = 700_000
    status, token = _prepare(port, "Probe Sender", _binary(size, name))
    if status != 200:
        print(f"prepare failed: {status} {token}", file=sys.stderr)
        return 2
    session = token["sessionId"]
    upload = _upload(port, session, "f1", token["files"]["f1"], b"\0" * size)
    print(json.dumps({"prepare": status, "upload": upload}))
    return 0 if upload == 200 else 2


def cmd_abandon(port):
    status, token = _prepare(port, "Abandoning Probe", _binary(5000))
    if status != 200:
        print(f"prepare failed: {status} {token}", file=sys.stderr)
        return 2
    print(json.dumps({"prepare": status, "upload": "never"}))
    return 0


def cmd_text(port, payload):
    # Real peers (and this app) announce a message as a `text` pseudo-file whose
    # body lives in `preview`; nothing is uploaded afterwards.
    files = {
        "1": {
            "id": "1",
            "fileName": "message.txt",
            "size": len(payload.encode()),
            "fileType": "text",
            "preview": payload,
        }
    }
    status, body = _prepare(port, "Text Probe", files)
    if status != 204:
        print(f"prepare failed: {status} {body}", file=sys.stderr)
    print(json.dumps({"prepare": status}))
    return 0 if status == 204 else 2


def _blocks(log_path, member):
    with open(log_path, encoding="utf-8", errors="replace") as fh:
        text = fh.read()
    parts = re.split(r"^method call .*member=(AddNotification|RemoveNotification)\s*$",
                     text, flags=re.MULTILINE)
    out = []
    for i in range(1, len(parts), 2):
        if parts[i] == member:
            out.append(parts[i + 1])
    return out


def _field(block, name):
    m = re.search(r'string "%s"\s*\n\s*variant\s+string "((?:[^"\\]|\\.)*)"' % name, block)
    return m.group(1) if m else None


def _notifications(log_path):
    found = []
    for block in _blocks(log_path, "AddNotification"):
        ids = re.findall(r'^\s+string "([^"]*)"\s*$', block, flags=re.MULTILINE)
        found.append({
            "app": ids[0] if ids else None,
            "id": ids[1] if len(ids) > 1 else None,
            "title": _field(block, "title"),
            "body": _field(block, "body"),
            "priority": _field(block, "priority"),
        })
    return found


def cmd_summary(log_path):
    for n in _notifications(log_path):
        print(f"{n['id']:>12}  {str(n['priority']):<7}  {n['title']}  |  {n['body']}")
    return 0


def cmd_verify_progress(log_path):
    notes = _notifications(log_path)
    errors = []
    ids = {n["id"] for n in notes}
    if ids != {"localsend"}:
        errors.append(f"expected a single notification id, saw {sorted(ids)}")
    receiving = [n for n in notes if n["title"] == "Receiving"]
    if len(receiving) < 3:
        errors.append(f"expected >=3 in-place progress updates, saw {len(receiving)}")
    loud = [n for n in receiving if n["priority"] != "low"]
    if loud:
        errors.append(f"{len(loud)} progress notifications were not low priority")
    results = [n for n in notes if (n["title"] or "").startswith("Received from")]
    if len(results) != 1:
        errors.append(f"expected exactly 1 result notification, saw {len(results)}")
    elif results[0]["priority"] != "normal":
        errors.append(f"result notification priority was {results[0]['priority']}")
    for note in notes:
        if note["app"] != "io.github.localsend_adwaita.LocalSend":
            errors.append(f"unexpected app id {note['app']}")
            break
    print(f"{len(receiving)} progress updates, {len(results)} result: "
          f"{results[0]['body'] if results else '-'}")
    for err in errors:
        print(f"  ! {err}", file=sys.stderr)
    return 1 if errors else 0


def cmd_verify_abandon(log_path):
    notes = _notifications(log_path)
    failures = [n for n in notes if n["title"] == "Transfer failed"]
    if not failures:
        print("  ! no 'Transfer failed' notification", file=sys.stderr)
        return 1
    body = failures[-1]["body"] or ""
    if "stopped responding" not in body:
        print(f"  ! unexpected failure body: {body}", file=sys.stderr)
        return 1
    print(f"reported after idle timeout: {body}")
    return 0


def main(argv):
    if len(argv) < 2:
        print(__doc__, file=sys.stderr)
        return 2
    cmd = argv[1]
    if cmd == "progress":
        return cmd_progress(int(argv[2]) if len(argv) > 2 else 53317,
                            argv[3] if len(argv) > 3 else "probe.bin")
    if cmd == "abandon":
        return cmd_abandon(int(argv[2]) if len(argv) > 2 else 53317)
    if cmd == "text":
        return cmd_text(int(argv[2]), argv[3])
    if cmd == "summary":
        return cmd_summary(argv[2])
    if cmd == "verify-progress":
        return cmd_verify_progress(argv[2])
    if cmd == "verify-abandon":
        return cmd_verify_abandon(argv[2])
    print(f"unknown command: {cmd}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv))
