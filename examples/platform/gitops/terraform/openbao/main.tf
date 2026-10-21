# OpenBao's policies as code: change them in a pull request, Atlantis plans
# it, `atlantis apply` applies it. The vault provider works with OpenBao;
# VAULT_ADDR and VAULT_TOKEN come from Atlantis's environment.
terraform {
  backend "kubernetes" {
    secret_suffix     = "openbao"
    namespace         = "<TOFU_STATE_NAMESPACE>"
    in_cluster_config = true
  }
  required_providers {
    vault = {
      source  = "hashicorp/vault"
      version = "~> 4.0"
    }
  }
}

provider "vault" {}
