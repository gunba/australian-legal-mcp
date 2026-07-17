#!/usr/bin/env bash
# Explicit management-plane start/deallocation wrapper for the disposable Azure
# test VM. Guest `shutdown` is intentionally not used because allocated stopped
# VMs can continue incurring compute charges.
set -euo pipefail

[[ $# = 3 ]] || {
  echo "usage: azure-vm.sh start|deallocate|status RESOURCE_GROUP VM_NAME" >&2
  exit 2
}
ACTION="$1"
RESOURCE_GROUP="$2"
VM_NAME="$3"
[[ "$RESOURCE_GROUP" =~ ^[A-Za-z0-9._()-]{1,90}$ \
  && "$RESOURCE_GROUP" != *. \
  && "$VM_NAME" =~ ^[A-Za-z0-9._-]{1,64}$ ]] || {
  echo "unsafe Azure resource name" >&2
  exit 2
}
command -v az >/dev/null || { echo "missing az" >&2; exit 2; }

case "$ACTION" in
  start)
    az vm start --resource-group "$RESOURCE_GROUP" --name "$VM_NAME" --output none
    ;;
  deallocate)
    az vm deallocate --resource-group "$RESOURCE_GROUP" --name "$VM_NAME" --output none
    ;;
  status)
    # shellcheck disable=SC2016 # Backticks are JMESPath literals, not shell expansion.
    az vm get-instance-view --resource-group "$RESOURCE_GROUP" --name "$VM_NAME" \
      --query 'instanceView.statuses[?starts_with(code, `PowerState/`)].{code:code,displayStatus:displayStatus}' \
      --output json
    ;;
  *)
    echo "action must be start, deallocate, or status" >&2
    exit 2
    ;;
esac
