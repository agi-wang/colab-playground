#!/usr/bin/env python3
"""W-ID device login and encrypted Colab account transfer.

Run with the Colab CLI's Python interpreter so it uses the same OS keyring.
No Google credential is printed or stored in a plaintext file.
"""

import argparse
import base64
from collections import deque
import getpass
import hashlib
import json
import os
from pathlib import Path
import secrets
import sys
import time
from urllib.error import HTTPError
from urllib.parse import urlencode
from urllib.request import Request, urlopen
import webbrowser

import keyring
from cryptography.hazmat.primitives.ciphers.aead import AESGCM


ISSUER = "https://auth.myfastools.com"
CLIENT_ID = "client_655cda8f9e234c6d8a6a84f70cc1ac19"
SYNC_URL = os.environ.get("COLAB_PLAYGROUND_SYNC_URL", ISSUER + "/colab-sync")
AUTH_SERVICE = "colab-playground-w-id"
VAULT_SERVICE = "colab-playground"
MAX_SNAPSHOT = 1024 * 1024


def request(url, *, method="GET", data=None, token=None, etag=None):
    headers = {"Accept": "application/json", "User-Agent": "Colab-Playground-Sync/0.5"}
    if token:
        headers["Authorization"] = "Bearer " + token
    if etag is not None:
        headers["If-Match"] = etag
    if isinstance(data, dict):
        data = urlencode(data).encode()
        headers["Content-Type"] = "application/x-www-form-urlencoded"
    elif data is not None:
        headers["Content-Type"] = "application/octet-stream"
    try:
        with urlopen(Request(url, data=data, headers=headers, method=method), timeout=15) as response:
            return response.status, response.headers.get("ETag"), response.read(MAX_SNAPSHOT + 1)
    except HTTPError as error:
        return error.code, error.headers.get("ETag"), error.read(512)


def oauth_begin():
    status, _, body = request(ISSUER + "/oauth/device_authorization", method="POST", data={
        "client_id": CLIENT_ID, "scope": "openid profile email offline_access",
    })
    if status != 200:
        raise RuntimeError("W-ID device authorization failed: HTTP %d" % status)
    flow = json.loads(body)
    keyring.set_password(AUTH_SERVICE, "device_flow", json.dumps(flow))
    return flow


def oauth_finish(flow):
    if isinstance(flow, str):
        flow = json.loads(flow)
    deadline = time.monotonic() + flow["expires_in"]
    interval = max(5, flow.get("interval", 5))
    while time.monotonic() < deadline:
        time.sleep(interval)
        status, _, body = request(ISSUER + "/oauth/token", method="POST", data={
            "grant_type": "urn:ietf:params:oauth:grant-type:device_code",
            "device_code": flow["device_code"], "client_id": CLIENT_ID,
        })
        reply = json.loads(body)
        if status == 200 and reply.get("access_token"):
            save_auth(reply)
            keyring.delete_password(AUTH_SERVICE, "device_flow")
            return reply["access_token"]
        if reply.get("error") == "slow_down":
            interval += 5
        elif reply.get("error") != "authorization_pending":
            raise RuntimeError("W-ID login failed: " + str(reply.get("error", status)))
    raise RuntimeError("W-ID login timed out")


def oauth_login():
    flow = oauth_begin()
    print("Open this W-ID page and approve the device: " + flow["verification_uri_complete"])
    print("Code: " + flow["user_code"])
    webbrowser.open(flow["verification_uri_complete"])
    return oauth_finish(flow)


def save_auth(tokens):
    keyring.set_password(AUTH_SERVICE, "tokens", json.dumps(tokens))


def access_token(allow_login=True):
    saved = keyring.get_password(AUTH_SERVICE, "tokens")
    if saved:
        tokens = json.loads(saved)
        refresh = tokens.get("refresh_token")
        if refresh:
            status, _, body = request(ISSUER + "/oauth/token", method="POST", data={
                "grant_type": "refresh_token", "client_id": CLIENT_ID, "refresh_token": refresh,
            })
            if status == 200:
                updated = json.loads(body)
                if updated.get("access_token"):
                    if not updated.get("refresh_token"):
                        updated["refresh_token"] = refresh
                    save_auth(updated)
                    return updated["access_token"]
            elif status not in (400, 401):
                raise RuntimeError("W-ID token refresh failed: HTTP %d" % status)
    if not allow_login:
        raise RuntimeError("W-ID login required")
    return oauth_login()


