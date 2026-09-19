#!/bin/sh
set -eu
[ "$#" -le 5 ] || { echo "expected render node, modifier, optional profile, execution and format" >&2; exit 2; }
render_node=${1:?render node required}
modifier=${2:?hexadecimal DRM modifier required}
profile=${3:-raw}
case "$profile" in raw|va-h264|production-va-h264) ;; *) echo "profile must be raw, va-h264 or production-va-h264" >&2; exit 2 ;; esac
execution=${4:-host}
case "$execution" in host|sandbox|sandbox-denied) ;; *) echo "execution must be host, sandbox or sandbox-denied" >&2; exit 2 ;; esac
case "$profile" in raw) default_format=XR24 ;; *) default_format=AR24 ;; esac
pixel_format=${5:-$default_format}
case "$pixel_format" in XR24|AR24|XB24|AB24) ;; *) echo "format must be XR24, AR24, XB24 or AB24" >&2; exit 2 ;; esac
test_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$test_dir/../.." && pwd)
gpu_runtime_dir=$(mktemp -d /var/tmp/pronk-gpu-media.XXXXXXXX)
cargo build --locked --manifest-path "$project_dir/Cargo.toml" \
    -p pronk-gpu-media-test --features native --message-format=json \
    > "$gpu_runtime_dir/build.json"
binary=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "pronk-gpu-media-test" and .executable != null) | .executable' "$gpu_runtime_dir/build.json")
[ -n "$binary" ] && [ -x "$binary" ]
PIPEWIRE_RUNTIME_DIR="$gpu_runtime_dir" pipewire -c "$test_dir/pipewire.conf" \
    > "$gpu_runtime_dir/pipewire.log" 2>&1 &
gpu_server_pid=$!
trap 'kill -TERM "$gpu_server_pid" 2>/dev/null || :; wait "$gpu_server_pid" 2>/dev/null || :' EXIT
trap 'exit 1' HUP INT TERM
attempt=0
while [ ! -S "$gpu_runtime_dir/pronk-gpu-test" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ] || ! kill -0 "$gpu_server_pid" 2>/dev/null; then
        echo "Private PipeWire startup failed; log: $gpu_runtime_dir/pipewire.log" >&2
        exit 1
    fi
    sleep 0.05
done
echo "Private graph: $gpu_runtime_dir"
set -- "$binary" "$gpu_runtime_dir/pronk-gpu-test" "$render_node" "$modifier" "$profile" "$pixel_format"
case "$execution" in
sandbox) set -- sh "$test_dir/run-sandbox.sh" "$@" allowed ;;
sandbox-denied) set -- sh "$test_dir/run-sandbox.sh" "$@" denied ;;
esac
if PIPEWIRE_REMOTE="$gpu_runtime_dir/pronk-gpu-test" GST_REGISTRY="$gpu_runtime_dir/gst-registry.bin" \
timeout --signal=TERM --kill-after=5 45 "$@" \
    > "$gpu_runtime_dir/client.log" 2>&1; then
    cat "$gpu_runtime_dir/client.log"
else
    result=$?
    cat "$gpu_runtime_dir/client.log" >&2
    exit "$result"
fi
if rg -q 'Validation Error|VUID-' "$gpu_runtime_dir/client.log"; then
    echo "Vulkan validation reported a problem" >&2
    exit 1
fi
