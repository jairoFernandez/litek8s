---
title: "The sidecar that never arrived: admission webhooks and the controllers' pods"
description: "Sidecar injectors patched a pod made with kubectl run but not a Deployment's pods. Why the controllers skipped admission, and what changed in 0.2.25."
date: 2026-11-14
tags: [admission, webhooks, controllers, sidecars]
docs: [limitations, auth]
translation: es/el-sidecar-que-nunca-llegaba
---

Istio, Linkerd and Vault Agent add their container to your pods with a
mutating admission webhook. On litek8s 0.2.24 and earlier that worked for a
pod you created by hand with `kubectl run`, but not for a Deployment's pods.
Those got no sidecar, and policies in Kyverno or Gatekeeper did not hold
for them either.

This post explains why, how it was fixed in 0.2.25 and what happens now when
the webhook goes down.

## What a mutating webhook does

Every write to the Kubernetes API goes through the same steps before it is
stored:

1. **Authentication**: who is asking.
2. **Authorization**: whether they may do it (RBAC).
3. **Mutating admission**: webhooks that may change the object. A sidecar
   injector answers with a JSON patch that adds a container.
4. **Validation** of the object against its schema.
5. **Validating admission**: webhooks and ValidatingAdmissionPolicies that
   may only say yes or no. Kyverno and Gatekeeper live here.
6. **The store**.

When you create a Deployment, it is not you who creates its pods. The
Deployment controller creates a ReplicaSet, and the ReplicaSet controller
creates the pods. In upstream Kubernetes these controllers run in
kube-controller-manager, which is just another client of the API: it sends
the pods over HTTP like kubectl would, as user
`system:kube-controller-manager`. So its pods go through the same six steps
and the injector sees them.

## Why they skipped it in litek8s

litek8s runs the API server, the controllers and the scheduler in one
process. The controllers do not talk to the API over HTTP. They call the
server's internal functions (`create_object`, `update_object`) directly.
That saves serializing every write, opening a connection and checking a
token for each of them. But admission was in the HTTP handler, and the
direct path did not go through it.

As a result, every object the server wrote itself skipped steps 3 and 5:
the ReplicaSets, the pods, the Jobs' pods, the Events. Nobody had noticed
because without webhooks or policies nothing is missing. They only matter
once you install one.

There was a second, smaller bug: the webhooks always received
`system:admin` as the request's user, whoever was asking. That broke
policies that look at the user, such as `matchConditions` or a
ValidatingAdmissionPolicy that checks `request.userInfo`.

## Reproducing it

You do not need Istio to see it. A mutating webhook is an HTTPS server that
receives an `AdmissionReview` and answers with a patch. This one, in Python,
adds a container named `sidecar` and logs who sent it each pod:

```python
def do_POST(self):
    review = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
    req = review['request']
    print(req['operation'], req['kind']['kind'], req['userInfo']['username'], flush=True)
    patch = [{"op": "add", "path": "/spec/containers/-",
              "value": {"name": "sidecar", "image": "envoy:v1"}}]
    # ... answer with allowed: true, patchType: JSONPatch and the patch in base64
```

It is registered with a `MutatingWebhookConfiguration` for pod `CREATE` in
namespaces labelled `inject=enabled`, with `failurePolicy: Fail`. Then a
loose pod and a two-replica Deployment:

```bash
kubectl create namespace demo
kubectl label namespace demo inject=enabled
kubectl -n demo run solo --image=nginx
kubectl -n demo create deployment web --image=nginx --replicas=2
```

On 0.2.24 (`apiserver --no-node`, so nothing runs; we only look at what gets
stored):

```text
NAME                   CONTAINERS
solo                   solo,sidecar
web-7957885594-p72ck   nginx
web-7957885594-rbdpb   nginx

webhook saw:
CREATE Pod system:admin
```

The webhook received one pod, the one we created ourselves. It never heard
about the Deployment's two.

## The fix

Every write the server makes itself now goes through admission before it
reaches the store, as user `system:kube-controller-manager`, like upstream.
It goes in `admit_internal`, called by `create_object`, `update_object`,
`update_status` and the deletes:

