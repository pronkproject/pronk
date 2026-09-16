#!/bin/sh
# Application capture against an unused CastKMS device and isolated media policy.
set -eu
[ "$#" -eq 2 ] || { echo "expected pipeline probe and unused DRM device" >&2; exit 2; }
capture_probe=$1
capture_device=$2
[ -x "$capture_probe" ]
capture_source=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
capture_test=$(mktemp -d /var/tmp/pronk-capture-pipeline.XXXXXXXX)
capture_runtime=$capture_test/runtime
capture_config=$capture_test/config
capture_data=$capture_test/data
capture_server_pid=
capture_policy_pid=

cleanup() {
    for capture_child in "$capture_policy_pid" "$capture_server_pid"; do
        if [ -n "$capture_child" ]; then
            kill -TERM "$capture_child" 2>/dev/null || :
            wait "$capture_child" 2>/dev/null || :
        fi
    done
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

mkdir -p "$capture_runtime" "$capture_config/pipewire/pipewire.conf.d" \
    "$capture_config/wireplumber/wireplumber.conf.d" "$capture_data/wireplumber/scripts"
chmod 0700 "$capture_runtime"
cp "$capture_source/data/pipewire/80-pronk-remotes.conf" \
    "$capture_config/pipewire/pipewire.conf.d/80-pronk-remotes.conf"
cp "$capture_source/data/wireplumber/80-pronk-access.conf" \
    "$capture_config/wireplumber/wireplumber.conf.d/80-pronk-access.conf"
cp "$capture_source/data/wireplumber/scripts/pronk-private-policy.lua" \
    "$capture_source/data/wireplumber/scripts/pronk-policy-marker.lua" \
    "$capture_data/wireplumber/scripts/"

export XDG_CONFIG_HOME="$capture_config" XDG_DATA_HOME="$capture_data"
export XDG_RUNTIME_DIR="$capture_runtime" PIPEWIRE_RUNTIME_DIR="$capture_runtime"
export GST_REGISTRY="$capture_test/gst-registry.bin"
if [ -n "${WIREPLUMBER_CONFIG_DIR-}" ]; then
    export WIREPLUMBER_CONFIG_DIR="$capture_config/wireplumber:$WIREPLUMBER_CONFIG_DIR"
fi
if [ -n "${WIREPLUMBER_DATA_DIR-}" ]; then
    export WIREPLUMBER_DATA_DIR="$capture_data/wireplumber:$WIREPLUMBER_DATA_DIR"
fi

echo "Application capture test logs: $capture_test"
pipewire -c pipewire.conf >"$capture_test/pipewire.log" 2>&1 &
capture_server_pid=$!
attempt=0
while [ ! -S "$capture_runtime/pipewire-0-manager" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ] || ! kill -0 "$capture_server_pid" 2>/dev/null; then
        cat "$capture_test/pipewire.log" >&2
        exit 1
    fi
    sleep 0.05
done

wireplumber -c wireplumber.conf -p policy >"$capture_test/wireplumber.log" 2>&1 &
capture_policy_pid=$!
attempt=0
while ! timeout 1 pw-dump -r pipewire-0-manager 2>/dev/null |
    jq -e '.[] | select(.type == "PipeWire:Interface:Metadata" and
        .props["metadata.name"] == "pronk-policy-v1" and
        any(.metadata[]?; .subject == 0 and .key == "pronk.policy.version" and .value == 1))' \
        >/dev/null; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 20 ] || ! kill -0 "$capture_policy_pid" 2>/dev/null; then
        cat "$capture_test/wireplumber.log" >&2
        exit 1
    fi
    sleep 0.05
done

if timeout --signal=TERM --kill-after=5 45 "$capture_probe" "$capture_device" \
    "$capture_runtime/pipewire-0-pronk-backend" >"$capture_test/client.log" 2>&1; then
    cat "$capture_test/client.log"
else
    result=$?
    cat "$capture_test/client.log" "$capture_test/wireplumber.log" >&2
    exit "$result"
fi
