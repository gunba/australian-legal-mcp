#!/usr/bin/env bash
# Prove the installer grants exactly the three forced deployment operations,
# digest-pins their helper, and retains the single forced-command wrapper.
# shellcheck disable=SC2016
set -euo pipefail
export LC_ALL=C

repo_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
installer="$repo_dir/infra/linode/install-host.sh"
image_updater="$repo_dir/infra/hosting/update-image.sh"
quadlet_template="$repo_dir/infra/hosting/legal-mcp.container.template"
[[ -f "$installer" && ! -L "$installer" ]] || exit 2

grep -Fq "render_publisher_sudoers \"\$installed_host_deploy_sha256\"" "$installer"
grep -Fq 'LEGAL_MCP_HOST_TOOLS_V2' "$installer"
grep -Fq 'CONFIGURE_AUTH_SHA256=' "$installer"
grep -Fq 'UPDATE_IMAGE_SHA256=' "$installer"
grep -Fq 'CONTAINER_TEMPLATE_SHA256=' "$installer"
grep -Fq 'RENDERED_QUADLET_SHA256=' "$installer"
grep -Fq 'LEGAL_MCP_HOST_TOOLS_TRANSACTION_V2' "$installer"
grep -Fq 'LEGAL_MCP_HOST_TOOL_LAUNCHER_V1' "$installer"
grep -Fq 'CONFIGURE_AUTH_POINTER=/etc/legal-mcp/configure-auth-implementation' "$installer"
grep -Fq 'UPDATE_IMAGE_POINTER=/etc/legal-mcp/update-image-implementation' "$installer"
grep -Fq 'require_file "$pointer" root root 644 64' "$installer"
grep -Fq 'flock -x 9' "$installer"
if grep -Fq 'flock -u 9' "$installer"; then exit 1; fi
grep -Fq 'export LEGAL_MCP_HOST_TRANSACTION_LOCK_FD=9' "$installer"
grep -Fq 'disabled_dark_recovery_ready' "$installer"
grep -Fq '[[ "$role" = configure-auth && $# -eq 1 && "$1" = --recover ]]' "$installer"
grep -Fq '"$CONFIGURE" --recover || status=$?' "$installer"
grep -Fq '"$CONFIGURE" --prepare-auth-dispatch || prepare_status=$?' "$installer"
grep -Fq '"$CONFIGURE" --finalize-auth-ready || status=$?' "$installer"
grep -Fq 'prior_snapshot="$(auth_state_snapshot)" || status=1' "$installer"
grep -Fq 'auth_journal_state_present' "$installer"
grep -Fq 'configure_digest="$(read_implementation "$CONFIGURE_POINTER" configure-auth)"' "$installer"
grep -Fq 'signal.pidfd_send_signal(pidfd, signal.SIGKILL)' "$installer"
grep -Fq 'drain_legacy_host_tool_processes "$HOST_TOOLS_TRANSACTION"' "$installer"
grep -Fq '"$HOST_TOOL_RENDERED_QUADLET_SOURCE" "$RENDERED_QUADLET" root root 644' "$installer"
grep -Fq "\"\$HOST_TOOL_SOURCE_CONTAINER_TEMPLATE\" \"\$CONTAINER_TEMPLATE\" root root 644" "$installer"
grep -Fq 'install -o root -g root -m 0640 /dev/null /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK' "$installer"
grep -Fq 'require_empty_regular_file /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK root root 640' "$installer"
grep -Fq 'require_exact_acl /srv/legal-mcp/lifecycle/LIFECYCLE_LOCK' "$installer"
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
grep -Fq 'validate_v2_host_tool_release' "$image_updater"
grep -Fq 'installed and release-bundled V2 host tools do not match exactly' "$image_updater"
grep -Fxq 'ExecCondition=/usr/local/libexec/legal-mcp/host-tool-launcher --check-auth-ready' \
  "$quadlet_template"

lock_line="$(grep -nF 'flock -x 9' "$installer" | head -n 1 | cut -d: -f1)"
selection_line="$(grep -nF 'configure_digest="$(read_implementation "$CONFIGURE_POINTER" configure-auth)"' \
  "$installer" | cut -d: -f1)"
[[ "$lock_line" =~ ^[0-9]+$ && "$selection_line" =~ ^[0-9]+$ \
  && "$lock_line" -lt "$selection_line" ]]

launcher_fixture="$(mktemp)"
trap 'rm -f "$launcher_fixture"' EXIT
awk '
  /^  cat <<'\''LAUNCHER'\''$/ { in_launcher=1; next }
  in_launcher && /^LAUNCHER$/ { exit }
  in_launcher { print }
' "$installer" > "$launcher_fixture"
[[ -s "$launcher_fixture" ]]
bash -n "$launcher_fixture"
prepare_line="$(grep -nF '"$CONFIGURE" --prepare-auth-dispatch || prepare_status=$?' \
  "$launcher_fixture" | cut -d: -f1)"
darken_line="$(grep -nF 'elif ! remove_auth_ready; then' "$launcher_fixture" | cut -d: -f1)"
configure_line="$(grep -nF '"$CONFIGURE" "$@" || status=$?' \
  "$launcher_fixture" | cut -d: -f1)"
publish_line="$(grep -nF 'publish_auth_ready || status=1' \
  "$launcher_fixture" | tail -n 1 | cut -d: -f1)"
finalize_line="$(grep -nF '"$CONFIGURE" --finalize-auth-ready || status=$?' \
  "$launcher_fixture" | tail -n 1 | cut -d: -f1)"
[[ "$prepare_line" =~ ^[0-9]+$ && "$darken_line" =~ ^[0-9]+$ \
  && "$configure_line" =~ ^[0-9]+$ && "$publish_line" =~ ^[0-9]+$ \
  && "$finalize_line" =~ ^[0-9]+$ \
  && "$prepare_line" -lt "$darken_line" && "$darken_line" -lt "$configure_line" \
  && "$configure_line" -lt "$publish_line" && "$publish_line" -lt "$finalize_line" ]]
rm -f "$launcher_fixture"
trap - EXIT

echo host-installer-publisher-contract-fixture-ok
