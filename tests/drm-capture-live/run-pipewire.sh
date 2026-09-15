#!/bin/sh
# Only use an unoccupied Rust CastKMS device in a disposable test environment.
set -eu
[ "$#" -eq 2 ] || { echo "expected probe binary and unused DRM device" >&2; exit 2; }
capture_probe=$1
capture_device=$2
[ -x "$capture_probe" ]
capture_test_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
capture_runtime=$(mktemp -d /var/tmp/pronk-capture-pipewire.XXXXXXXX)
PIPEWIRE_RUNTIME_DIR="$capture_runtime" pipewire -c "$capture_test_dir/../gpu-media/pipewire.conf" \
    > "$capture_runtime/pipewire.log" 2>&1 &
capture_server_pid=$!
trap 'kill -TERM "$capture_server_pid" 2>/dev/null || :; wait "$capture_server_pid" 2>/dev/null || :' EXIT
trap 'exit 1' HUP INT TERM
attempt=0
while [ ! -S "$capture_runtime/pronk-gpu-test" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ] || ! kill -0 "$capture_server_pid" 2>/dev/null; then
        echo "Private server failed; log: $capture_runtime/pipewire.log" >&2
        exit 1
    fi
    sleep 0.05
done
echo "Capture test logs: $capture_runtime"
if PIPEWIRE_REMOTE="$capture_runtime/pronk-gpu-test" GST_REGISTRY="$capture_runtime/gst-registry.bin" \
    timeout --signal=TERM --kill-after=5 45 "$capture_probe" "$capture_device" "$capture_runtime/pronk-gpu-test" \
    > "$capture_runtime/client.log" 2>&1; then
    cat "$capture_runtime/client.log"
else
    result=$?
    cat "$capture_runtime/client.log" >&2
    exit "$result"
fi
