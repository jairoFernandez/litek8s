# CustomResourceDefinitions and server-side apply

`litek8s apiserver` validates CustomResourceDefinitions and custom resources
the way a Kubernetes 1.33 API server does, so an operator developed against
litek8s sees the same accepted and rejected objects, with the same error
messages, as on a real cluster. The cases in `compat/cases/L1-crd.sh` pin
this down; every one of them passes on k3s 1.33 and on litek8s.

## Creating or updating a CRD

On top of names, versions and conversion, the schemas are checked:

- `x-kubernetes-validations` rules are compiled and **type-checked** against
  the schema: `self` (and `oldSelf`) have the declared types, so an
  undefined field, a wrong overload (`self.replicas == 'two'`), a non-bool
  rule, a non-string `messageExpression` or an unknown function is rejected
  with cel-go's message and position (`ERROR: <input>:1:5: undefined field
  'foo'`).
- `oldSelf` is only allowed where the old value can be correlated (not under
  atomic or set lists); `optionalOldSelf` only in rules that use `oldSelf`;
  `reason`, `fieldPath`, `message` are checked.
- Each rule's **estimated cost** must fit the budget (10⁷ per rule, 10⁸ per
  CRD), with the API server's estimator: sizes come from `maxLength`,
  `maxItems` and `maxProperties`, or from the 3 MiB request limit when a
  field is unbounded. Nested loops over unbounded lists, or `string(int)`
  concatenated in a `messageExpression`, are rejected (`estimated rule cost
  exceeds budget by factor of more than 100x (try ... adding maxItems,
  maxProperties, and maxLength ...)`). The estimator was calibrated against
  k3s on 84 rule shapes (identical factors) and accepts all 144 real CRDs
  tried (the corpus charts' CRDs and Gateway API v1.3).
- `pattern` must be a valid RE2 regex; `x-kubernetes-list-type` and
  `x-kubernetes-map-type` must be used on arrays/objects with valid values,
  list-map keys must be required or defaulted, set items must be atomic.

Rules that the old CRD already had are not re-checked for cost on update.

## Creating or updating a custom resource

In order: unknown fields are pruned, defaults filled in, nulls in
non-nullable fields dropped (or replaced by the default), then:

1. **OpenAPI value validation**: types, `enum`, `required`, string and
   number bounds, `multipleOf`, `minProperties`/`maxProperties`, `pattern`,
   `format` (`date-time`, `date`, `duration`, `email`, `hostname`, `ipv4`,
   `ipv6`, `cidr`, `uri`, `uuid`/`uuid3-5`, `byte`, `mac`, `hexcolor`, ...;
   other formats are not checked, as upstream), `allOf`/`anyOf`/`oneOf`/`not`,
   and uniqueness in `x-kubernetes-list-type: set` and `map` lists.
2. **CEL rules**, unless step 1 found a type, format or enum error (then
   `some validation rules were not checked because the object was invalid`).
   Transition rules (using `oldSelf`) run on updates only, with
   `optionalOldSelf` giving `oldSelf` as an optional on create. Failures use
   `message`, `messageExpression`, `reason` and `fieldPath` like upstream.
3. **Ratcheting**: on update, a value equal to its old value is not
   re-validated, so tightening a schema does not break existing objects
   until the field changes.

Status subresource writes are validated the same way.

The CEL environment matches what 1.33 offers CRD rules: the standard
library, the strings extension (`charAt`, `indexOf`, `replace`, `split`,
`substring`, `trim`, `join`, `format`, `strings.quote`, ...), regex `find`/
`findAll`, the lists library (`isSorted`, `sum`, `min`, `max`, `indexOf`),
`url`/`isURL`, `quantity`/`isQuantity`, `ip`/`cidr`, `sets.*`, `format.*`
named formats, optionals (`a.?b`, `m[?k]`, `optional.of`), two-variable
comprehensions (`all(i, v, p)`, `transformList`, `transformMap`, ...),
timestamps and durations (`format: date-time`/`date` fields are timestamps,
`format: duration` fields durations, `format: byte` fields bytes). Not in
1.33 and so not here either: `math.*`, `base64.*`, `semver`, `cel.bind`, the
list extensions (`sort`, `slice`, ...).

## Field management and server-side apply

Every write records `metadata.managedFields` (`fieldsV1`):

- **Updates** (create, update, patch, scale, and the in-process controllers,
  which write as `kube-controller-manager`) give their manager the fields
  they added or changed, and take them from other managers. The manager is
  `?fieldManager=` or the client's user agent up to the first `/` (kubectl
  sends `kubectl-create`, `kubectl-edit`, `kubectl-patch`, ...).
- **Applies** (`kubectl apply --server-side`) merge the configuration into
  the object following the schema: `x-kubernetes-list-type: map` items merge
  by their keys, `set` items by value, atomic lists and
  `x-kubernetes-map-type: atomic` maps are replaced whole; built-in types
  merge `containers`, `env`, `volumes`, `ports`, ... by their merge keys.
  Changing a field another manager owns is a **409 conflict**
  (`Apply failed with 1 conflict: conflict with "alice": .spec.replicas`)
  unless `--force-conflicts`, which takes the field over. Applying the same
  value co-owns it. A field the manager applied before and leaves out now is
  **removed**, unless another manager still owns it.
- `status` is not owned through the main resource and `spec` not through the
  status subresource. A request may replace managedFields, or clear them
  with `managedFields: [{}]`. Objects with no managedFields (written before
  this existed) give everything to `before-first-apply` on the first apply.

Not done: version conversion of managed field sets (CRDs with several
versions whose schemas differ in field names), and attributing changes made
by mutating admission webhooks to their own manager.

## Storage versions

`status.storedVersions` lists every version objects may still be stored in.
It starts with the storage version, gains each new storage version when
the CRD's storage version changes, and only shrinks through status updates.
A version that is in `storedVersions` cannot be removed from `spec.versions`
(`missing from spec.versions; v1 was previously a storage version, and must
remain in spec.versions until a storage migration ensures no data remains
persisted in v1 ...`).

What litek8s does **not** do is migrate stored objects: like upstream, objects
keep the version they were written in until something rewrites them. With
`conversion: None` this is invisible (reads at any served version only
rewrite `apiVersion`); with a conversion webhook, objects are converted on
read. To drop an old version, rewrite every object (a no-op update, or the
kube-storage-version-migrator) and then remove the version from
`status.storedVersions` with a status update.

## Remaining differences

- Error lists are in a stable order (upstream's order follows Go map
  iteration and varies between requests).
- Runtime cost limits (the per-request CEL budget) are not enforced; the
  static estimate at CRD creation is.
- Object type names in CEL type errors are `selfType<N>` with a stable
  `<N>` (upstream's is random).
- Time zone arguments to timestamp getters accept `UTC` and fixed offsets,
  not IANA names.
