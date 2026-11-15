---
title: "Who are you? Users from OIDC, static tokens and a webhook"
description: "litek8s servers knew only certificates signed by their own CA. 0.2.25 takes kube-apiserver's OIDC, token file and webhook flags. How each works."
date: 2026-11-14
tags: [authentication, oidc, rbac, security]
docs: [auth, limitations]
translation: es/quien-eres
---

Since 0.2.12, a litek8s server authenticates every request and enforces
RBAC. But it only knew two kinds of client: a certificate signed by the
cluster's CA (you, with kubectl) and a service account token (your pods).
For one person that is enough. For a team it is awkward: every person needs
a certificate signed with the CA's key, there is no way to revoke one, and
when someone leaves you have to rotate the CA.

Most teams already have a place where their users live: Google, Entra ID,
Keycloak, Dex, GitHub through Dex. 0.2.25 takes kube-apiserver's three
flags for bringing them in. This post explains each one, the order a token
is tried in, and a test against a small OIDC issuer.

## Three ways in

| flag | what it is | use it for |
|---|---|---|
| `--token-auth-file` | a CSV of fixed tokens: `token,user,uid,"group1,group2"` | a CI runner, a script, a lab |
| `--oidc-issuer-url`, `--oidc-client-id` | ID tokens signed by an OpenID Connect provider | people, with the login they already have |
| `--authentication-token-webhook-config-file` | a service of yours answering a `TokenReview` for each token | tokens only you know how to check |

The flags, and their defaults, are kube-apiserver's, so these flags from a
`kube-apiserver` command line you already have carry over unchanged. Whoever comes in, RBAC
treats them like any other user: you bind their name or their groups to a
role.

## In what order a token is tried

A request comes with `Authorization: Bearer <token>`. litek8s looks at it in
the same order as kube-apiserver:

```rust
if let Some(user) = tokens.static_user(token) {
    return Ok(Some(user));
}
if token.matches('.').count() == 2 {            // looks like a JWT
    if let Some(oidc) = tokens.oidc_for(token) {  // its `iss` is the OIDC issuer
        return oidc.authenticate(token).await.map(Some);
    }
    return match (state.authenticate_service_account(token), tokens.webhook()) {
        (Ok(user), _) => Ok(Some(user)),
        (Err(_), Some(webhook)) => webhook.authenticate(token).await.map(Some),
        (Err(e), None) => Err(e),
    };
}
// then the node join and agent tokens, and the webhook last
```

A JWT is routed by its issuer (`iss`, read without trusting it yet): this
cluster's service accounts on one side, the OIDC provider on the other. The
signature check comes after. Only a token nobody recognizes reaches the
webhook, so the webhook does not get a request for every pod in the
cluster.

## What OIDC checks

An ID token is a JWT: a header, the claims and a signature. To trust it,
litek8s:

1. Fetches the issuer's discovery document
   (`<issuer>/.well-known/openid-configuration`), checks that it names the
   same issuer, and fetches the public keys from its `jwks_uri` (HTTPS only).
2. Verifies the signature with the key the token names (`kid`). RS256 by
   default; `--oidc-signing-algs` allows RS/PS 256 to 512, ES256, ES384 and
   EdDSA.
3. Checks the issuer, the audience (it must contain `--oidc-client-id`),
   `exp` and `nbf`, and each `--oidc-required-claim`.
4. Takes the user from `--oidc-username-claim` (default `sub`) and the
   groups from `--oidc-groups-claim`, with their prefixes.

Keys are fetched the first time they are needed and kept. If a token names
a `kid` that is not among them, the provider has probably rotated its keys,
so they are fetched again, at most once every 10 seconds. That limit stops
anyone from making the server hit the provider by sending tokens with
made-up `kid`s.

The prefixes are there to avoid collisions. By default the user is
`<issuer>#<sub>`, so an `admin` from your provider is not the same as a
local `admin`. With `email` as the claim there is no prefix, and
`email_verified` must not be false. Usernames that start with `system:` are
refused, so nobody can pass themselves off as a node or a controller.

The signature is checked with ring, the same library the binary already
used for TLS. No new dependency came in for this.

## Trying it

