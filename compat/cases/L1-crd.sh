# L1: CustomResourceDefinition validation and server-side apply, as
# operator developers depend on them: x-kubernetes-validations (CEL) rules
# checked when the CRD is created and enforced on objects, pattern and
# format, validation ratcheting, list types, and field ownership with
# managedFields (two managers, conflicts, force, removal).

# expect_fail <substring> <command...>: the command fails and says <substring>.
expect_fail() {
    local want=$1
    shift
    local out
    if out=$("$@" 2>&1); then
        echo "expected a failure containing '$want', got success: $out" >&2
        return 1
    fi
    expect_contains "$out" "$want"
}

# ssa <manager> <yaml> [kubectl flags...]: server-side apply a document.
ssa() {
    local manager=$1 doc=$2
    shift 2
    printf '%s\n' "$doc" | k apply --server-side --field-manager="$manager" "$@" -f -
}

# fresh_crd <name>: delete a CRD a previous run may have left behind.
fresh_crd() {
    k delete crd "$1" --ignore-not-found --wait=true >/dev/null
}

# crd_ready <group/version>: wait until the API serves the CRD's version.
crd_ready() {
    wait_for 30 k get --raw "/apis/$1"
}

case_def crd_cel_compile L1 30 "CRD rules that do not compile or type-check are rejected"
t_crd_cel_compile() {
    local f out
    f=$(mktemp)
    cat >"$f" <<'EOF'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: celcompiles.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: celcompiles, kind: CelCompile}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            x-kubernetes-validations:
            - rule: "self.foo > 0"
            - rule: "self.replicas"
            - rule: "self.replicas == 'two'"
            - rule: "self.name.matches('[')"
            - rule: "nosuchfunction(self.name)"
            - rule: "self.replicas > 0"
              optionalOldSelf: true
            properties:
              replicas: {type: integer}
              name: {type: string, maxLength: 20}
              tags:
                type: array
                maxItems: 4
                x-kubernetes-list-type: set
                items: {type: string, maxLength: 8, x-kubernetes-validations: [{rule: "self == oldSelf"}]}
EOF
    out=$(k create --dry-run=server -f "$f" 2>&1 || true)
    rm -f "$f"
    expect_contains "$out" "x-kubernetes-validations[0].rule: Invalid value: apiextensions.ValidationRule{Rule:\"self.foo > 0\""
    expect_contains "$out" "compilation failed: ERROR: <input>:1:5: undefined field 'foo'"
    expect_contains "$out" "x-kubernetes-validations[1].rule"
    expect_contains "$out" "cel expression must evaluate to a bool"
    expect_contains "$out" "found no matching overload for '_==_' applied to '(int, string)'"
    expect_contains "$out" "invalid matches argument"
    expect_contains "$out" "undeclared reference to 'nosuchfunction'"
    expect_contains "$out" "x-kubernetes-validations[5].optionalOldSelf: Invalid value: true: may not be set if oldSelf is not used in rule"
    expect_contains "$out" "oldSelf cannot be used on the uncorrelatable portion of the schema within spec.validation.openAPIV3Schema.properties[spec].properties[tags]"
}

case_def crd_cel_cost L1 30 "CRD rules over the CEL cost budget are rejected, bounded ones accepted"
t_crd_cel_cost() {
    local out crd
    crd() { # <maxItems line> <maxLength line> <messageExpression>
        cat <<EOF
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: celcosts.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: celcosts, kind: CelCost}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            properties:
              count: {type: integer}
              names:
                type: array
                $1
                items: {type: string, $2}
                x-kubernetes-validations:
                - rule: "self.all(a, self.exists_one(b, a == b))"
                  message: names must be unique
                  messageExpression: "$3"
EOF
    }
    out=$(crd "x-kubernetes-list-type: atomic" "minLength: 1" "'names must be unique'" | k create --dry-run=server -f - 2>&1 || true)
    expect_contains "$out" "properties[names].x-kubernetes-validations[0].rule: Forbidden: estimated rule cost exceeds budget by factor of more than 100x (try simplifying the rule, or adding maxItems, maxProperties, and maxLength where arrays, maps, and strings are declared)"
    expect_contains "$out" "x-kubernetes-validations estimated rule cost total for entire OpenAPIv3 schema exceeds budget"
    out=$(crd "maxItems: 16" "maxLength: 63" "'name ' + self[0] + ' repeats'" | k create --dry-run=server -f - 2>&1)
    expect_contains "$out" "created (server dry run)"
    # string(int) has no size bound: concatenating it blows the budget.
    out=$(crd "maxItems: 16" "maxLength: 63" "'got ' + string(size(self)) + ' names'" | k create --dry-run=server -f - 2>&1 || true)
    expect_contains "$out" "x-kubernetes-validations[0].messageExpression: Forbidden: estimated messageExpression cost exceeds budget by factor of more than 100x"
}

