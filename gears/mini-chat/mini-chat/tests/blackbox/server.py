"""Launch the debug server for the black-box suite (pid-based lifecycle)."""

import copy
import os
import shlex
import socket
import subprocess
import tempfile
import time

import httpx
import yaml

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "../../../../.."))
BINARY = os.environ.get("MINI_CHAT_SERVER_BINARY", os.path.join(ROOT, "target/debug/cf-gears-example-server"))

TOKENS = [
    {"token": "tok-a", "identity": {"subject_id": "11111111-6a88-4768-9dfc-6bcd5187d9ed", "subject_tenant_id": "00000000-df51-5b42-9538-d2b56b7ee953", "subject_type": "gts.cf.core.security.subject_user.v1~", "token_scopes": ["*"]}},
    {"token": "tok-a2", "identity": {"subject_id": "44444444-6a88-4768-9dfc-6bcd5187d9ed", "subject_tenant_id": "00000000-df51-5b42-9538-d2b56b7ee953", "subject_type": "gts.cf.core.security.subject_user.v1~", "token_scopes": ["*"]}},
    {"token": "tok-b", "identity": {"subject_id": "22222222-6a88-4768-9dfc-6bcd5187d9ed", "subject_tenant_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb", "subject_type": "gts.cf.core.security.subject_user.v1~", "token_scopes": ["*"]}},
]


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def build_config(mock_port, api_port, home, mini_chat_patch=None, policy_patch=None):
    with open(os.path.join(ROOT, "config/mini-chat.yaml")) as f:
        cfg = yaml.safe_load(f)
    cfg["server"]["home_dir"] = home
    cfg["logging"] = {"default": {"console_level": "info", "file": os.path.join(home, "server.log"), "file_level": "debug"}}
    gw = cfg["gears"]["api-gateway"]["config"]
    gw["bind_addr"] = f"127.0.0.1:{api_port}"
    gw["auth_disabled"] = False
    authn = cfg["gears"]["static-authn-plugin"]["config"]
    authn["mode"] = "static_tokens"
    authn["tokens"] = TOKENS
    cfg["gears"]["static-credstore-plugin"]["config"]["secrets"] = [{"key": "mock-key", "value": "sk-mock"}]
    for g in ["simple-user-settings", "file-parser", "resource-group"]:
        cfg["gears"].pop(g, None)
    mc = cfg["gears"]["mini-chat"]["config"]
    mc["providers"] = {
        "azure_openai": {
            "kind": "openai_responses",
            "storage_kind": "openai",
            "host": "127.0.0.1",
            "port": mock_port,
            "use_http": True,
            "api_path": "/v1/responses",
        }
    }
    mc["orphan_watchdog"] = {"scan_interval_secs": 1, "timeout_secs": 90}
    if mini_chat_patch:
        deep_merge(mc, mini_chat_patch)
    pol = cfg["gears"]["static-mini-chat-model-policy-plugin"]["config"]
    if policy_patch:
        deep_merge(pol, policy_patch)
    cfg["gears"]["oagw"]["config"]["allow_http_upstream"] = True
    return cfg


def deep_merge(base, patch):
    for k, v in patch.items():
        if isinstance(v, dict) and isinstance(base.get(k), dict):
            deep_merge(base[k], v)
        else:
            base[k] = copy.deepcopy(v)


class Server:
    def __init__(self, mock_port, mini_chat_patch=None, policy_patch=None):
        self.home = tempfile.mkdtemp(prefix="mc-bb-")
        self.port = free_port()
        self.cfg = build_config(mock_port, self.port, self.home, mini_chat_patch, policy_patch)
        self.cfg_path = os.path.join(self.home, "config.yaml")
        with open(self.cfg_path, "w") as f:
            yaml.safe_dump(self.cfg, f)
        self.base = f"http://127.0.0.1:{self.port}/mini-chat/v1"

    @property
    def db_path(self):
        return os.path.join(self.home, "mini-chat", "mini_chat.db")

    @property
    def pidfile(self):
        return os.path.join(self.home, "server.pid")

    def _pid(self):
        try:
            with open(self.pidfile) as f:
                return int(f.read().strip())
        except (OSError, ValueError):
            return None

    def alive(self):
        pid = self._pid()
        if pid is None:
            return False
        try:
            os.kill(pid, 0)
        except OSError:
            return False
        # a zombie child of a finished shell is reaped by init; treat /proc state Z as dead
        try:
            with open(f"/proc/{pid}/stat") as f:
                return f.read().split()[2] != "Z"
        except OSError:
            return False

    def start(self):
        logf = os.path.join(self.home, "stdout.log")
        # pid-based lifecycle: background the binary from a shell and record $!
        cmd = f'{shlex.quote(BINARY)} --config {shlex.quote(self.cfg_path)} run >{shlex.quote(logf)} 2>&1 & echo $! > {shlex.quote(self.pidfile)}'
        subprocess.run(["sh", "-c", cmd], check=True)
        deadline = time.time() + 120
        while time.time() < deadline:
            if not self.alive():
                raise RuntimeError("server exited: " + self.log()[-4000:])
            try:
                r = httpx.get(f"{self.base}/models", headers={"Authorization": "Bearer tok-a"}, timeout=2)
                if r.status_code == 200:
                    return
            except Exception:  # noqa: BLE001
                pass
            time.sleep(0.3)
        self.stop()
        raise RuntimeError("server did not become ready: " + self.log()[-4000:])

    def stop(self):
        pid = self._pid()
        if pid is None or not self.alive():
            return
        subprocess.run(["sh", "-c", f'kill "$(cat {shlex.quote(self.pidfile)})"'], check=False)
        deadline = time.time() + 20
        while time.time() < deadline and self.alive():
            time.sleep(0.1)
        if self.alive():
            subprocess.run(["sh", "-c", f'kill -9 "$(cat {shlex.quote(self.pidfile)})"'], check=False)

    def log(self):
        try:
            return open(os.path.join(self.home, "stdout.log")).read()
        except OSError:
            return ""
