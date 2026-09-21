#!/bin/sh
# `zygo mcp` against a real kernel, driven the way an agent host drives it.
#
# The unit tests in `cmd/mcp.rs` cover the dispatch, the tool list and the
# rendering. None of them can catch what this does: a real handshake over a
# real pipe, a real sandbox behind each tool call, and the boundary checked by
# attempting to cross it from inside.
#
# The checks live in `mcp_driver.py`, because the protocol is JSON and a shell
# is the wrong tool for reading it. This file does what the other suites do:
# prepare a cgroup the launcher can build under, and hand the driver a way to
# start the server inside it.
#
# Run:  make verify-mcp
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux}

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-mcp}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

if [ ! -x "$ZYGO" ]; then
    echo "  no binary at $ZYGO — run \`make poc/zygo-linux-musl\` first" >&2
    exit 1
fi

# The driver starts the server as a child process, so it cannot use the
# harness's `zygo()` shell function. This is that function, as a script: put
# the process in a cgroup that holds nothing else, then become the server.
# Everything `run_code` starts inherits it.
LAUNCHER=/tmp/zygo-mcp-launch.sh
cat > "$LAUNCHER" <<EOF
#!/bin/sh
root=$ZYGO_HARNESS_ROOT
if [ -d "\$root/launch/zygo.slice/system" ]; then
    echo \$\$ > "\$root/launch/zygo.slice/system/cgroup.procs" 2>/dev/null
else
    echo \$\$ > "\$root/launch/cgroup.procs" 2>/dev/null
fi
exec "$ZYGO" mcp "\$@"
EOF
chmod +x "$LAUNCHER"

WORKSPACE=/tmp/zygo-mcp-workspace
rm -rf "$WORKSPACE"

python3 "$SRC/poc/mcp_driver.py" "$LAUNCHER" "$WORKSPACE"
status=$?

harness_verdict
exit $status