```rust
fn admit_internal(&self, rt: &ResourceType, operation: &'static str, ns: &str, name: &str,
                  sub: Option<&str>, old: Option<&Value>, obj: Value) -> Result<Value, ApiError> {
    let user = UserInfo { username: INTERNAL_USER.into(), ..Default::default() }.authenticated();
    let attrs = admission::Attributes { rt, operation, namespace: ns, name, sub, old, dry_run: false, user: Some(&user) };
    if !admission::webhooks_configured(self) {
        admission::validating_policies(self, &attrs, &obj)?;
        return Ok(obj);
    }
    // ...
    tokio::task::block_in_place(|| handle.block_on(admission::admit(&me, &attrs, obj)))
}
```

Three details:

- **No webhooks, no cost.** If no webhook configuration exists, only the
  ValidatingAdmissionPolicies are checked, as before on the HTTP path. That
  is the usual case on a dev cluster, and those writes stay as fast as they
  were.
- **The controllers wait for the webhook.** The controllers' code is
  synchronous and a webhook is an HTTP call. `block_in_place` lets the
  controller wait on its own thread without blocking the rest of the
  runtime, the same way an upstream controller waits for its API call.
- **Defaults after mutation.** A webhook may add a container without
  `imagePullPolicy` or `terminationMessagePath`. The pod's defaults are
  applied again after the patch, as the API does.

The webhooks also get the real user of each request now: yours for what you
create, `system:kube-controller-manager` for what the controllers create.

The same demo on 0.2.25:

```text
NAME                   CONTAINERS
solo                   solo,sidecar
web-7957885594-hhd58   nginx,sidecar
web-7957885594-mtn7r   nginx,sidecar

webhook saw:
CREATE Pod system:admin
CREATE Pod system:kube-controller-manager
CREATE Pod system:kube-controller-manager
```

## When the webhook goes down

Sending the controllers' writes to an external server creates a new failure
mode: with `failurePolicy: Fail`, if the webhook does not answer, the pods
cannot be created. That is what you asked for (a pod without its sidecar may
be worse than no pod), but it has to be visible.

Before, the ReplicaSet controller recorded an event only when a create
failed with a 403 (a quota, for instance). Every other error only reached
the server's log. Now any error other than a conflict records a
`FailedCreate` event and the ReplicaSet's `ReplicaFailure` condition, as
upstream does, and the controller retries in 5 seconds.

With the webhook stopped, we scaled the Deployment to 4:

```text
$ kubectl -n demo get events --field-selector reason=FailedCreate
COUNT   OBJECT           MESSAGE
4       web-7957885594   Error creating: Internal error occurred: failed calling webhook "sidecar.example.com": failed to call webhook: Post "https://127.0.0.1:17656/mutate?timeout=10s": failed to connect to 127.0.0.1:17656: Connection refused (os error 61)

$ kubectl -n demo get rs -o jsonpath='{.items[0].status.conditions[?(@.type=="ReplicaFailure")].reason}'
FailedCreate

$ kubectl get --raw /readyz
ok
```

The API server stays healthy: only the writes that need the webhook stop.
With the webhook back up, the two missing pods come out at the next retry,
with their sidecar, and the condition clears:

```text
NAME                   CONTAINERS
solo                   solo,sidecar
web-7957885594-7htld   nginx,sidecar
web-7957885594-h5rnr   nginx,sidecar
web-7957885594-rpd8f   nginx,sidecar
web-7957885594-v8qwt   nginx,sidecar
```

If you run a webhook on the same cluster it serves, the usual advice holds:
leave its own namespace out with a `namespaceSelector`. Otherwise, when its
pod dies, nobody can create the next one.

## What we did not test

We tested with the webhook above, which speaks the same `AdmissionReview`
protocol as Istio's or Linkerd's injectors, and with unit tests: a mutating
webhook that adds a sidecar to a pod the server creates, a validating one
that refuses it, updates and deletes reaching the webhook, and a
ValidatingAdmissionPolicy that refuses an internal create. We have not yet
installed Istio, Linkerd, Kyverno or Gatekeeper on litek8s with this change.
If you do, we would like to know how it goes.

## In short

A single process is litek8s's advantage: no network between the API and its
controllers. But the shortcut has to keep everything that happens on the
normal path. Here it skipped admission, which is the step people extend the
most. Since 0.2.25 the controllers' writes go through the same mutating
webhooks, validating webhooks and policies as yours.
