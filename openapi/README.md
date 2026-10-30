# Vendored OpenAPI definitions

`swagger.json` is upstream's
[`api/openapi-spec/swagger.json`](https://github.com/kubernetes/kubernetes/blob/v1.33.0/api/openapi-spec/swagger.json)
at tag `v1.33.0`, reduced to `swagger`, `info` and `definitions` (the
`paths` are most of the file and no client reads them from litek8s), keys
sorted. Apache-2.0, like `proto/` (see `proto/LICENSE`).

From `v1.37.2` (docs/specs/09-pod-certificates.md): the
`io.k8s.api.certificates.{v1,v1beta1}` ClusterTrustBundle* and
PodCertificateRequest* definitions, `io.k8s.api.core.v1.PodCertificateProjection`
and `io.k8s.api.core.v1.VolumeProjection`. A full update to 1.37 keeps them.

`build.rs` embeds the definitions, gzipped, as compact JSON and in the gnostic
protobuf form (`src/kube/openapi_pb.rs`); `src/kube/openapi.rs` serves them
with the CRDs' schemas on `/openapi/v2` and `/openapi/v3`.

`OpenAPIv2.proto` is gnostic's `openapi_v2.Document` schema
([google/gnostic-models](https://github.com/google/gnostic-models/blob/v0.7.0/openapiv2/OpenAPIv2.proto)
v0.7.0, Apache-2.0), what client-go decodes `/openapi/v2` with. Only the tests
use it, to decode what the encoder writes.

To update to a new Kubernetes version:

```bash
v=v1.34.0
curl -fsSL https://raw.githubusercontent.com/kubernetes/kubernetes/$v/api/openapi-spec/swagger.json |
  python3 -c 'import json, sys; d = json.load(sys.stdin)
json.dump({k: d[k] for k in ("swagger", "info", "definitions")}, sys.stdout, indent=1, sort_keys=True); print()' \
  >openapi/swagger.json
```
