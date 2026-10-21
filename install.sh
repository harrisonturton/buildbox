#!/usr/bin/env bash
#
# Builds buildbox with Bazel and installs the binary.
#
# Usage: ./install.sh
#
# The binary is installed to $PREFIX/bin/buildbox, where PREFIX defaults to
# ~/.local.

set -euo pipefail

readonly TARGET="//buildbox/cli:buildbox"
readonly BAZEL_FLAGS=(--compilation_mode=opt)

prefix="${PREFIX:-$HOME/.local}"
bin_dir="$prefix/bin"

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
install -m 0755 "$binary" "$bin_dir/buildbox"
echo "Installed buildbox to $bin_dir/buildbox"

case ":$PATH:" in
  *":$bin_dir:"*) ;;
  *) echo "warning: $bin_dir is not on PATH." >&2 ;;
esac
