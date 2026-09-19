#!/bin/sh
set -eu

render_node=${1:?selected DRM render node required}
[ "$#" -eq 1 ] || { echo "expected one render node" >&2; exit 2; }
render_node=$(realpath -e -- "$render_node")
[ -c "$render_node" ] || { echo "render node is not a character device" >&2; exit 2; }
case "$render_node" in
    /dev/dri/renderD*) ;;
    *) echo "expected one /dev/dri/renderD node" >&2; exit 2 ;;
esac
render_number=${render_node#/dev/dri/renderD}
case "$render_number" in
    ''|*[!0-9]*) echo "render node number is invalid" >&2; exit 2 ;;
esac

test_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
project_dir=$(CDPATH= cd -- "$test_dir/../.." && pwd)
test_run_dir=$(mktemp -d /var/tmp/pronk-va-preparation.XXXXXXXX)
cargo test --locked --manifest-path "$project_dir/Cargo.toml" \
    -p pronk-chromiacast --no-run --message-format=json \
    > "$test_run_dir/build.json"
test_binary=$(jq -er 'select(.reason == "compiler-artifact" and
    .target.name == "pronk_chromiacast" and .profile.test == true and
    .executable != null) | .executable' "$test_run_dir/build.json")
[ -x "$test_binary" ]

set --
if [ -n "${LIBVA_DRIVERS_PATH:-}" ]; then
    set -- "$@" "--setenv=LIBVA_DRIVERS_PATH=$LIBVA_DRIVERS_PATH"
fi
if [ -n "${LIBVA_DRIVER_NAME:-}" ]; then
    set -- "$@" "--setenv=LIBVA_DRIVER_NAME=$LIBVA_DRIVER_NAME"
fi

echo "Backend preparation log: $test_run_dir/test.log"
if timeout --signal=TERM --kill-after=5 75 \
    systemd-run --user --wait --pipe --collect \
    "--unit=pronk-va-preparation-$$" \
    --property=Type=exec --property=NoNewPrivileges=yes \
    --property=CapabilityBoundingSet= --property=AmbientCapabilities= \
    --property=DevicePolicy=closed --property=LockPersonality=yes \
    --property=MemoryDenyWriteExecute=yes --property=PrivateDevices=yes \
    --property=PrivateTmp=yes --property=ProtectControlGroups=yes \
    '--property=InaccessiblePaths=-/home -/var/home -/root' \
    --property=ProtectKernelLogs=yes --property=ProtectKernelModules=yes \
    --property=ProtectKernelTunables=yes --property=ProtectSystem=strict \
    '--property=RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6 AF_NETLINK' \
    --property=RestrictNamespaces=yes --property=RestrictRealtime=yes \
    --property=RestrictSUIDSGID=yes --property=SystemCallArchitectures=native \
    --property=SystemCallFilter=@system-service \
    --property=SystemCallErrorNumber=EPERM --property=UMask=0077 \
    --property=RuntimeMaxSec=60 --property=TimeoutStopSec=3 \
    "--property=DeviceAllow=$render_node rw" \
    "--property=BindReadOnlyPaths=$render_node" \
    "--setenv=PRONK_GPU_RENDER_NODE=$render_node" \
    --setenv=GST_REGISTRY=/tmp/pronk-va-preparation-registry.bin \
    --setenv=XDG_CACHE_HOME=/tmp/pronk-va-preparation-cache \
    "$@" "$test_binary" \
    selected_va_device_prepares_only_its_usable_mode_formats \
    --ignored --nocapture > "$test_run_dir/test.log" 2>&1; then
    cat "$test_run_dir/test.log"
else
    result=$?
    cat "$test_run_dir/test.log" >&2
    exit "$result"
fi
