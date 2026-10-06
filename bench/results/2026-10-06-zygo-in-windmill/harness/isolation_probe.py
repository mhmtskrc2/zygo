# SPDX-License-Identifier: Apache-2.0
import json, os, socket

def reach(host, port):
    try:
        s = socket.create_connection((host, port), timeout=2); s.close(); return "OPEN"
    except Exception as e:
        return type(e).__name__

def status(field):
    for line in open("/proc/self/status"):
        if line.startswith(field + ":"):
            return line.split(":", 1)[1].strip()
    return None

def cgroup_limit(name):
    try:
        rel = open("/proc/self/cgroup").read().split("::", 1)[1].strip()
    except Exception:
        return "no cgroup v2 line"
    for base in ("/sys/fs/cgroup" + rel, "/sys/fs/cgroup"):
        try:
            return open(os.path.join(base, name)).read().strip()
        except Exception:
            continue
    return "unreadable"

def main():
    return {
        "uid": os.getuid(),
        "seccomp (0 off, 2 filter)": status("Seccomp"),
        "no_new_privs": status("NoNewPrivs"),
        "effective capabilities": status("CapEff"),
        "own cgroup": open("/proc/self/cgroup").read().strip(),
        "memory.max": cgroup_limit("memory.max"),
        "pids.max": cgroup_limit("pids.max"),
        "Windmill's Postgres (db:5432)": reach("db", 5432),
        "Windmill's server (windmill_server:8000)": reach("windmill_server", 8000),
        "cloud metadata 169.254.169.254:80": reach("169.254.169.254", 80),
        "the VM host 192.168.5.2:22": reach("192.168.5.2", 22),
        "internet 1.1.1.1:53": reach("1.1.1.1", 53),
        "can read /proc/1/environ": os.access("/proc/1/environ", os.R_OK),
        "database-looking env vars": sorted(k for k in os.environ if any(w in k.upper() for w in ("DATABASE", "TOKEN", "SECRET", "PASSWORD"))),
    }
