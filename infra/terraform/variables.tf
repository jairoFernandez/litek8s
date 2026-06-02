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
  description = "Number of EC2 worker nodes to provision"
  type        = number
  default     = 6
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
