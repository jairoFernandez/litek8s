# metaphor

The platform's sample app. A push to `main` runs
`.github/workflows/ci.yaml` (Gitea Actions or GitHub Actions) on a runner in
the cluster with the label `lkdocker`: `docker build` on litek8s's Docker
API puts the image straight into the cluster's own image store, no
registry. The job then sets its tag in the gitops repository's
`registry/environments/development`, which Argo CD deploys at
`https://metaphor-development.<domain>`.
