variable "profile" {
  description = "AWS CLI profile to use"
  type        = string
  default     = "smarteachme-root"
}

variable "region" {
  description = "AWS region for litek8s nodes"
  type        = string
  default     = "us-east-1"
}

variable "project_prefix" {
  description = "Name prefix applied to every resource for identification/cleanup"
  type        = string
  default     = "litek8s"
}

variable "node_count" {
  description = "Number of homogeneous EC2 worker nodes (instance_type). Set 0 to use hetero_nodes only."
  type        = number
  default     = 0
}

variable "hetero_nodes" {
  description = "Heterogeneous nodes: a mix of instance types and architectures, like a household of varied machines."
  type = list(object({
    name = string
    type = string
    arch = string # x86_64 | arm64
  }))
  default = [
    { name = "node-micro-0", type = "t3.micro", arch = "x86_64" },
    { name = "node-micro-1", type = "t3.micro", arch = "x86_64" },
    { name = "node-arm-0", type = "c6g.large", arch = "arm64" },
    { name = "node-arm-1", type = "c6g.large", arch = "arm64" },
    { name = "node-big-0", type = "c7i.xlarge", arch = "x86_64" },
  ]
}

variable "instance_type" {
  description = "EC2 instance type for nodes"
  type        = string
  default     = "t3.micro"
}

variable "ssh_user" {
  description = "SSH login user baked into the AMI (Canonical Ubuntu = ubuntu)"
  type        = string
  default     = "ubuntu"
}

variable "public_key_path" {
  description = "Path to the SSH public key registered on the nodes"
  type        = string
  default     = "~/.ssh/id_ed25519.pub"
}

variable "ssh_ingress_cidr" {
  description = "CIDR allowed to reach SSH (port 22). Empty string = auto-detect current public IP as /32."
  type        = string
  default     = ""
}

variable "vpc_cidr" {
  description = "CIDR for the dedicated litek8s VPC (kept off the default VPC range)"
  type        = string
  default     = "10.42.0.0/16"
}

variable "subnet_cidr" {
  description = "CIDR for the litek8s subnet"
  type        = string
  default     = "10.42.1.0/24"
}