# The CRD most cases below share.
apply_rules_crd() {
    cat <<'EOF' | k apply -f - >/dev/null
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: celrules.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: celrules, singular: celrule, kind: CelRule}
  versions:
  - name: v1
    served: true
    storage: true
    subresources: {status: {}}
    schema:
      openAPIV3Schema:
        type: object
        x-kubernetes-validations:
        - rule: "self.metadata.name.startsWith('cr-')"
          message: "name must start with cr-"
        properties:
          spec:
            type: object
            x-kubernetes-validations:
            - rule: "self.min <= self.max"
              message: "min must be <= max"
            - rule: "!has(self.mode) || self.mode != 'legacy'"
              reason: FieldValueForbidden
              fieldPath: ".mode"
              message: "legacy mode is no longer supported"
            - rule: "has(self.owner)"
              reason: FieldValueRequired
              message: "an owner is required"
            - rule: "!has(self.mode) || self.mode.size() < 6"
              messageExpression: "'mode ' + self.mode + ' is too long'"
            - rule: "self.replicas >= oldSelf.replicas"
              message: "replicas cannot be reduced"
            properties:
              min: {type: integer}
              max: {type: integer}
              replicas: {type: integer, default: 1}
              mode: {type: string, maxLength: 20}
              owner: {type: string, maxLength: 63}
              id:
                type: string
                maxLength: 36
                x-kubernetes-validations:
                - {rule: "self == oldSelf", message: "id is immutable"}
              nickname:
                type: string
                maxLength: 20
                x-kubernetes-validations:
                - {rule: "!oldSelf.hasValue() || oldSelf.value() == self", optionalOldSelf: true, message: "nickname can be set only once"}
              memory:
                type: string
                maxLength: 16
                x-kubernetes-validations:
                - {rule: "isQuantity(self) && quantity(self).isLessThan(quantity('2Gi'))", message: "memory must be a quantity below 2Gi"}
              endpoint:
                type: string
                maxLength: 256
                x-kubernetes-validations:
                - {rule: "isURL(self) && url(self).getScheme() == 'https'", message: "endpoint must be an https URL"}
              subnet:
                type: string
                maxLength: 64
                x-kubernetes-validations:
                - {rule: "isCIDR(self) && cidr(self).containsIP('10.0.0.1')", message: "subnet must contain 10.0.0.1"}
              hostname:
                type: string
                maxLength: 253
                x-kubernetes-validations:
                - {rule: "!format.dns1123Subdomain().validate(self).hasValue()", messageExpression: "format.dns1123Subdomain().validate(self).value()[0]"}
              version:
                type: string
                maxLength: 32
                x-kubernetes-validations:
                - {rule: "self.find('[0-9]+') != ''", message: "version must contain a number"}
              priorities:
                type: array
                maxItems: 8
                items: {type: integer}
                x-kubernetes-validations:
                - {rule: "self.isSorted()", message: "priorities must be sorted"}
                - {rule: "self.all(i, p, p > i)", message: "priority must exceed its index"}
              window:
                type: object
                properties:
                  start: {type: string, format: date-time}
                  end: {type: string, format: date-time}
                  grace: {type: string, format: duration}
                x-kubernetes-validations:
                - {rule: "self.start < self.end", message: "start must be before end"}
                - {rule: "!has(self.grace) || self.grace <= duration('1h')", message: "grace is at most 1h"}
              labels:
                type: object
                maxProperties: 8
                additionalProperties: {type: string, maxLength: 63}
                x-kubernetes-validations:
                - {rule: "self.all(k, k.size() <= 20)", message: "label keys are short"}
                - {rule: "self.?team.orValue('') != 'root'", message: "team root is reserved"}
          status:
            type: object
            properties:
              observed:
                type: integer
                x-kubernetes-validations:
                - {rule: "self >= 0", message: "observed must not be negative"}
EOF
    crd_ready compat.litek8s.dev/v1
}

