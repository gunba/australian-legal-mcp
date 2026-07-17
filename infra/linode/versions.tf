terraform {
  required_version = ">= 1.9.0"
  required_providers {
    linode = {
      source  = "linode/linode"
      version = "~> 4.1.0"
    }
  }
}

provider "linode" {}
