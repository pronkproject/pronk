#!/bin/sh
set -eu
render_node=${1:?render node required}
modifier=${2:?hexadecimal DRM modifier required}
profile=${3:-raw}
test_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$test_dir/../.." && pwd)
cargo build --locked --manifest-path "$project_dir/Cargo.toml" \
    -p pronk-gpu-media-test --features native
gpu_runtime_dir=$(mktemp -d /var/tmp/pronk-gpu-media.XXXXXXXX)
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
if PIPEWIRE_REMOTE="$gpu_runtime_dir/pronk-gpu-test" GST_REGISTRY="$gpu_runtime_dir/gst-registry.bin" \
timeout --signal=TERM --kill-after=5 45 \
cargo run --locked --manifest-path "$project_dir/Cargo.toml" \
    -p pronk-gpu-media-test --features native -- \
    "$gpu_runtime_dir/pronk-gpu-test" "$render_node" "$modifier" "$profile" \
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
