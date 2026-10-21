# metaphor

The platform's sample app. A push to `main` builds the image with Gitea
Actions (runner `lkdocker`: `docker build` on litek8s's Docker API, so the
image lands in the cluster's own image store, no registry) and commits its
tag to the `gitops` repository's `registry/environments/development`,
which Argo CD deploys at `https://metaphor-development.<domain>`.