case_def crd_cel_rules L1 60 "custom objects are validated by CRD rules: message, messageExpression, reason, fieldPath"
t_crd_cel_rules() {
    use_ns
    apply_rules_crd
    local out
    out=$(printf 'apiVersion: compat.litek8s.dev/v1\nkind: CelRule\nmetadata: {name: bad, namespace: %s}\nspec: {min: 5, max: 1, mode: legacy}\n' "$NS" | k create -f - 2>&1 || true)
    expect_contains "$out" 'The CelRule "bad" is invalid'
    expect_contains "$out" '<nil>: Invalid value: "object": name must start with cr-'
    expect_contains "$out" 'spec: Invalid value: "object": min must be <= max'
    expect_contains "$out" 'spec.mode: Forbidden: legacy mode is no longer supported'
    expect_contains "$out" 'spec: Required value: an owner is required'
    expect_contains "$out" 'spec: Invalid value: "object": mode legacy is too long'
    # A rule that hits a missing field is an evaluation error, not a pass.
    out=$(printf 'apiVersion: compat.litek8s.dev/v1\nkind: CelRule\nmetadata: {name: cr-nomax, namespace: %s}\nspec: {min: 1, owner: me}\n' "$NS" | k create -f - 2>&1 || true)
    expect_contains "$out" 'spec: Invalid value: "object": no such key: max evaluating rule: min must be <= max'
    printf 'apiVersion: compat.litek8s.dev/v1\nkind: CelRule\nmetadata: {name: cr-good, namespace: %s}\nspec: {min: 1, max: 2, owner: me, mode: fast}\n' "$NS" | k create -f - >/dev/null
    # Status writes are validated too.
    expect_fail 'status.observed: Invalid value: "integer": observed must not be negative' \
        k -n "$NS" patch celrule cr-good --subresource status --type merge -p '{"status":{"observed":-1}}'
    k -n "$NS" patch celrule cr-good --subresource status --type merge -p '{"status":{"observed":3}}' >/dev/null
}

case_def crd_cel_transition L1 60 "transition rules (oldSelf) run on update: immutability, set-once, no scale-down"
t_crd_cel_transition() {
    use_ns
    apply_rules_crd
    printf 'apiVersion: compat.litek8s.dev/v1\nkind: CelRule\nmetadata: {name: cr-t, namespace: %s}\nspec: {min: 1, max: 2, owner: me, id: abc, replicas: 3}\n' "$NS" | k create -f - >/dev/null
    expect_fail 'spec.id: Invalid value: "string": id is immutable' k -n "$NS" patch celrule cr-t --type merge -p '{"spec":{"id":"xyz"}}'
    expect_fail 'spec: Invalid value: "object": replicas cannot be reduced' k -n "$NS" patch celrule cr-t --type merge -p '{"spec":{"replicas":2}}'
    k -n "$NS" patch celrule cr-t --type merge -p '{"spec":{"replicas":4}}' >/dev/null
    # optionalOldSelf: the field may be set when it was unset, then never changed.
    k -n "$NS" patch celrule cr-t --type merge -p '{"spec":{"nickname":"first"}}' >/dev/null
    expect_fail 'spec.nickname: Invalid value: "string": nickname can be set only once' \
        k -n "$NS" patch celrule cr-t --type merge -p '{"spec":{"nickname":"second"}}'
    expect_eq "$(k -n "$NS" get celrule cr-t -o jsonpath='{.spec.id}/{.spec.replicas}/{.spec.nickname}')" "abc/4/first"
}

case_def crd_cel_libraries L1 60 "CRD rules can use quantity, url, cidr, format, regex, lists, timestamps, optionals"
t_crd_cel_libraries() {
    use_ns
    apply_rules_crd
    local out
    out=$(cat <<EOF | k create -f - 2>&1 || true
apiVersion: compat.litek8s.dev/v1
kind: CelRule
metadata: {name: cr-libs, namespace: $NS}
spec:
  min: 1
  max: 2
  owner: me
  memory: 3Gi
  endpoint: http://example.com/x
  subnet: 192.168.0.0/16
  hostname: Not_A_Host
  version: vX
  priorities: [3, 1]
  window: {start: "2026-01-02T00:00:00Z", end: "2026-01-01T00:00:00Z", grace: 2h}
  labels: {team: root, averyveryverylonglabelkey: x}
EOF
)
    expect_contains "$out" 'spec.memory: Invalid value: "string": memory must be a quantity below 2Gi'
    expect_contains "$out" 'spec.endpoint: Invalid value: "string": endpoint must be an https URL'
    expect_contains "$out" 'spec.subnet: Invalid value: "string": subnet must contain 10.0.0.1'
    expect_contains "$out" "spec.hostname: Invalid value: \"string\": a lowercase RFC 1123 subdomain must consist of lower case alphanumeric characters"
    expect_contains "$out" 'spec.version: Invalid value: "string": version must contain a number'
    expect_contains "$out" 'spec.priorities: Invalid value: "array": priorities must be sorted'
    expect_contains "$out" 'spec.priorities: Invalid value: "array": priority must exceed its index'
    expect_contains "$out" 'spec.window: Invalid value: "object": start must be before end'
    expect_contains "$out" 'spec.window: Invalid value: "object": grace is at most 1h'
    expect_contains "$out" 'spec.labels: Invalid value: "object": label keys are short'
    expect_contains "$out" 'spec.labels: Invalid value: "object": team root is reserved'
    cat <<EOF | k create -f - >/dev/null
apiVersion: compat.litek8s.dev/v1
kind: CelRule
metadata: {name: cr-libs, namespace: $NS}
spec:
  min: 1
  max: 2
  owner: me
  memory: 512Mi
  endpoint: https://example.com/x
  subnet: 10.0.0.0/8
  hostname: api.example.com
  version: v12
  priorities: [1, 2, 5]
  window: {start: "2026-01-01T00:00:00Z", end: "2026-01-02T00:00:00Z", grace: 30m}
  labels: {team: web}
EOF
}