A small issuer in Python (about 50 lines, with the `cryptography` package)
serves the discovery document and an RSA key, and mints two ID tokens for
`jane@example.com` in group `dev-team`: one for audience `litek8s` and one
for another application. The same server answers `TokenReview`s, accepting
one CI token. The API server:

```bash
litek8s apiserver --no-node \
  --token-auth-file tokens.csv \
  --oidc-issuer-url https://127.0.0.1:17672 --oidc-client-id litek8s \
  --oidc-username-claim email --oidc-groups-claim groups --oidc-groups-prefix oidc: \
  --oidc-ca-file cert.pem \
  --authentication-token-webhook-config-file webhook.yaml
```

The startup line lists what it accepts:

```text
apiserver: listening on https://127.0.0.1:17671 (authorization mode RBAC: service account tokens, client certificates, static tokens (1), OIDC (https://127.0.0.1:17672), token webhook (https://127.0.0.1:17672/tokenreview))
```

Static token:

```text
$ kubectl --token $ALICE auth whoami
ATTRIBUTE   VALUE
Username    alice
UID         1001
Groups      [ops system:authenticated]
```

OIDC. Jane gets in, but RBAC does not let her do anything yet:

```text
$ kubectl --token $JANE auth whoami
ATTRIBUTE   VALUE
Username    jane@example.com
Groups      [oidc:dev-team system:authenticated]

$ kubectl --token $JANE get pods -n default
Error from server (Forbidden): pods is forbidden: User "jane@example.com" cannot list resource "pods" in API group "" in the namespace "default"
```

Binding the group, not the person, is what makes it scale: whoever the
provider puts in `dev-team` comes in with that role.

```text
$ kubectl create clusterrolebinding dev-team-view --clusterrole=view --group=oidc:dev-team
$ kubectl --token $JANE get pods -n default
No resources found in default namespace.
```

The same signed token, issued for another application, is refused. A token
your provider gave some other app must not open the cluster:

```text
$ kubectl --token $OTHER auth whoami
error: You must be logged in to the server (Unauthorized)
```

The webhook accepts its token and refuses one it does not know:

```text
$ kubectl --token $CI auth whoami
ATTRIBUTE   VALUE
Username    ci-bot
UID         ci-1
Groups      [ci system:authenticated]

$ kubectl --token $BAD auth whoami
error: You must be logged in to the server (Unauthorized)
```

And the issuer's log shows the caching at work. Three OIDC tokens, one fetch
of the discovery document and one of the keys. Only the two tokens nobody
else knew reached the webhook:

```text
GET /.well-known/openid-configuration
GET /keys
TokenReview for ci-runner-7f...
TokenReview for not-a-real-t...
```

## The webhook's cache

A webhook is a network call, and kubectl makes several requests per
command. The answers are cached for the token:

- **approvals** for `--authentication-token-webhook-cache-ttl`
  (default 2 minutes, as upstream);
- **rejections** for 10 seconds, so a broken script does not flood the
  service;
- **transport errors** are not cached: if the service does not answer, the
  next request tries again.

The webhook's kubeconfig says where the service is and how the server
authenticates to it: a CA, a client certificate, or a `token` or
`tokenFile`.

## For kubectl

kubectl does not do the OIDC login itself. A credential plugin such as
[kubelogin](https://github.com/int128/kubelogin) opens the browser, gets
the ID token and refreshes it. In the kubeconfig that is a user with
`exec`:

```yaml
users:
- name: jane
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1
      command: kubectl
      args: [oidc-login, get-token, --oidc-issuer-url=https://login.example.com, --oidc-client-id=litek8s]
```

## What it does not do yet

- **`--authentication-config`**: the structured configuration that allows
  several OIDC issuers and CEL claim mappings. For now, one issuer.
- **The `TokenReview` API** still checks only service account tokens. An
  aggregated API server that sends you a user's OIDC token to check will
  not get it approved.
- **A token from the configured issuer** is judged only by OIDC: if it
  fails there, it does not go on to the webhook.
- **Real providers.** We tested against the issuer above and in the unit
  tests (a real HTTPS server with discovery, keys and TokenReview: valid
  tokens, another token's signature, the wrong audience, an unknown `kid`,
  the webhook cache), not yet against Dex, Keycloak or Google. If you
  connect one, we would like to know how it goes.

The details of each flag are in [auth.md](/docs/auth/).
