# SPDX-License-Identifier: Apache-2.0
import json, os, socket
def reach(host, port):
    try:
        s = socket.create_connection((host, port), timeout=2); s.close(); return "OPEN"
    except Exception as e:
        return type(e).__name__
def main():
    env = sorted(k for k in os.environ)
    secretish = [k for k in env if any(w in k.upper() for w in ("DATABASE", "TOKEN", "SECRET", "PASSWORD", "KEY"))]
    return {
        "uid": os.getuid(),
        "env_count": len(env),
        "secret_like_env": secretish,
        "windmill_db (db:5432)": reach("db", 5432),
        "windmill server (windmill_server:8000)": reach("windmill_server", 8000),
        "cloud metadata 169.254.169.254:80": reach("169.254.169.254", 80),
        "the Mac's Postgres (host.docker.internal:5432)": reach("host.docker.internal", 5432),
        "LAN router 192.168.1.1:80": reach("192.168.1.1", 80),
        "internet 1.1.1.1:53": reach("1.1.1.1", 53),
        "can read /proc/1/environ": os.access("/proc/1/environ", os.R_OK),
    }