case_def crd_pattern_format L1 60 "pattern and format are enforced with the API server's messages"
t_crd_pattern_format() {
    use_ns
    cat <<'EOF' | k apply -f - >/dev/null
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: formats.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: formats, kind: Format}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            x-kubernetes-validations:
            - rule: "true"
            properties:
              code: {type: string, pattern: '^[A-Z]{3}-[0-9]+$'}
              when: {type: string, format: date-time}
              day: {type: string, format: date}
              timeout: {type: string, format: duration}
              email: {type: string, format: email}
              host: {type: string, format: hostname}
              ipv4: {type: string, format: ipv4}
              ipv6: {type: string, format: ipv6}
              net: {type: string, format: cidr}
              link: {type: string, format: uri}
              uid: {type: string, format: uuid}
              blob: {type: string, format: byte}
EOF
    crd_ready compat.litek8s.dev/v1
    local out
    out=$(cat <<EOF | k create -f - 2>&1 || true
apiVersion: compat.litek8s.dev/v1
kind: Format
metadata: {name: bad, namespace: $NS}
spec: {code: abc-1, when: yesterday, day: 2026-13-01, timeout: soon, email: nope, host: -bad-, ipv4: 1.2.3, ipv6: zz, net: 10.0.0.0/33, link: "::", uid: x, blob: "!!"}
EOF
)
    expect_contains "$out" "spec.code: Invalid value: \"abc-1\": spec.code in body should match '^[A-Z]{3}-[0-9]+\$'"
    expect_contains "$out" 'spec.when: Invalid value: "yesterday": spec.when in body must be of type date-time: "yesterday"'
    expect_contains "$out" 'spec.day: Invalid value: "2026-13-01": spec.day in body must be of type date: "2026-13-01"'
    expect_contains "$out" 'spec.timeout: Invalid value: "soon": spec.timeout in body must be of type duration: "soon"'
    expect_contains "$out" 'spec.email: Invalid value: "nope": spec.email in body must be of type email: "nope"'
    expect_contains "$out" 'spec.host: Invalid value: "-bad-": spec.host in body must be of type hostname: "-bad-"'
    expect_contains "$out" 'spec.ipv4: Invalid value: "1.2.3": spec.ipv4 in body must be of type ipv4: "1.2.3"'
    expect_contains "$out" 'spec.ipv6: Invalid value: "zz": spec.ipv6 in body must be of type ipv6: "zz"'
    expect_contains "$out" 'spec.net: Invalid value: "10.0.0.0/33": spec.net in body must be of type cidr: "10.0.0.0/33"'
    expect_contains "$out" 'spec.link: Invalid value: "::": spec.link in body must be of type uri: "::"'
    expect_contains "$out" 'spec.uid: Invalid value: "x": spec.uid in body must be of type uuid: "x"'
    expect_contains "$out" 'spec.blob: Invalid value: "!!": spec.blob in body must be of type byte: "!!"'
    # Format errors stop the CEL rules from running.
    expect_contains "$out" '<nil>: Invalid value: "null": some validation rules were not checked because the object was invalid; correct the existing errors to complete validation'
    cat <<EOF | k create -f - >/dev/null
apiVersion: compat.litek8s.dev/v1
kind: Format
metadata: {name: good, namespace: $NS}
spec: {code: ABC-12, when: "2026-10-18T12:00:00Z", day: 2026-10-18, timeout: 1h30m, email: ops@example.com, host: api.example.com, ipv4: 10.1.2.3, ipv6: "fd00::1", net: 10.0.0.0/8, link: "https://example.com/a?b=c", uid: 123e4567-e89b-12d3-a456-426614174000, blob: aGVsbG8=}
EOF
}

