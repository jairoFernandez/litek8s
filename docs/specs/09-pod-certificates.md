# Spec 09: Pod certificates y ClusterTrustBundles (camino a Agent Substrate)

- Prioridad: P1
- Estado: Implementado (fases 1-3); fase 4 es un informe
- Depende de: ninguno
- Se beneficia de: `05-sandbox-isolation`

## Contexto

[Agent Substrate](https://ate.dev) (repo `github.com/agent-substrate/substrate`, k8s.io/api v0.37) corre
agentes que pasan casi todo el tiempo dormidos: los suspende y reanuda en < 500 ms dentro de sandboxes
gVisor (o microVMs). Queremos correr las cargas de agents-generator sobre litek8s con Substrate.

Hoy la instalación falla en litek8s (API v1.33): todos sus componentes (ate-api-server, ate-controller,
atelet, el router y egress de atenet) montan volúmenes `projected` con fuentes `podCertificate` y
`clusterTrustBundle`, y su `podcertcontroller` firma `PodCertificateRequest`s. Upstream eso pide los
feature gates `ClusterTrustBundle`, `ClusterTrustBundleProjection`, `PodCertificateRequest` y
`certificates.k8s.io/v1beta1` (kind lo activa en `hack/create-kind-cluster.sh`). En litek8s un pod así
se rechaza:

```
strict decoding error: unknown field "spec.volumes[0].projected.sources[0].podCertificate"
```

Lo que Substrate usa (verificado en su código):

- `certificates.k8s.io/v1beta1` para `PodCertificateRequest` (informer + `UpdateStatus`, o v1 si está)
  y `ClusterTrustBundle` (informer, `fieldSelector=metadata.name=...`, server-side apply desde
  `egressmitmtrust_controller`).
- La clave pública la lee de `spec.stubPKCS10Request` y, si falta, de `spec.pkixPublicKey`.
- Pods con `podCertificate` `keyType: ECDSAP256`; las CAs de sus firmantes son ED25519 / ECDSAP256.

## Objetivos

1. **API**: servir `ClusterTrustBundle` (cluster-scoped) y `PodCertificateRequest` (namespaced, con
   `/status`) en `certificates.k8s.io` `v1` y `v1beta1`, con la validación de upstream 1.37 que importa
   para la seguridad (ver abajo).
2. **Esquema del Pod**: aceptar las fuentes `podCertificate` y `clusterTrustBundle` en volúmenes
   `projected` (strict decoding, protobuf, OpenAPI) y validarlas como upstream.
3. **Nodo (kubelet)**: materializarlas como el kubelet: generar la clave, crear un PCR ligado al
   pod/nodo/service account, esperar al firmante externo, escribir los archivos, renovar antes de
   expirar; escribir los trust bundles seleccionados como PEM normalizado.
4. **Informe** de qué más necesita el worker gVisor de Substrate en nodos crun (sin construir device
   plugins ni KVM).

## No objetivos

- Firmantes `kubernetes.io/*` propios (upstream tampoco implementa `kube-apiserver-client-pod`).
- La comprobación de permiso `attest` sobre el firmante al escribir un `ClusterTrustBundle` con
  `signerName` (upstream la hace en admisión; aquí queda en RBAC normal sobre el recurso). Anotado
  como diferencia.
- Claves RSA y ECDSA P-521 en el nodo: `ring` no las genera. La API las acepta; el nodo responde con
  un error claro. Substrate no las usa.

## Diseño

### Esquemas vendorizados

- `proto/k8s.io/api/certificates/v1` y `.../v1beta1`: `generated.proto` de `kubernetes/api` v0.37.2.
- `proto/k8s.io/api/core/v1/generated.proto`: se mantiene en 1.33 y se agrega sólo
  `PodCertificateProjection` y `VolumeProjection.podCertificate = 6` (texto de v0.37.2). El README lo
  documenta.
- `openapi/swagger.json`: se agregan las definiciones `io.k8s.api.certificates.v1{,beta1}.*` de
  ClusterTrustBundle / PodCertificateRequest y `io.k8s.api.core.v1.PodCertificateProjection`, y se
  reemplaza `io.k8s.api.core.v1.VolumeProjection`, todas de upstream v1.37.

### Registro

`registry.rs`: `ClusterTrustBundle` y `PodCertificateRequest` en `v1` (preferida) y `v1beta1`. Un mismo
objeto almacenado; al servirlo como `v1` se quitan los campos que v1 no tiene
(`spec.pkixPublicKey`, `spec.proofOfPossession`).

### Validación (`src/kube/certificates.rs`, DER mínimo en `src/kube/x509.rs`)

ClusterTrustBundle:
- `spec.signerName` opcional, con formato `dominio/ruta`; inmutable. Si está, el nombre debe empezar
  por el firmante con `/` → `:` más `:`; si no está, el nombre no puede tener ese prefijo.
- `spec.trustBundle`: obligatorio, ≤ 1 MiB, sólo bloques PEM `CERTIFICATE` sin headers, cada uno un
  X.509 válido con basicConstraints CA, sin duplicados.

PodCertificateRequest (create):
- `signerName` cualificado; `podName`, `podUID`, `serviceAccountName`, `serviceAccountUID`,
  `nodeName`, `nodeUID` obligatorios.
- `maxExpirationSeconds`: default 86400; entre 3600 y 7862400 (≤ 86400 para `kubernetes.io/*`).
- Clave: exactamente una de `stubPKCS10Request` (CSR PKCS#10 con firma verificada) o
  `pkixPublicKey` + `proofOfPossession` (firma del `podUID`: Ed25519 sobre los bytes, ECDSA sobre su
  SHA-256). Tipos: RSA3072/4096, ECDSA P256/P384/P521, ED25519. `ring` no verifica firmas P-521 ni
  RSA-PSS: esos pedidos se rechazan (Substrate usa P256).
- `unverifiedUserAnnotations`: claves con prefijo de dominio, ≤ 256 KiB en total.
- Coherencia (lo que upstream hace con NodeRestriction + strategy): el pod existe con ese UID, en ese
  nodo, con esa service account, y el UID de nodo y SA coinciden. Así nadie pide un certificado a
  nombre de otro pod.
- `spec` inmutable en updates.

PodCertificateRequest (`/status`):
- Condiciones `Issued`, `Denied`, `Failed`: a lo sumo una, con `status: "True"`; una vez puesta no se
  quita.
- `Issued` exige `certificateChain` (PEM de certificados válidos), `notBefore`, `notAfter`,
  `beginRefreshAt`; `notBefore`/`notAfter` iguales a los de la hoja; la clave pública de la hoja es la
  del pedido; vida entre 3600 s y `maxExpirationSeconds`; `beginRefreshAt` entre `notBefore` y
  `notAfter - 10min`. Todo inmutable una vez puesto.

Pod (`projected.sources[*]`):
- `podCertificate`: `signerName` cualificado, `keyType` del enum, `maxExpirationSeconds` en rango,
  `credentialBundlePath` o (`keyPath` y `certificateChainPath`), rutas relativas sin `..`, no
  repetidas; `userAnnotations` como arriba.
- `clusterTrustBundle`: `path` obligatorio; `name` o `signerName` (exclusivos); `labelSelector` sólo
  con `signerName`.

### Nodo

Conexión agente ↔ servidor (`protocol.rs`, `hub.rs`): el agente ya pide objetos con `Get`. Se agregan
tres "recursos" de esa RPC, todos acotados a pods de ese nodo en el servidor:

- `clustertrustbundles`: `name` es la proyección en JSON; responde `{"bundles": [...]}`.
- `podcertificaterequests`: get de un PCR del namespace (sólo si su `nodeName` es el del agente).
- `podcertificaterequests/create`: el servidor completa `nodeName`/`nodeUID`, `podUID` y la SA desde
  el pod real del nodo y crea el PCR por la ruta normal de creación (validación + watch).

El hub reenvía cambios de `clustertrustbundles` como `ObjectChanged`; los pods que proyectan alguno
reescriben el volumen.

`agent/podcert.rs`: estado por (pod, volumen, fuente), persistido en
`<root>/pods/<uid>/podcerts/<volumen>-<i>.json` (0600) para sobrevivir reinicios del agente:

1. sin certificado ni pedido → genera la clave (P256/P384/ED25519, PKCS#8), crea el PCR con
   `stubPKCS10Request` (CSR vacío; upstream exige exactamente una de las dos formas de clave y el
   kubelet usa ésta); el volumen falla con
   "waiting for PodCertificateRequest ... to be issued" (el pod queda en ContainerCreating y se
   reintenta cada 2 s, como el kubelet);
2. pedido emitido → guarda clave + cadena + tiempos; `Denied`/`Failed` → error visible, nuevo intento
   en el siguiente ciclo;
3. con certificado y pasado `beginRefreshAt` (o 50 % de la vida si falta) → nuevo par de claves y PCR;
   mientras tanto sigue escribiendo el vigente;
4. escribe `credentialBundlePath` (clave + cadena), `keyPath`, `certificateChainPath` (0600 por defecto
   según `defaultMode`).

El worker del pod reprograma `prepare_volumes` para la próxima renovación (o en 5 s si hay un pedido
de renovación pendiente). Al borrar el pod se borra su estado (los PCR tienen ownerReference al pod).

## Pruebas

- Unitarias: x509/DER, validaciones de CTB/PCR/pod, generación de clave + CSR verificable, normalización
  de PEM.
- `tests/kube_apiserver.rs`: pod con ambas fuentes aceptado (JSON estricto); CTB/PCR por v1 y v1beta1;
  rechazos; coherencia pod/nodo; flujo de `/status` con un firmante de prueba.
- Agente: `podcert` con un cliente falso que hace de firmante: archivos escritos, renovación, pedido
  denegado.

## Fase 4: informe del worker gVisor

Ver `docs/substrate.md` (gaps de capabilities, seccomp/AppArmor `Unconfined`, cgroups anidados,
hostPaths y device plugins en nodos crun).

## Decisiones tomadas

- Servir v1 y v1beta1 sobre el mismo objeto, sin conversión salvo quitar los dos campos de v1beta1.
- Validación de coherencia pod/nodo/SA en el servidor, no sólo confiar en el agente.
- Estado del certificado en disco por pod para no regenerar claves en cada reinicio del agente.

## Siguiente paso recomendado

Instalar Substrate sobre un clúster litek8s con nodos crun Linux y cerrar los gaps del informe de la
fase 4 (device plugins `ate.dev/*`, cgroup v2 anidado).
