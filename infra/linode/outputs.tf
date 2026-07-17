output "instance_id" {
  value = linode_instance.mcp.id
}

output "public_ipv4" {
  value = tolist(linode_instance.mcp.ipv4)[0]
}

output "public_ipv6" {
  value = linode_instance.mcp.ipv6
}

output "volume_id" {
  value = linode_volume.corpus.id
}

output "volume_device" {
  value = linode_volume.corpus.filesystem_path
}

output "public_mcp_enabled" {
  value = var.public_mcp_enabled
}
