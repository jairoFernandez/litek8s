# Who may do what in OpenBao. external-secrets' read-only roles are set up
# by up.sh (the platform needs them before Atlantis runs).

resource "vault_policy" "admin" {
  name   = "admin"
  policy = <<-EOT
    path "*" {
      capabilities = ["create", "read", "update", "patch", "delete", "list", "sudo"]
    }
  EOT
}

resource "vault_policy" "developer" {
  name   = "developer"
  policy = <<-EOT
    # Application secrets: read and write, no deleting history.
    path "secret/data/*" {
      capabilities = ["create", "read", "update", "patch", "list"]
    }
    path "secret/metadata/*" {
      capabilities = ["read", "list"]
    }
  EOT
}
