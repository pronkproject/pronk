#!/usr/bin/bash
set -euo pipefail

source_file="${BASH_SOURCE[0]%/*}/../src/vulkan/private/transfer/shader.comp"
checked_file="${source_file%.comp}.spv"
temporary_file="$(mktemp)"
trap 'rm -f "$temporary_file"' EXIT

glslangValidator -V --target-env vulkan1.1 "$source_file" -o "$temporary_file"
cmp "$checked_file" "$temporary_file"
