# CustomResourceDefinitions y server-side apply

`litek8s apiserver` valida CustomResourceDefinitions y recursos personalizados
como lo hace un API server de Kubernetes 1.33, así que un operador desarrollado
contra litek8s ve los mismos objetos aceptados y rechazados, con los mismos
mensajes de error, que en un cluster real. Los casos de
`compat/cases/L1-crd.sh` lo fijan; cada uno de ellos pasa en k3s 1.33 y en
litek8s.

## Crear o actualizar un CRD

Además de los nombres, las versiones y la conversión, se revisan los esquemas:

- Las reglas de `x-kubernetes-validations` se compilan y se **verifican sus
  tipos** contra el esquema: `self` (y `oldSelf`) tienen los tipos declarados,
  así que un campo no definido, una sobrecarga incorrecta
  (`self.replicas == 'two'`), una regla que no es bool, un `messageExpression`
  que no es string o una función desconocida se rechazan con el mensaje y la
  posición de cel-go (`ERROR: <input>:1:5: undefined field
  'foo'`).
- `oldSelf` solo se permite donde el valor anterior se puede correlacionar (no
  dentro de listas atomic o set); `optionalOldSelf`, solo en reglas que usan
  `oldSelf`; se revisan `reason`, `fieldPath` y `message`.
- El **costo estimado** de cada regla debe caber en el presupuesto (10⁷ por
  regla, 10⁸ por CRD), con el estimador del API server: los tamaños vienen de
  `maxLength`, `maxItems` y `maxProperties`, o del límite de 3 MiB por petición
  cuando un campo no tiene límite. Los bucles anidados sobre listas sin límite,
  o `string(int)` concatenado en un `messageExpression`, se rechazan
  (`estimated rule cost
  exceeds budget by factor of more than 100x (try ... adding maxItems,
  maxProperties, and maxLength ...)`). El estimador se calibró contra k3s con 84
  formas de reglas (factores idénticos) y acepta los 144 CRDs reales que se
  probaron (los CRDs de los charts del corpus y Gateway API v1.3).
- `pattern` debe ser una regex RE2 válida; `x-kubernetes-list-type` y
  `x-kubernetes-map-type` deben usarse en arrays/objetos con valores válidos,
  las llaves de las list-maps deben ser obligatorias o tener valor por defecto,
  y los elementos de los sets deben ser atomic.

Al actualizar, el costo de las reglas que el CRD anterior ya tenía no se vuelve
a revisar.

## Crear o actualizar un recurso personalizado

En orden: se podan los campos desconocidos, se completan los valores por
defecto, se descartan los nulls en campos no nullable (o se reemplazan por el
valor por defecto), y luego:

1. **Validación de valores OpenAPI**: tipos, `enum`, `required`, límites de
   strings y números, `multipleOf`, `minProperties`/`maxProperties`, `pattern`,
   `format` (`date-time`, `date`, `duration`, `email`, `hostname`, `ipv4`,
   `ipv6`, `cidr`, `uri`, `uuid`/`uuid3-5`, `byte`, `mac`, `hexcolor`, ...;
   los demás formatos no se revisan, como en upstream), `allOf`/`anyOf`/`oneOf`/`not`,
   y unicidad en las listas `x-kubernetes-list-type: set` y `map`.
2. **Reglas CEL**, a menos que el paso 1 haya encontrado un error de tipo, de
   formato o de enum (en ese caso, `some validation rules were not checked because the object was invalid`).
   Las reglas de transición (las que usan `oldSelf`) corren solo en
   actualizaciones, y `optionalOldSelf` entrega `oldSelf` como un opcional al
   crear. Los fallos usan `message`, `messageExpression`, `reason` y
   `fieldPath` como en upstream.
3. **Ratcheting**: al actualizar, un valor igual a su valor anterior no se
   vuelve a validar, así que endurecer un esquema no rompe los objetos
   existentes hasta que el campo cambia.

Las escrituras en el subrecurso status se validan de la misma forma.

