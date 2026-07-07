# {{PROJECT_NAME}}

A self-contained Terraform + litek8s deploy project, scaffolded by
`litek8s infra init`. It provisions litek8s nodes on AWS EC2 and deploys an app
to them, without living inside the litek8s source tree.

## Layout

```
.
├── terraform/                 # Provisions the AWS nodes (VPC, subnet, SG, EC2)
│   ├── main.tf                # VPC/subnet/IGW/SG/keypair/instances
│   ├── variables.tf           # profile/region/node_count/hetero_nodes/...
│   ├── outputs.tf             # node IPs + ready-to-run `node add` commands
│   ├── providers.tf           # AWS provider (profile from var.profile)
│   ├── versions.tf            # required terraform + provider versions
│   ├── userdata.sh            # cloud-init: installs Docker on each node
│   ├── terraform.tfvars.example  # copy to terraform.tfvars and edit
│   └── .gitignore             # ignores state, tfvars, keys
├── deploy/
│   ├── app.yaml               # example litek8s Deployment (nginx:alpine)
│   └── service.yaml           # example litek8s Service exposing the app
├── deploy.sh                  # one-shot: register nodes → apply → smoke test
└── README.md                  # this file
```

## Flow

1. **Provision the nodes with Terraform.**

   ```sh
   cd terraform
   cp terraform.tfvars.example terraform.tfvars   # then edit profile/region/nodes
   terraform init
   terraform plan
   terraform apply
   cd ..
   ```

2. **Register the nodes with litek8s** (`litek8s node add`). `deploy.sh` does this
   for every node in `terraform output hetero_nodes`, passing
   `--ssh-key ~/.ssh/id_ed25519` and `--install` (cross-builds + ships the agent):

   ```sh
   litek8s node add node-0 --ssh ubuntu@<ip> --ssh-key ~/.ssh/id_ed25519 --install
   ```

3. **Deploy the app** (`litek8s apply`) and smoke-test it. `deploy.sh` applies
   `deploy/app.yaml` then `deploy/service.yaml`, then curls the Service:

   ```sh
   ./deploy.sh
   ```

   Inspect with `litek8s get deployments`, `litek8s get services`, and
   `litek8s logs {{PROJECT_NAME}}`. (Note: `litek8s get pods` does not exist.)

## Customizing

- Edit `terraform/terraform.tfvars` to change the AWS profile/region and the set
  of `hetero_nodes` (name/type/arch/disk). Each node `name` is the logical name
  you reference from a manifest's `spec.template.spec.nodeName`.
- Edit `deploy/app.yaml` to change the image, ports, resource limits, readiness
  probe, or to mount a host directory (see the commented `volumes` /
  `volumeMounts` example).
- Edit `deploy/service.yaml` to change the local `listenAddr` or `targetPort`.

Regenerate this scaffold at any time with `litek8s infra init`.
