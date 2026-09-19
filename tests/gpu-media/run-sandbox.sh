#!/bin/sh
set -eu
binary=${1:?built fixture required}
socket=${2:?private socket required}
render_node=${3:?render node required}
modifier=${4:?modifier required}
profile=${5:?media profile required}
pixel_format=${6:?pixel format required}
output_size=${7:?output size required}
access=${8:?choose allowed or denied}
case "$output_size" in
1920x1080) runtime_limit=55 ;;
2560x1440) runtime_limit=90 ;;
3840x2160) runtime_limit=160 ;;
*) exit 2 ;;
esac
# Device bindings name one resolved character device, never the whole DRM tree.
render_node=$(realpath -e -- "$render_node")
[ -c "$render_node" ]
case "$render_node" in /dev/dri/renderD*) ;; *) exit 2 ;; esac
render_number=${render_node#/dev/dri/renderD}
case "$render_number" in ''|*[!0-9]*) exit 2 ;; esac
runtime_dir=$(dirname -- "$socket")
set --
case "$access" in
allowed) set -- "--property=DeviceAllow=$render_node rw" "--property=BindReadOnlyPaths=$render_node" ;;
denied) ;;
*) exit 2 ;;
esac
if [ -n "${LIBVA_DRIVERS_PATH:-}" ]; then
    set -- "$@" "--setenv=LIBVA_DRIVERS_PATH=$LIBVA_DRIVERS_PATH"
fi
if [ -n "${LIBVA_DRIVER_NAME:-}" ]; then
    set -- "$@" "--setenv=LIBVA_DRIVER_NAME=$LIBVA_DRIVER_NAME"
fi
exec systemd-run --user --wait --pipe --collect \
    --unit="pronk-gpu-media-$$" \
    --property=Type=exec --property=NoNewPrivileges=yes \
    --property=CapabilityBoundingSet= --property=AmbientCapabilities= \
    --property=DevicePolicy=closed --property=LockPersonality=yes \
    --property=MemoryDenyWriteExecute=yes --property=PrivateDevices=yes \
    --property=PrivateTmp=yes --property=ProtectControlGroups=yes \
    '--property=InaccessiblePaths=-/home -/var/home -/root' \
    --property=ProtectKernelLogs=yes --property=ProtectKernelModules=yes \
    --property=ProtectKernelTunables=yes --property=ProtectSystem=strict \
    --property=RestrictAddressFamilies=AF_UNIX \
    --property=RestrictNamespaces=yes --property=RestrictRealtime=yes \
    --property=RestrictSUIDSGID=yes --property=SystemCallArchitectures=native \
    --property=SystemCallFilter=@system-service --property=SystemCallErrorNumber=EPERM \
    --property=UMask=0077 "--property=RuntimeMaxSec=$runtime_limit" --property=TimeoutStopSec=3 \
    "--property=BindReadOnlyPaths=$runtime_dir" \
    "--setenv=PIPEWIRE_REMOTE=$socket" \
    --setenv=GST_REGISTRY=/tmp/pronk-gpu-registry.bin \
    --setenv=XDG_CACHE_HOME=/tmp/pronk-gpu-cache \
    "--setenv=PRONK_GPU_TEST_SANDBOX=$access" "$@" \
    "$binary" "$socket" "$render_node" "$modifier" "$profile" "$pixel_format" "$output_size"