def unlock(token, pin):
    status, _, body = request(SYNC_URL + "/v1/unlock", method="POST", data=json.dumps({"pin": pin}).encode(), token=token)
    if status not in (200, 201):
        raise RuntimeError("PIN unlock failed: HTTP %d" % status)
    share = base64.b64decode(body, validate=True)
    if len(share) != 32:
        raise RuntimeError("Invalid server key share")
    return hashlib.sha256(b"colab-playground-sync-v1\0" + share + pin.encode()).digest()


def data_path():
    override = os.environ.get("COLAB_PLAYGROUND_DATA_DIR")
    if override:
        return Path(override) / "accounts.json"
    if sys.platform == "darwin":
        return Path.home() / "Library/Application Support/colab-playground/accounts.json"
    if sys.platform == "win32":
        return Path(os.environ["APPDATA"]) / "colab-playground/accounts.json"
    return Path.home() / ".local/share/colab-playground/accounts.json"


def sync_log_path():
    return data_path().with_name("sync-log.jsonl")


def local_sync_log():
    try:
        with sync_log_path().open(encoding="utf-8") as stream:
            lines = deque(stream, maxlen=20)
    except FileNotFoundError:
        return [], False
    except OSError:
        return [], True
    try:
        events = [json.loads(line) for line in lines]
        if any(event.get("action") not in ("push", "pull")
               or event.get("result") not in ("success", "failed")
               or not isinstance(event.get("at"), int) for event in events):
            raise ValueError("invalid sync log")
        return events[::-1], False
    except (ValueError, AttributeError):
        return [], True


def record_sync(action, result):
    path = sync_log_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
        with os.fdopen(fd, "ab") as stream:
            stream.write((json.dumps({"action": action, "result": result, "at": int(time.time())}) + "\n").encode())
    except OSError:
        pass  # A log write must not turn a completed transfer into a reported failure.


def history():
    local, local_error = local_sync_log()
    login, login_error = [], False
    try:
        token = access_token(False)
        status, _, body = request(ISSUER + "/oauth/login-history", token=token)
        if status != 200:
            raise RuntimeError("W-ID login history unavailable")
        login = json.loads(body)["events"]
        if not isinstance(login, list):
            raise ValueError("invalid W-ID login history")
    except Exception:
        login_error = True
    return {"login": login, "local": local, "login_error": login_error, "local_error": local_error}


def local_accounts():
    path = data_path()
    if not path.exists():
        return {"version": 1, "accounts": []}
    data = json.loads(path.read_text())
    if data.get("version") != 1 or not isinstance(data.get("accounts"), list):
        raise RuntimeError("Unsupported accounts.json")
    return data


def vault(accounts=None):
    entries = []
    for account in accounts if accounts is not None else local_accounts()["accounts"]:
        account_id = account["id"]
        if not isinstance(account_id, str) or not account_id.isascii() or not account_id.replace("-", "").isalnum():
            raise RuntimeError("Invalid account ID")
        credential = keyring.get_password(VAULT_SERVICE, "account:" + account_id)
        if credential:
            entries.append({"account": account, "credential": credential})
    return entries


def encrypt(entries, key):
    nonce = secrets.token_bytes(12)
    plain = json.dumps({"version": 1, "entries": entries}).encode()
    return b"CPG1" + nonce + AESGCM(key).encrypt(nonce, plain, b"colab-playground-sync-v1")


def decrypt(blob, key):
    if len(blob) < 32 or not blob.startswith(b"CPG1"):
        raise RuntimeError("Unsupported encrypted snapshot")
    plain = AESGCM(key).decrypt(blob[4:16], blob[16:], b"colab-playground-sync-v1")
    data = json.loads(plain)
    if data.get("version") != 1 or not isinstance(data.get("entries"), list):
        raise RuntimeError("Unsupported snapshot version")
    return data["entries"]


def remote(token, key):
    status, etag, body = request(SYNC_URL + "/v1/snapshot", token=token)
    if status == 404:
        return [], "*"
    if status != 200 or len(body) > MAX_SNAPSHOT:
        raise RuntimeError("Snapshot download failed: HTTP %d" % status)
    return decrypt(body, key), etag


def merged(local, incoming):
    result = {entry["account"]["id"]: entry for entry in local}
    emails = {email.lower() for entry in local if (email := entry["account"].get("email"))}
    for entry in incoming:
        account = entry["account"]
        account_id = account["id"]
        email = account.get("email")
        if account_id not in result and (not email or email.lower() not in emails):
            result[account_id] = entry
            if email:
                emails.add(email.lower())
    return list(result.values())


