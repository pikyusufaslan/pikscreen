#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
source_file="$script_dir/pikscreen-hyprland-input.cpp"
output_file="$script_dir/pikscreen-hyprland-input.so"
abi_file="$output_file.abi"

read -r -a hyprland_flags <<< "$(pkg-config --cflags hyprland)"
c++ -std=c++23 -O2 -shared -fPIC -fno-gnu-unique \
  "${hyprland_flags[@]}" \
  "$source_file" \
  -o "$output_file"

hyprctl version | sed -n 's/^Version ABI string: //p' > "$abi_file"
nm -D "$output_file" | grep -q ' pluginAPIVersion'
nm -D "$output_file" | grep -q ' pluginInit'
nm -D "$output_file" | grep -q ' pluginExit'
