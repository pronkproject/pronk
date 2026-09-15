#!/usr/bin/env bash
set -euo pipefail

shader_test_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
shader_source_dir="$shader_test_dir/../src/vulkan/private/matrix"
shader_check_dir=$(mktemp -d "${TMPDIR:-/var/tmp}/pronk-matrix-shader.XXXXXXXX")
trap 'rm -f -- "$shader_check_dir/shader.spv"; rmdir -- "$shader_check_dir"' EXIT

"${GLSLANG_VALIDATOR:-glslangValidator}" -V --target-env vulkan1.1 \
    "$shader_source_dir/shader.comp" -o "$shader_check_dir/shader.spv"
cmp -- "$shader_source_dir/shader.spv" "$shader_check_dir/shader.spv"
