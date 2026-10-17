# Vendored Kubernetes protobuf schemas

`generated.proto` files from [kubernetes/api](https://github.com/kubernetes/api)
[kubernetes/apimachinery](https://github.com/kubernetes/apimachinery) and
[kubernetes/apiextensions-apiserver](https://github.com/kubernetes/apiextensions-apiserver), tag
`v0.33.0` (Kubernetes 1.33), unmodified, Apache-2.0 (see `LICENSE`).

`build.rs` compiles them with protox into a descriptor set embedded in the
binary. `src/kube/protobuf.rs` uses it to decode the protobuf request bodies
kubectl sends for built-in types.

To update, re-download the same paths at a new tag:

```bash
v=v0.34.0
for gv in admissionregistration/v1 apps/v1 autoscaling/v1 autoscaling/v2 batch/v1 certificates/v1 \
          coordination/v1 core/v1 discovery/v1 events/v1 networking/v1 policy/v1 rbac/v1 \
          scheduling/v1 storage/v1; do
  curl -fsSL https://raw.githubusercontent.com/kubernetes/api/$v/$gv/generated.proto \
    -o proto/k8s.io/api/$gv/generated.proto
done
for p in pkg/apis/meta/v1 pkg/runtime pkg/runtime/schema pkg/util/intstr pkg/api/resource; do
  curl -fsSL https://raw.githubusercontent.com/kubernetes/apimachinery/$v/$p/generated.proto \
    -o proto/k8s.io/apimachinery/$p/generated.proto
done
```