El entorno CEL coincide con lo que 1.33 ofrece a las reglas de los CRDs: la
biblioteca estándar, la extensión de strings (`charAt`, `indexOf`, `replace`, `split`,
`substring`, `trim`, `join`, `format`, `strings.quote`, ...), `find`/
`findAll` de regex, la biblioteca de listas (`isSorted`, `sum`, `min`, `max`, `indexOf`),
`url`/`isURL`, `quantity`/`isQuantity`, `ip`/`cidr`, `sets.*`, los formatos con
nombre de `format.*`, los opcionales (`a.?b`, `m[?k]`, `optional.of`), las
comprehensions de dos variables (`all(i, v, p)`, `transformList`, `transformMap`, ...),
timestamps y durations (los campos `format: date-time`/`date` son timestamps,
los campos `format: duration` son durations, los campos `format: byte` son
bytes). Lo que no está en 1.33, y por lo tanto tampoco aquí: `math.*`,
`base64.*`, `semver`, `cel.bind`, las extensiones de listas (`sort`, `slice`, ...).

## Gestión de campos y server-side apply

Cada escritura registra `metadata.managedFields` (`fieldsV1`):

- Las **actualizaciones** (create, update, patch, scale, y los controladores
  internos del proceso, que escriben como `kube-controller-manager`) le dan a
  su manager los campos que agregaron o cambiaron, y se los quitan a otros
  managers. El manager es `?fieldManager=` o el user agent del cliente hasta la
  primera `/` (kubectl envía `kubectl-create`, `kubectl-edit`, `kubectl-patch`, ...).
- Los **applies** (`kubectl apply --server-side`) fusionan la configuración en
  el objeto siguiendo el esquema: los elementos de `x-kubernetes-list-type: map`
  se fusionan por sus llaves, los de `set` por valor, y las listas atomic y los
  mapas `x-kubernetes-map-type: atomic` se reemplazan completos; los tipos
  integrados fusionan `containers`, `env`, `volumes`, `ports`, ... por sus merge
  keys. Cambiar un campo que pertenece a otro manager es un **conflicto 409**
  (`Apply failed with 1 conflict: conflict with "alice": .spec.replicas`)
  a menos que se use `--force-conflicts`, que toma el campo. Aplicar el mismo
  valor lo vuelve copropiedad. Un campo que el manager aplicó antes y que ahora
  omite se **elimina**, a menos que otro manager todavía sea su dueño.
- `status` no se posee a través del recurso principal, ni `spec` a través del
  subrecurso status. Una petición puede reemplazar managedFields, o vaciarlos
  con `managedFields: [{}]`. Los objetos sin managedFields (escritos antes de
  que esto existiera) le dan todo a `before-first-apply` en el primer apply.

Sin hacer: conversión de versiones de los conjuntos de campos gestionados (CRDs
con varias versiones cuyos esquemas difieren en los nombres de los campos), y
atribuir a su propio manager los cambios que hacen los mutating admission
webhooks.

## Versiones de almacenamiento

`status.storedVersions` lista todas las versiones en las que todavía puede haber
objetos almacenados. Empieza con la versión de almacenamiento, incorpora cada
nueva versión de almacenamiento cuando la del CRD cambia, y solo se reduce
mediante actualizaciones de status. Una versión que está en `storedVersions` no
se puede quitar de `spec.versions`
(`missing from spec.versions; v1 was previously a storage version, and must
remain in spec.versions until a storage migration ensures no data remains
persisted in v1 ...`).

Lo que litek8s **no** hace es migrar los objetos almacenados: como en upstream,
los objetos conservan la versión con la que se escribieron hasta que algo los
reescribe. Con `conversion: None` esto es invisible (las lecturas en cualquier
versión servida solo reescriben `apiVersion`); con un webhook de conversión, los
objetos se convierten al leerlos. Para eliminar una versión antigua, reescribe
todos los objetos (una actualización sin cambios, o el
kube-storage-version-migrator) y luego quita la versión de
`status.storedVersions` con una actualización de status.

## Diferencias restantes

- Las listas de errores tienen un orden estable (el orden de upstream sigue la
  iteración de mapas de Go y varía entre peticiones).
- Los límites de costo en tiempo de ejecución (el presupuesto de CEL por
  petición) no se aplican; la estimación estática al crear el CRD sí.
- Los nombres de tipos de objeto en los errores de tipos de CEL son
  `selfType<N>` con un `<N>` estable (el de upstream es aleatorio).
- Los argumentos de zona horaria de los getters de timestamps aceptan `UTC` y
  offsets fijos, no nombres IANA.