case_def crd_value_validations L1 60 "OpenAPI value validations: bounds, multipleOf, properties, oneOf/anyOf/not, nullable"
t_crd_value_validations() {
    use_ns
    cat <<'EOF' | k apply -f - >/dev/null
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: bounds.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: bounds, kind: Bound}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            properties:
              size: {type: integer, minimum: 2, maximum: 10, multipleOf: 2}
              ratio: {type: number, maximum: 1, exclusiveMaximum: true}
              name: {type: string, minLength: 2, maxLength: 5}
              selector:
                type: object
                properties: {byName: {type: string}, byLabel: {type: string}}
                oneOf: [{required: [byName]}, {required: [byLabel]}]
              target:
                type: object
                properties: {host: {type: string}, ip: {type: string}}
                anyOf: [{required: [host]}, {required: [ip]}]
              tags: {type: object, maxProperties: 2, additionalProperties: {type: string}}
              note: {type: string}
              comment: {type: string, nullable: true}
EOF
    crd_ready compat.litek8s.dev/v1
    local out
    out=$(cat <<EOF | k create -f - 2>&1 || true
apiVersion: compat.litek8s.dev/v1
kind: Bound
metadata: {name: bad, namespace: $NS}
spec: {size: 11, ratio: 1, name: abcdef, selector: {byName: a, byLabel: b}, target: {}, tags: {a: "1", b: "2", c: "3"}}
EOF
)
    expect_contains "$out" 'spec.size: Invalid value: 11: spec.size in body should be less than or equal to 10'
    expect_contains "$out" 'spec.size: Invalid value: 11: spec.size in body should be a multiple of 2'
    expect_contains "$out" 'spec.ratio: Invalid value: 1: spec.ratio in body should be less than 1'
    expect_contains "$out" 'spec.name: Too long: may not be more than 5 bytes'
    expect_contains "$out" '"spec.selector" must validate one and only one schema (oneOf). Found 2 valid alternatives'
    expect_contains "$out" '"spec.target" must validate at least one schema (anyOf)'
    expect_contains "$out" 'spec.tags: Too many: 3: must have at most 2 items'
    # null in a non-nullable field is dropped; a nullable one keeps it.
    printf 'apiVersion: compat.litek8s.dev/v1\nkind: Bound\nmetadata: {name: nulls, namespace: %s}\nspec: {size: 4, note: null, comment: null}\n' "$NS" | k create -f - >/dev/null
    expect_eq "$(k -n "$NS" get bound nulls -o jsonpath='{.spec}')" '{"comment":null,"size":4}'
}

case_def crd_ratcheting L1 60 "validation ratcheting: unchanged values that no longer validate are kept"
t_crd_ratcheting() {
    use_ns
    local crd=ratchets.compat.litek8s.dev
    fresh_crd "$crd"
    cat <<'EOF' | k create -f - >/dev/null
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: ratchets.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: ratchets, kind: Ratchet}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            properties:
              code: {type: string}
              other: {type: string}
EOF
    crd_ready compat.litek8s.dev/v1
    printf 'apiVersion: compat.litek8s.dev/v1\nkind: Ratchet\nmetadata: {name: r, namespace: %s}\nspec: {code: lower, other: a}\n' "$NS" | k create -f - >/dev/null
    # Tighten the schema: the stored value no longer validates.
    k patch crd "$crd" --type json -p '[{"op":"add","path":"/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/code/pattern","value":"^[A-Z]+$"},{"op":"add","path":"/spec/versions/0/schema/openAPIV3Schema/properties/spec/properties/code/x-kubernetes-validations","value":[{"rule":"self.size() > 5","message":"code is too short"}]}]' >/dev/null
    sleep 2
    k -n "$NS" patch ratchet r --type merge -p '{"spec":{"other":"b"}}' >/dev/null
    expect_fail "spec.code in body should match '^[A-Z]+\$'" k -n "$NS" patch ratchet r --type merge -p '{"spec":{"code":"still"}}'
    expect_fail 'spec.code: Invalid value: "string": code is too short' k -n "$NS" patch ratchet r --type merge -p '{"spec":{"code":"OK"}}'
    k -n "$NS" patch ratchet r --type merge -p '{"spec":{"code":"UPPERCASE"}}' >/dev/null
}

