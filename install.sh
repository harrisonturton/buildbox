#!/usr/bin/env bash
#
# Builds buildbox with Bazel and installs the binary.
#
# Usage: ./install.sh [--apparmor]
#
# The binary is installed to $PREFIX/bin/bb, where PREFIX defaults to
# ~/.local.
#
# Running actions in containers needs unprivileged user namespaces, which
# Ubuntu (23.10 and later) only allows for programs with an AppArmor profile
# that permits them. --apparmor installs one for the installed binary, with
# sudo.

set -euo pipefail

readonly TARGET="//buildbox/cli:buildbox"
readonly BAZEL_FLAGS=(--compilation_mode=opt)
readonly RESTRICT_USERNS=/proc/sys/kernel/apparmor_restrict_unprivileged_userns

prefix="${PREFIX:-$HOME/.local}"
bin_dir="$prefix/bin"

install_profile=false
for arg in "$@"; do
  case "$arg" in
    --apparmor) install_profile=true ;;
    *)
      echo "usage: $0 [--apparmor]" >&2
      exit 2
      ;;
  esac
done

if ! command -v bazel >/dev/null 2>&1; then
  echo "error: bazel is not on PATH." >&2
  echo "Install bazelisk (https://github.com/bazelbuild/bazelisk) as 'bazel'." >&2
  exit 1
fi

cd "$(dirname "${BASH_SOURCE[0]}")"

bazel build "${BAZEL_FLAGS[@]}" "$TARGET"

# Ask Bazel where the binary is, rather than relying on convenience symlinks.
outputs="$(bazel cquery "${BAZEL_FLAGS[@]}" --output=files "$TARGET" 2>/dev/null)"
if [[ "$(wc -l <<<"$outputs")" -ne 1 ]]; then
  echo "error: expected one output from $TARGET, got:" >&2
  echo "$outputs" >&2
  exit 1
fi
binary="$(bazel info "${BAZEL_FLAGS[@]}" execution_root 2>/dev/null)/$outputs"

install -d "$bin_dir"
install -m 0755 "$binary" "$bin_dir/bb"
echo "Installed buildbox to $bin_dir/bb"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "warning: $bin_dir is not on PATH." >&2 ;;
esac

if [[ "$install_profile" == true ]]; then
  bazel/apparmor/install_profile.sh buildbox "$(realpath "$bin_dir/bb")"
fi

if [[ -r "$RESTRICT_USERNS" && "$(cat "$RESTRICT_USERNS")" == 1 && ! -e /etc/apparmor.d/buildbox ]]; then
  echo "note: this machine restricts user namespaces, so buildbox can't run actions in containers." >&2
  echo "      Run $0 --apparmor to install an AppArmor profile that allows them." >&2
fi
