output "node_ids" {
  description = "EC2 instance IDs"
  value       = aws_instance.node[*].id
}

output "node_public_ips" {
  description = "Public IPs of the litek8s nodes"
  value       = aws_instance.node[*].public_ip
}

output "node_ssh_targets" {
  description = "SSH targets for litek8s node add"
  value       = [for i in aws_instance.node : "${var.ssh_user}@${i.public_ip}"]
}

output "ssh_ingress_cidr" {
  description = "CIDR allowed to reach SSH (resolved)"
  value       = local.ssh_ingress_cidr
}

output "node_add_commands" {
  description = "Ready-to-run litek8s node registration commands"
  value = join("\n", [
    for idx, i in aws_instance.node :
    "litek8s node add ${var.project_prefix}-node-${idx} --ssh ${var.ssh_user}@${i.public_ip} --install --label role=worker"
  ])
}
