variable "label" {
  description = "DNS-safe label prefix for the disposable compute host and persistent volume."
  type        = string
  default     = "australian-legal-mcp"
  validation {
    condition     = can(regex("^[a-z0-9][a-z0-9-]{1,21}[a-z0-9]$", var.label))
    error_message = "label must be 3-23 lowercase letters, digits, or hyphens so derived resource labels remain valid."
  }
}

variable "region" {
  description = "Akamai Cloud region; ap-southeast is Sydney."
  type        = string
  default     = "ap-southeast"
}

variable "instance_type" {
  description = "Pinned 8-GiB shared-CPU plan used for the test service."
  type        = string
  default     = "g6-standard-4"
  validation {
    condition     = var.instance_type == "g6-standard-4"
    error_message = "The validated test deployment uses g6-standard-4; review limits before changing it."
  }
}

variable "admin_ssh_public_key" {
  description = "Break-glass administrator OpenSSH public key; use a different key for corpus publishing."
  type        = string
  validation {
    condition     = can(regex("^ssh-(ed25519|rsa) [A-Za-z0-9+/=]+( .*)?$", trimspace(var.admin_ssh_public_key)))
    error_message = "admin_ssh_public_key must be one ed25519 or RSA OpenSSH public key."
  }
}

variable "admin_source_cidr" {
  description = "One IPv4 /32 or IPv6 /128 allowed to reach SSH."
  type        = string
  validation {
    condition = can(cidrhost(var.admin_source_cidr, 0)) && (
      (strcontains(var.admin_source_cidr, ":") && endswith(var.admin_source_cidr, "/128")) ||
      (!strcontains(var.admin_source_cidr, ":") && endswith(var.admin_source_cidr, "/32"))
    )
    error_message = "admin_source_cidr must be one IPv4 /32 or IPv6 /128."
  }
}

variable "volume_size_gib" {
  description = "Persistent corpus volume size; 128 GiB relies on required XFS reflink deltas."
  type        = number
  default     = 128
  validation {
    condition     = var.volume_size_gib >= 128 && var.volume_size_gib <= 10240
    error_message = "volume_size_gib must be between 128 and 10240."
  }
}

variable "public_mcp_enabled" {
  description = "Open TCP 80/443 only after private auth/readiness checks pass."
  type        = bool
  default     = false
}

variable "dns_domain_id" {
  description = "Optional existing Akamai DNS Manager domain ID."
  type        = number
  default     = null
  nullable    = true
  validation {
    condition     = var.dns_domain_id == null || (var.dns_domain_id > 0 && floor(var.dns_domain_id) == var.dns_domain_id)
    error_message = "dns_domain_id must be a positive integer when set."
  }
}

variable "dns_record_name" {
  description = "Optional A/AAAA record label within dns_domain_id."
  type        = string
  default     = null
  nullable    = true
  validation {
    condition = var.dns_record_name == null || can(regex(
      "^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?(\\.[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*$",
      var.dns_record_name
    ))
    error_message = "dns_record_name must be one or more lowercase DNS labels when set."
  }
}
