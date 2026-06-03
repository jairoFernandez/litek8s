# --- Current public IP (for default SSH ingress) ---
data "http" "myip" {
  url = "https://checkip.amazonaws.com"
}

# --- First availability zone in the region ---
data "aws_availability_zones" "available" {
  state = "available"
}

# --- Latest Ubuntu 24.04 LTS (Canonical) amd64 ---
# 24.04 ships glibc 2.39, matching the rust:1.90 build image used by
# `litek8s node add --install`. 22.04 (glibc 2.35) is too old for that binary.
data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

# --- Latest Ubuntu 24.04 LTS (Canonical) arm64, for Graviton nodes ---
data "aws_ami" "ubuntu_arm64" {
  most_recent = true
  owners      = ["099720109477"]

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-arm64-server-*"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }
}

locals {
  # AMI per architecture, so a heterogeneous node can pick the right one.
  ami_for_arch = {
    x86_64 = data.aws_ami.ubuntu.id
    arm64  = data.aws_ami.ubuntu_arm64.id
  }
}

locals {
  ssh_ingress_cidr = var.ssh_ingress_cidr != "" ? var.ssh_ingress_cidr : "${chomp(data.http.myip.response_body)}/32"
}

# --- Dedicated VPC ---
resource "aws_vpc" "litek8s" {
  cidr_block           = var.vpc_cidr
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = {
    Name = "${var.project_prefix}-vpc"
  }
}

resource "aws_subnet" "litek8s" {
  vpc_id                  = aws_vpc.litek8s.id
  cidr_block              = var.subnet_cidr
  availability_zone       = data.aws_availability_zones.available.names[0]
  map_public_ip_on_launch = true

  tags = {
    Name = "${var.project_prefix}-subnet"
  }
}

resource "aws_internet_gateway" "litek8s" {
  vpc_id = aws_vpc.litek8s.id

  tags = {
    Name = "${var.project_prefix}-igw"
  }
}

resource "aws_route_table" "litek8s" {
  vpc_id = aws_vpc.litek8s.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.litek8s.id
  }

  tags = {
    Name = "${var.project_prefix}-rt"
  }
}

resource "aws_route_table_association" "litek8s" {
  subnet_id      = aws_subnet.litek8s.id
  route_table_id = aws_route_table.litek8s.id
}

# --- Security group: SSH from current IP only, egress all ---
resource "aws_security_group" "litek8s" {
  name        = "${var.project_prefix}-sg"
  description = "litek8s node access: SSH from control plane"
  vpc_id      = aws_vpc.litek8s.id

  ingress {
    description = "SSH from control plane"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = [local.ssh_ingress_cidr]
  }

  # Intra-cluster traffic: nodes talk to each other over the private subnet so
  # the in-cluster coordinator can drive a distributed forward at LAN latency
  # (~ms) instead of routing every matmul back through the control plane over
  # the internet (~600ms). Private range only — not internet-exposed.
  ingress {
    description = "All traffic within the cluster subnet"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = [var.subnet_cidr]
  }

  egress {
    description = "All outbound (apt, docker pull)"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = {
    Name = "${var.project_prefix}-sg"
  }
}

# --- Key pair from local public key ---
resource "aws_key_pair" "litek8s" {
  key_name   = "${var.project_prefix}-key"
  public_key = file(pathexpand(var.public_key_path))

  tags = {
    Name = "${var.project_prefix}-key"
  }
}

# --- Homogeneous worker nodes (node_count x instance_type) ---
resource "aws_instance" "node" {
  count                  = var.node_count
  ami                    = data.aws_ami.ubuntu.id
  instance_type          = var.instance_type
  subnet_id              = aws_subnet.litek8s.id
  vpc_security_group_ids = [aws_security_group.litek8s.id]
  key_name               = aws_key_pair.litek8s.key_name
  user_data              = file("${path.module}/userdata.sh")

  tags = {
    Name = "${var.project_prefix}-node-${count.index}"
  }
}

# --- Heterogeneous worker nodes: a mix of instance types and architectures
# (x86_64 + arm64), to mirror a household of varied machines. Each entry picks
# its AMI by arch; the agent install cross-compiles per node's uname. ---
resource "aws_instance" "hetero" {
  for_each = { for n in var.hetero_nodes : n.name => n }

  ami                    = local.ami_for_arch[each.value.arch]
  instance_type          = each.value.type
  subnet_id              = aws_subnet.litek8s.id
  vpc_security_group_ids = [aws_security_group.litek8s.id]
  key_name               = aws_key_pair.litek8s.key_name
  user_data              = file("${path.module}/userdata.sh")

  tags = {
    Name = "${var.project_prefix}-${each.value.name}"
    Arch = each.value.arch
  }
}
