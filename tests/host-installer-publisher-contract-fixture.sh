#!/usr/bin/env bash
# Prove the installer grants exactly the three forced deployment operations,
# digest-pins their helper, and retains the single forced-command wrapper.
set -euo pipefail
export LC_ALL=C

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer="$repo_dir/infra/linode/install-host.sh"
[[ -f "$installer" && ! -L "$installer" ]] || exit 2

grep -Fq "render_publisher_sudoers \"\$installed_host_deploy_sha256\"" "$installer"
grep -Fq "sha256:\$deploy_sha256 \$HOST_DEPLOY ^prepare [0-9a-f]{64}\$" "$installer"
grep -Fq "sha256:\$deploy_sha256 \$HOST_DEPLOY ^activate [0-9a-f]{64}\$" "$installer"
grep -Fq "sha256:\$deploy_sha256 \$HOST_DEPLOY ^abort [0-9a-f]{64}\$" "$installer"
[[ "$(grep -Fc 'restrict,command="/usr/local/sbin/legal-mcp-publisher-command" %s\n' "$installer")" -eq 1 ]]

host_tools_line="$(grep -nF \
  "atomic_install_file \"\$host_tools_marker_tmp\" \"\$HOST_TOOLS_MARKER\" root root 444" \
  "$installer" | cut -d: -f1)"
host_installed_line="$(grep -nF \
  "atomic_install_file \"\$host_installed_tmp\" /etc/legal-mcp/host-installed root root 444" \
  "$installer" | cut -d: -f1)"
[[ "$host_tools_line" =~ ^[0-9]+$ && "$host_installed_line" =~ ^[0-9]+$ \
  && "$host_tools_line" -lt "$host_installed_line" ]]
grep -Fq "sync -f \"\$(dirname \"\$destination\")\"" "$installer"

echo host-installer-publisher-contract-fixture-ok
