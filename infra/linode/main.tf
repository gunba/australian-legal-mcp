locals {
  admin_ipv4  = strcontains(var.admin_source_cidr, ":") ? [] : [var.admin_source_cidr]
  admin_ipv6  = strcontains(var.admin_source_cidr, ":") ? [var.admin_source_cidr] : []
  dns_enabled = var.dns_domain_id != null && var.dns_record_name != null
}

check "dns_inputs_are_paired" {
  assert {
    condition     = (var.dns_domain_id == null) == (var.dns_record_name == null)
    error_message = "dns_domain_id and dns_record_name must be set together."
  }
}

resource "linode_instance" "mcp" {
  label              = var.label
  image              = "linode/ubuntu24.04"
  region             = var.region
  type               = var.instance_type
  authorized_keys    = [trimspace(var.admin_ssh_public_key)]
  booted             = true
  private_ip         = false
  swap_size          = 512
  maintenance_policy = "linode/migrate"
  firewall_id        = linode_firewall.mcp.id
  tags               = ["australian-legal-mcp", "disposable-compute"]
}

resource "linode_volume" "corpus" {
  label      = "${var.label}-corpus"
  region     = var.region
  size       = var.volume_size_gib
  linode_id  = linode_instance.mcp.id
  encryption = "enabled"
  tags       = ["australian-legal-mcp", "persistent-corpus"]

  lifecycle {
    prevent_destroy = true
  }
}

resource "linode_firewall" "mcp" {
  label = "${var.label}-firewall"

  inbound {
    label    = "restricted-ssh"
    action   = "ACCEPT"
    protocol = "TCP"
    ports    = "22"
    ipv4     = length(local.admin_ipv4) > 0 ? local.admin_ipv4 : null
    ipv6     = length(local.admin_ipv6) > 0 ? local.admin_ipv6 : null
  }

  inbound {
    label    = "essential-icmpv6"
    action   = "ACCEPT"
    protocol = "ICMP"
    ipv6     = ["::/0"]
  }

  dynamic "inbound" {
    for_each = var.public_mcp_enabled ? toset(["80", "443"]) : toset([])
    content {
      label    = inbound.value == "80" ? "public-acme-http" : "public-mcp-https"
      action   = "ACCEPT"
      protocol = "TCP"
      ports    = inbound.value
      ipv4     = ["0.0.0.0/0"]
      ipv6     = ["::/0"]
    }
  }

  inbound_policy  = "DROP"
  outbound_policy = "ACCEPT"
  tags            = ["australian-legal-mcp"]
}

resource "linode_domain_record" "ipv4" {
  count       = local.dns_enabled ? 1 : 0
  domain_id   = var.dns_domain_id
  name        = var.dns_record_name
  record_type = "A"
  target      = tolist(linode_instance.mcp.ipv4)[0]
  ttl_sec     = 300
}

resource "linode_domain_record" "ipv6" {
  count       = local.dns_enabled ? 1 : 0
  domain_id   = var.dns_domain_id
  name        = var.dns_record_name
  record_type = "AAAA"
  target      = split("/", linode_instance.mcp.ipv6)[0]
  ttl_sec     = 300
}
