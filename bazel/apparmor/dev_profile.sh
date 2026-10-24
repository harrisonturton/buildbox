#!/usr/bin/env bash
#
# Lets the buildbox binaries Bazel builds in this workspace create user
# namespaces, so the container tests run rather than skipping, and
# `bazel run //buildbox/cli:buildbox -- up` can run containers. Uses sudo.
#
# Usage: bazel/apparmor/dev_profile.sh

set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
output_path="$(bazel info output_path 2>/dev/null)"
exec bazel/apparmor/install_profile.sh buildbox-dev \
  "$output_path/*/bin/buildbox/{cli/buildbox,container/container_init}"