case_def crd_list_types L1 60 "x-kubernetes-list-type set and map: uniqueness and CRD checks"
t_crd_list_types() {
    use_ns
    local out
    out=$(cat <<'EOF' | k create --dry-run=server -f - 2>&1 || true
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: badlists.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: badlists, kind: BadList}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          ports:
            type: array
            x-kubernetes-list-type: map
            x-kubernetes-list-map-keys: [name]
            items: {type: object, properties: {name: {type: string}}}
          weird: {type: object, x-kubernetes-list-type: set}
EOF
)
    expect_contains "$out" "properties[ports].items.properties[name].default: Required value: this property is in x-kubernetes-list-map-keys, so it must have a default or be a required property"
    expect_contains "$out" 'properties[weird].type: Invalid value: "object": must be array if x-kubernetes-list-type is specified'
    cat <<'EOF' | k apply -f - >/dev/null
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: taglists.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: taglists, kind: TagList}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            properties:
              tags: {type: array, x-kubernetes-list-type: set, items: {type: string}}
              ports:
                type: array
                x-kubernetes-list-type: map
                x-kubernetes-list-map-keys: [name]
                items: {type: object, required: [name], properties: {name: {type: string}, port: {type: integer}}}
EOF
    crd_ready compat.litek8s.dev/v1
    out=$(printf 'apiVersion: compat.litek8s.dev/v1\nkind: TagList\nmetadata: {name: dup, namespace: %s}\nspec: {tags: [a, b, a], ports: [{name: http, port: 80}, {name: http, port: 81}]}\n' "$NS" | k create -f - 2>&1 || true)
    expect_contains "$out" 'spec.tags[2]: Duplicate value: "a"'
    expect_contains "$out" 'spec.ports[1]: Duplicate value: map[string]interface {}{"name":"http"}'
}

case_def crd_stored_versions L1 60 "changing a CRD's storage version keeps status.storedVersions"
t_crd_stored_versions() {
    use_ns
    local crd=versioneds.compat.litek8s.dev
    crd_versions() { # <v1 storage> <v2 storage>
        cat <<EOF
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: versioneds.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: versioneds, kind: Versioned}
  versions:
  - {name: v1, served: true, storage: $1, schema: {openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}}}
  - {name: v2, served: true, storage: $2, schema: {openAPIV3Schema: {type: object, x-kubernetes-preserve-unknown-fields: true}}}
EOF
    }
    fresh_crd "$crd"
    crd_versions true false | k apply -f - >/dev/null
    crd_ready compat.litek8s.dev/v2
    printf 'apiVersion: compat.litek8s.dev/v1\nkind: Versioned\nmetadata: {name: o, namespace: %s}\nspec: {a: 1}\n' "$NS" | k create -f - >/dev/null
    expect_eq "$(k get crd "$crd" -o jsonpath='{.status.storedVersions}')" '["v1"]'
    crd_versions false true | k apply -f - >/dev/null
    expect_eq "$(k get crd "$crd" -o jsonpath='{.status.storedVersions}')" '["v1","v2"]'
    expect_eq "$(k -n "$NS" get versioneds.v2.compat.litek8s.dev o -o jsonpath='{.apiVersion}/{.spec.a}')" "compat.litek8s.dev/v2/1"
    # A version objects may still be stored in cannot be dropped.
    expect_fail 'status.storedVersions[0]: Invalid value: "v1": missing from spec.versions; v1 was previously a storage version, and must remain in spec.versions until a storage migration ensures no data remains persisted in v1 and removes v1 from status.storedVersions' \
        k patch crd "$crd" --type json -p '[{"op":"remove","path":"/spec/versions/0"}]'
}

case_def ssa_managed_fields L1 30 "writes record managedFields per manager and operation"
t_ssa_managed_fields() {
    use_ns
    k -n "$NS" create configmap mf --from-literal=a=1 >/dev/null
    k -n "$NS" label configmap mf team=web >/dev/null
    printf 'apiVersion: v1\nkind: ConfigMap\nmetadata: {name: mf, namespace: %s}\ndata: {b: "2"}\n' "$NS" | k apply --server-side --field-manager=tool -f - >/dev/null
    local mf
    mf=$(k -n "$NS" get configmap mf --show-managed-fields -o jsonpath='{range .metadata.managedFields[*]}{.manager}/{.operation}={.fieldsV1}{"\n"}{end}')
    expect_contains "$mf" 'tool/Apply={"f:data":{"f:b":{}}}'
    expect_contains "$mf" 'kubectl-create/Update={"f:data":{".":{},"f:a":{}}}'
    expect_contains "$mf" 'kubectl-label/Update={"f:metadata":{"f:labels":{".":{},"f:team":{}}}}'
}

