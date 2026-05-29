# infra/terraform — litek8s node provisioning

Terraform provisions AWS infrastructure that litek8s orchestrates on top of.
litek8s itself stays AWS-free: it only does orchestration over SSH. This module
creates the **remote nodes** it drives.

## What it creates

All resources are prefixed `litek8s-` and carry `Project=litek8s` + `ManagedBy=terraform` tags.

- Dedicated VPC (`10.42.0.0/16`) + public subnet + IGW + route table
- Security group: SSH (22) inbound **from your current public IP only**, all egress
- Key pair from your local public key (`~/.ssh/id_ed25519.pub` by default)
- 2× EC2 `t3.micro` (Ubuntu 22.04) with Docker installed via `user_data`

It does **not** touch any pre-existing AWS resource — those are not in the state.

## Prerequisites

- `terraform >= 1.5`, `aws` CLI configured with profile `smarteachme-root`
- A local SSH public key (default `~/.ssh/id_ed25519.pub`)
- Docker running locally (needed by `litek8s node add --install`, Linux target build)

## Usage

```bash
cd infra/terraform
terraform init
terraform plan      # review — should create only litek8s-* resources
terraform apply
```

Override defaults if needed:

```bash
terraform apply \
  -var="profile=smarteachme-root" \
  -var="region=us-east-1" \
  -var="node_count=2" \
  -var="public_key_path=~/.ssh/id_ed25519.pub"
```

After apply, grab the registration commands:

```bash
terraform output -raw node_add_commands
```

## Register nodes with litek8s

From the repo root (control plane):

```bash
cargo run -- serve &                  # start daemon
# paste the commands from: terraform output -raw node_add_commands
cargo run -- node add litek8s-node-0 --ssh ubuntu@<ip0> --install --label role=worker
cargo run -- node add litek8s-node-1 --ssh ubuntu@<ip1> --install --label role=worker
cargo run -- get nodes
```

Then deploy a workload pinned to a node (`nodeName: litek8s-node-0` or
`nodeSelector: {role: worker}`):

```bash
cargo run -- apply -f examples/http-echo-server77.yaml   # adjust nodeName
cargo run -- get pods
```

## Verify isolation (read-only)

```bash
aws ec2 describe-instances --profile smarteachme-root \
  --filters "Name=tag:Project,Values=litek8s" \
  --query 'Reservations[].Instances[].[InstanceId,Tags[?Key==`Name`].Value|[0],State.Name]' \
  --output table
```

## Tear down

```bash
terraform destroy
```

`destroy` only removes what is in this state — pre-existing resources are untouched.

## Notes

- **State** is local (`terraform.tfstate`, gitignored) for simplicity. To move it
  to S3 later: create a bucket (a small separate `bootstrap/` module with its own
  local state avoids the chicken-and-egg problem), then add a `backend "s3"` block
  to `versions.tf` and run `terraform init -migrate-state`.
- SSH ingress auto-detects your current public IP. If your IP changes, re-apply
  or pass `-var="ssh_ingress_cidr=x.x.x.x/32"`.
- The litek8s binary is deployed by `node add --install`, not by `user_data`.
