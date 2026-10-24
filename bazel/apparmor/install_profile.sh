#!/usr/bin/env bash
#
# Installs an AppArmor profile that lets programs create user namespaces,
# which buildbox needs to run actions in containers. Ubuntu (23.10 and later)
# only allows unprivileged user namespaces for programs with such a profile.
# The profile allows nothing else: the programs are otherwise unconfined.
#
# Usage: install_profile.sh <name> <programs>
#
# <programs> is an AppArmor path pattern. Uses sudo. install.sh --apparmor
# runs this for the installed binary; dev_profile.sh runs it for the binaries
# Bazel builds, so the container tests can run.

set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <name> <programs>" >&2
  exit 2
fi
readonly name="$1" programs="$2"
readonly path="/etc/apparmor.d/$name"

echo "Installing the AppArmor profile $path for $programs (needs sudo)"
sudo tee "$path" >/dev/null <<PROFILE
# Lets buildbox create user namespaces, to run actions in containers.
# Installed by buildbox's bazel/apparmor/install_profile.sh.

abi <abi/4.0>,
include <tunables/global>

profile $name $programs flags=(unconfined) {
  userns,

  # Site-specific additions and overrides. See local/README for details.
  include if exists <local/$name>
}
PROFILE
sudo apparmor_parser -r "$path"