case_def ssa_conflict_force L1 30 "server-side apply: conflicts between managers, --force-conflicts takes over"
t_ssa_conflict_force() {
    use_ns
    local cm
    cm() { printf 'apiVersion: v1\nkind: ConfigMap\nmetadata: {name: shared, namespace: %s}\ndata: {%s}\n' "$NS" "$1"; }
    cm 'mode: a' | k apply --server-side --field-manager=alice -f - >/dev/null
    expect_fail 'Apply failed with 1 conflict: conflict with "alice": .data.mode' ssa bob "$(cm 'mode: b')"
    # Same value: co-ownership, no conflict.
    cm 'mode: a, extra: x' | k apply --server-side --field-manager=bob -f - >/dev/null
    cm 'mode: b, extra: x' | k apply --server-side --field-manager=bob --force-conflicts -f - >/dev/null
    expect_eq "$(k -n "$NS" get configmap shared -o jsonpath='{.data.mode}')" b
    local owners
    owners=$(k -n "$NS" get configmap shared --show-managed-fields -o jsonpath='{range .metadata.managedFields[*]}{.manager}={.fieldsV1}{"\n"}{end}')
    expect_contains "$owners" 'bob={"f:data":{"f:extra":{},"f:mode":{}}}'
    if [[ "$owners" == *alice* ]]; then
        echo "alice still owns fields after the forced apply: $owners" >&2
        return 1
    fi
    # An update by another manager also takes the field, without a conflict.
    k -n "$NS" patch configmap shared --type merge -p '{"data":{"mode":"c"}}' >/dev/null
    expect_fail 'conflict with "kubectl-patch" using v1' ssa bob "$(cm 'mode: d, extra: x')"
}

case_def ssa_field_removal L1 30 "server-side apply removes fields a manager stops applying, unless co-owned"
t_ssa_field_removal() {
    use_ns
    local cm
    cm() { printf 'apiVersion: v1\nkind: ConfigMap\nmetadata: {name: rm, namespace: %s}\ndata: {%s}\n' "$NS" "$1"; }
    cm 'a: "1", b: "2", c: "3"' | k apply --server-side --field-manager=alice -f - >/dev/null
    cm 'b: "2"' | k apply --server-side --field-manager=bob -f - >/dev/null
    cm 'a: "1"' | k apply --server-side --field-manager=alice -f - >/dev/null
    # c is gone (only alice owned it); b stays (bob co-owns it).
    expect_eq "$(k -n "$NS" get configmap rm -o jsonpath='{.data}')" '{"a":"1","b":"2"}'
    # A field someone changed with an update is theirs now: alice cannot remove it.
    k -n "$NS" patch configmap rm --type merge -p '{"data":{"a":"9"}}' >/dev/null
    cm '' | k apply --server-side --field-manager=alice -f - >/dev/null
    expect_eq "$(k -n "$NS" get configmap rm -o jsonpath='{.data}')" '{"a":"9","b":"2"}'
    cm '' | k apply --server-side --field-manager=bob -f - >/dev/null
    expect_eq "$(k -n "$NS" get configmap rm -o jsonpath='{.data}')" '{"a":"9"}'
}

case_def ssa_list_types L1 60 "server-side apply follows x-kubernetes-list-type: map items merge, atomic lists conflict"
t_ssa_list_types() {
    use_ns
    cat <<'EOF' | k apply -f - >/dev/null
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: ssalists.compat.litek8s.dev}
spec:
  group: compat.litek8s.dev
  scope: Namespaced
  names: {plural: ssalists, kind: SsaList}
  versions:
  - name: v1
    served: true
    storage: true
    schema:
      openAPIV3Schema:
        type: object
        properties:
          spec:
            type: object
            properties:
              ports:
                type: array
                x-kubernetes-list-type: map
                x-kubernetes-list-map-keys: [name]
                items: {type: object, required: [name], properties: {name: {type: string}, port: {type: integer}}}
              tags: {type: array, x-kubernetes-list-type: set, items: {type: string}}
              args: {type: array, items: {type: string}}
              settings: {type: object, x-kubernetes-map-type: atomic, additionalProperties: {type: string}}
