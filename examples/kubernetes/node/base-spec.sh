#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Prints the base OCI spec for the `zygo` runtime handler: containerd's own
# default, plus the node's /dev/net/tun as a device the container may open.
#
#   sh base-spec.sh > /etc/containerd/zygo-base-spec.json
#   CTR="k3s ctr" sh base-spec.sh > /var/lib/rancher/k3s/agent/etc/containerd/zygo-base-spec.json
#
# Generated rather than shipped, because the default it starts from is the
# installed containerd's, and a copy from another version would quietly change
# what every Zygo pod starts with. Run it again after upgrading containerd.
set -eu

CTR=${CTR:-ctr}
TUN='{"path": "/dev/net/tun", "type": "c", "major": 10, "minor": 200, "fileMode": 438, "uid": 0, "gid": 0}'
ALLOW='{"allow": true, "type": "c", "major": 10, "minor": 200, "access": "rwm"}'

[ -c /dev/net/tun ] || { echo "base-spec.sh: this node has no /dev/net/tun; modprobe tun first" >&2; exit 1; }

spec=$($CTR oci spec)
if command -v jq >/dev/null 2>&1; then
    printf '%s' "$spec" | jq --argjson tun "$TUN" --argjson allow "$ALLOW" '
        .linux.devices = ((.linux.devices // []) + [$tun])
        | .linux.resources.devices = ((.linux.resources.devices // []) + [$allow])'
elif command -v python3 >/dev/null 2>&1; then
    printf '%s' "$spec" | TUN="$TUN" ALLOW="$ALLOW" python3 -c '
import json, os, sys
s = json.load(sys.stdin)
linux = s.setdefault("linux", {})
linux.setdefault("devices", []).append(json.loads(os.environ["TUN"]))
linux.setdefault("resources", {}).setdefault("devices", []).append(json.loads(os.environ["ALLOW"]))
json.dump(s, sys.stdout, indent=2)
print()'
else
    echo "base-spec.sh: needs jq or python3 to edit the spec" >&2
    exit 1
fi