def restore(entries, existing=None):
    app_managed = existing is not None
    existing = existing if existing is not None else local_accounts()
    known = {account["id"] for account in existing["accounts"]}
    emails = {email.lower() for account in existing["accounts"] if (email := account.get("email"))}
    count = 0
    added = []
    for entry in entries:
        account = entry["account"]
        credential = entry["credential"]
        account_id = account["id"]
        email = account.get("email")
        if account_id in known or (email and email.lower() in emails):
            continue
        if not isinstance(account_id, str) or not account_id.isascii() or not account_id.replace("-", "").isalnum() or len(account_id) > 32:
            raise RuntimeError("Invalid account ID in snapshot")
        if not isinstance(credential, str) or not credential:
            raise RuntimeError("Invalid saved credential")
        keyring.set_password(VAULT_SERVICE, "account:" + account_id, credential)
        existing["accounts"].append(account)
        added.append(account)
        known.add(account_id)
        if email:
            emails.add(email.lower())
        count += 1
    if count and not app_managed:
        path = data_path()
        path.parent.mkdir(parents=True, exist_ok=True)
        temp = path.with_name("." + path.name + ".sync-" + secrets.token_hex(4))
        fd = os.open(temp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        try:
            with os.fdopen(fd, "w") as out:
                json.dump(existing, out, indent=2)
                out.flush()
                os.fsync(out.fileno())
            os.replace(temp, path)
        finally:
            if temp.exists():
                temp.unlink()
    return added


def main():
    parser = argparse.ArgumentParser(description="Sync Colab Playground accounts through W-ID")
    parser.add_argument("command", choices=("login", "status", "history", "begin", "finish", "push", "pull"))
    parser.add_argument("--pin-stdin", action="store_true")
    parser.add_argument("--no-login", action="store_true")
    parser.add_argument("--app-managed", action="store_true")
    args = parser.parse_args()
    if args.command == "login":
        oauth_login()
        print("W-ID login complete")
        return
    if args.command == "status":
        try:
            token = access_token(False)
        except RuntimeError as error:
            if str(error) != "W-ID login required":
                raise
            print(json.dumps({"signed_in": False, "email": None}))
            return
        status, _, body = request(ISSUER + "/oauth/userinfo", token=token)
        if status == 401:
            print(json.dumps({"signed_in": False, "email": None}))
            return
        if status != 200:
            raise RuntimeError("W-ID status failed: HTTP %d" % status)
        user = json.loads(body)
        email = user.get("email")
        print(json.dumps({"signed_in": True, "email": email if isinstance(email, str) else None}))
        return
    if args.command == "history":
        print(json.dumps(history()))
        return
    if args.command == "begin":
        flow = oauth_begin()
        print(json.dumps({"url": flow["verification_uri_complete"], "code": flow["user_code"]}))
        return
    if args.command == "finish":
        flow = keyring.get_password(AUTH_SERVICE, "device_flow")
        if not flow:
            raise RuntimeError("No W-ID login is pending")
        oauth_finish(flow)
        print("W-ID login complete")
        return
    try:
        app_input = json.load(sys.stdin) if args.app_managed else None
        token = access_token(not args.no_login)
        pin = app_input["pin"] if app_input is not None else (sys.stdin.readline().rstrip("\n") if args.pin_stdin else getpass.getpass("Six-digit sync PIN: "))
        if len(pin) != 6 or not pin.isascii() or not pin.isdecimal():
            raise RuntimeError("PIN must contain exactly six digits")
        key = unlock(token, pin)
        incoming, etag = remote(token, key)
        if args.command == "pull":
            added = restore(incoming, app_input["accounts"] if app_input is not None else None)
            record_sync("pull", "success")
            print(json.dumps({"added": added}) if app_input is not None else "Restored %d account(s)" % len(added))
        else:
            entries = merged(vault(app_input["accounts"]["accounts"] if app_input is not None else None), incoming)
            blob = encrypt(entries, key)
            if len(blob) > MAX_SNAPSHOT:
                raise RuntimeError("Encrypted snapshot exceeds 1 MiB")
            status, _, _ = request(SYNC_URL + "/v1/snapshot", method="PUT", data=blob, token=token, etag=etag)
            if status != 204:
                raise RuntimeError("Snapshot upload failed: HTTP %d" % status)
            record_sync("push", "success")
            print(json.dumps({"added": []}) if app_input is not None else "Synced %d account(s)" % len(entries))
    except Exception:
        record_sync(args.command, "failed")
        raise


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(str(error), file=sys.stderr)
        raise SystemExit(1)