EOF
    crd_ready compat.litek8s.dev/v1
    local obj
    obj() { printf 'apiVersion: compat.litek8s.dev/v1\nkind: SsaList\nmetadata: {name: l, namespace: %s}\nspec: %s\n' "$NS" "$1"; }
    obj '{ports: [{name: http, port: 80}], tags: [a], args: ["--x"], settings: {k: v}}' | k apply --server-side --field-manager=alice -f - >/dev/null
    obj '{ports: [{name: metrics, port: 9090}], tags: [b]}' | k apply --server-side --field-manager=bob -f - >/dev/null
    expect_eq "$(k -n "$NS" get ssalist l -o jsonpath='{.spec.ports[*].name}/{.spec.tags}')" 'http metrics/["a","b"]'
    expect_fail 'conflict with "alice": .spec.args' ssa bob "$(obj '{args: ["--y"]}')"
    expect_fail 'conflict with "alice": .spec.settings' ssa bob "$(obj '{settings: {k: w}}')"
    expect_fail 'conflict with "alice": .spec.ports[name="http"].port' ssa bob "$(obj '{ports: [{name: http, port: 81}]}')"
    # alice drops her port and tag: only bob's stay.
    obj '{args: ["--x"], settings: {k: v}}' | k apply --server-side --field-manager=alice -f - >/dev/null
    expect_eq "$(k -n "$NS" get ssalist l -o jsonpath='{.spec.ports[*].name}/{.spec.tags}/{.spec.args}')" 'metrics/["b"]/["--x"]'
}

case_def ssa_builtin_lists L1 60 "server-side apply on a Deployment merges containers by name and env by name"
t_ssa_builtin_lists() {
    use_ns
    cat <<EOF | k apply --server-side --field-manager=alice -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: web, namespace: $NS}
spec:
  replicas: 0
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      containers:
      - {name: app, image: "nginx:1.27", env: [{name: A, value: "1"}]}
EOF
    cat <<EOF | k apply --server-side --field-manager=bob -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: web, namespace: $NS}
spec:
  template:
    spec:
      containers:
      - {name: app, env: [{name: B, value: "2"}]}
      - {name: sidecar, image: "busybox:1.36"}
EOF
    expect_eq "$(k -n "$NS" get deploy web -o jsonpath='{.spec.template.spec.containers[*].name}|{.spec.template.spec.containers[0].image}|{.spec.template.spec.containers[0].env[*].name}')" 'app sidecar|nginx:1.27|A B'
    expect_fail 'conflict with "alice": .spec.template.spec.containers[name="app"].image' ssa bob "apiVersion: apps/v1
kind: Deployment
metadata: {name: web, namespace: $NS}
spec:
  template:
    spec:
      containers:
      - {name: app, image: nginx:1.28}"
}

case_def gateway_api_cel L1 120 "Gateway API CRDs: HTTPRoutes are validated by their CEL rules"
t_gateway_api_cel() {
    use_ns
    local installed=""
    if ! k get crd httproutes.gateway.networking.k8s.io >/dev/null 2>&1; then
        k apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.3.0/standard-install.yaml >/dev/null
        installed=1
    fi
    crd_ready gateway.networking.k8s.io/v1
    wait_for 30 k -n "$NS" get httproutes
    local route out
    route() { # <filters> <path>
        cat <<EOF
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: r, namespace: $NS}
spec:
  parentRefs: [{name: gw}]
  rules:
  - matches: [{path: {type: PathPrefix, value: "$2"}}]
    filters: $1
    backendRefs: [{name: svc, port: 80}]
EOF
    }
    out=$(route '[{type: RequestRedirect, requestRedirect: {scheme: https}}, {type: URLRewrite, urlRewrite: {hostname: x.example.com}}]' /api | k create --dry-run=server -f - 2>&1 || true)
    expect_contains "$out" "May specify either httpRouteFilterRequestRedirect or httpRouteFilterRequestRewrite, but not both"
    out=$(route '[{type: RequestHeaderModifier}]' /api | k create --dry-run=server -f - 2>&1 || true)
    expect_contains "$out" "filter.requestHeaderModifier must be specified for RequestHeaderModifier filter.type"
    out=$(route '[]' /a//b | k create --dry-run=server -f - 2>&1 || true)
    expect_contains "$out" "must not contain '//' when type one of ['Exact', 'PathPrefix']"
    route '[{type: RequestHeaderModifier, requestHeaderModifier: {set: [{name: X-Env, value: prod}]}}]' /api | k create -f - >/dev/null
    if [[ -n "$installed" ]]; then
        k delete -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.3.0/standard-install.yaml --wait=false >/dev/null 2>&1 || true
    fi
}

case_def crd_validation_cleanup L1 30 "remove the CRDs the crd_* and ssa_* cases created"
t_crd_validation_cleanup() {
    k delete crd celrules.compat.litek8s.dev formats.compat.litek8s.dev bounds.compat.litek8s.dev ratchets.compat.litek8s.dev \
        taglists.compat.litek8s.dev versioneds.compat.litek8s.dev ssalists.compat.litek8s.dev --ignore-not-found --wait=false >/dev/null
}
