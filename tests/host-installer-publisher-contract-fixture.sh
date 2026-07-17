#!/usr/bin/env bash
# Prove the installer grants exactly the three forced deployment operations and
# continues to bind the publisher key to the single forced-command wrapper.
set -euo pipefail
export LC_ALL=C

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer="$repo_dir/infra/linode/install-host.sh"
[[ -f "$installer" && ! -L "$installer" ]] || exit 2

expected_sudo='legal-mcp-publisher ALL=(root) NOPASSWD: /usr/local/sbin/legal-mcp-host-deploy prepare *, /usr/local/sbin/legal-mcp-host-deploy activate *, /usr/local/sbin/legal-mcp-host-deploy abort *'
[[ "$(grep -Fxc "$expected_sudo" "$installer")" -eq 1 ]]
[[ "$(grep -Fc 'legal-mcp-publisher ALL=(root) NOPASSWD:' "$installer")" -eq 1 ]]
[[ "$(grep -Fc 'restrict,command="/usr/local/sbin/legal-mcp-publisher-command" %s\n' "$installer")" -eq 1 ]]

echo host-installer-publisher-contract-fixture-ok
