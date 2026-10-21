# Private registries and mirrors

Nodes pull images themselves (the crun runtime, and the Docker API of a dev
cluster, share one puller). Out of the box they trust the public CAs, speak
plain HTTP only to `localhost` / `127.0.0.1`, and pull anonymously. A
**registry configuration** adds, per registry host: a private CA, a client
certificate, `insecure_skip_verify`, credentials, plain HTTP, and mirrors
(a pull-through cache for `docker.io`, a self-hosted registry in front of
another one). It is k3s's `/etc/rancher/k3s/registries.yaml` format, so a k3s
file works as it is.

```yaml
mirrors:
  docker.io:                          # pulls of docker.io/... try these first, in order,
    endpoint:                         # then docker.io itself
      - "https://cache.lan:5000"
  registry.lan:5000:                  # plain HTTP: an http:// endpoint of its own name
    endpoint: ["http://registry.lan:5000"]
  "*":                                # every registry without its own entry
    endpoint: ["https://harbor.lan/proxy"]
    rewrite:                          # the repository at the mirrors (not at the registry)
      "^library/(.*)": "dockerhub/$1"
configs:
  gitea.platform.lan:                 # host[:port] of a registry or of a mirror endpoint
    auth:
      username: ci
      password: s3cret                # or auth: <base64 of user:password>, or identitytoken: <refresh token>
    tls:
      ca_file: /etc/litek8s/platform-ca.pem   # trusted next to the public CAs
  cache.lan:5000:
    tls:
      ca: |                           # the PEM inline (litek8s; k3s has only ca_file)
        -----BEGIN CERTIFICATE-----
        ...
      cert_file: /etc/litek8s/client.pem      # a client certificate (or cert: / key: inline)
      key_file: /etc/litek8s/client-key.pem
  "*":
    tls:
      insecure_skip_verify: false
```

How a pull uses it:

- The registry of `image:` (`docker.io` for `nginx`, `gitea.platform.lan` for
  `gitea.platform.lan/team/app:1`) picks the `mirrors` entry, else `"*"`'s.
  Its endpoints are tried in order and the registry itself last; the first
  one that has the manifest serves the whole image. When all fail, the error
  lists each one's.
- An endpoint is `https://host[:port][/path]` (no scheme means https). The
  API root is `<endpoint>/v2`, or the path itself when it ends in `/v2`.
- An endpoint of the registry's own name only sets its scheme: that is how a
  registry becomes plain HTTP (`registry.lan:5000: {endpoint:
  ["http://registry.lan:5000"]}`), and how `localhost:5000` becomes HTTPS.
  `LITEK8S_INSECURE_REGISTRIES=host:port,...` in the environment of the
  server or agent still works too.
- `configs` is looked up by the endpoint's `host:port` (just `host` for port
  443), then, for the registry itself, by the registry's name (`docker.io`),
  then `"*"`. Credentials work with registries that ask for Basic auth
  (registry:2 with htpasswd) and with token services (Docker Hub, GHCR,
  Gitea, Harbor); `identitytoken` is exchanged as an OAuth2 refresh token.
- The Docker API's `docker login` credentials win over the file for the
  registry itself (not for its mirrors).
- Relative file names (`ca_file: ca.pem`) are relative to the file.

Pods' `imagePullSecrets` are not used: credentials come from the nodes'
configuration only. `docker push` is not supported by the Docker API yet.

## Where it goes

**A server or agent you run yourself**: `litek8s apiserver` and
`litek8s agent` read `/etc/litek8s/registries.yaml` when it exists, or the
file `--registry-config FILE` names. Every node that pulls needs it, servers
included unless `--no-node`.

**Installed with `litek8s install`**: `--registries FILE` installs it as
`/etc/litek8s/registries.yaml` (mode 0600; the files it names are read and
put inline) and restarts the service when it changed:

```bash
sudo litek8s install server --registries registries.yaml
litek8s install agent --ssh pi@192.168.1.20 --join https://192.168.1.10:6443 --token K10... --registries registries.yaml
```

With `--ssh` the file is read on this machine (with its CA and key files)
and uploaded like the join token. In a [cluster file](clusters.md#a-cluster-file),
`registries: registries.yaml` (relative to the cluster file) goes to every
machine.

**Dev clusters**: `litek8s dev up` reads `~/.litek8s/registries.yaml`
(`$LITEK8S_HOME/registries.yaml` with `LITEK8S_HOME`) and gives it, files
inlined, to every node of the cluster it starts (the dev VM cannot see your
files). Nothing reads the machine's `/etc/litek8s/registries.yaml` there. A
registry on your Mac's or PC's `localhost` already works without one
([dev.md](dev.md#images)).

## Changes need a restart

The configuration is read when the server or agent starts:

- installed: `litek8s install ... --registries FILE` again (it restarts the
  service), or after editing `/etc/litek8s/registries.yaml` by hand,
  `sudo systemctl restart litek8s-server` (or `litek8s-agent`) on each machine;
- dev clusters: `litek8s dev down --name N && litek8s dev up --name N`.

An installed service's pods keep running across its restart; images already
pulled stay either way.

## Checking it

The server or agent logs `registries: 2 mirror(s), 1 config(s) from
/etc/litek8s/registries.yaml` at start, and refuses to start on an invalid
file (an unknown field, a CA without a certificate, `username` without
`password`). A pull that fails says why in the pod's events:

```text
Failed to pull image "gitea.platform.lan/team/app:1": ... TLS handshake with gitea.platform.lan (10.0.0.5:443) failed: invalid peer certificate: UnknownIssuer
Failed to pull image "...": gitea.platform.lan requires authentication: give it credentials in the registry configuration (registries.yaml), or `docker login` for the Docker API
Failed to pull image "...": gitea.platform.lan refused the credentials the registry configuration (registries.yaml) or `docker login` gave it
```

`bench/registry-e2e.sh` runs a registry:2 with TLS from a private CA and
Basic auth in a dev cluster and checks that a pod pulls from it only with
the configuration.
