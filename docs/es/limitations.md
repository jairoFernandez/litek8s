# Limitaciones

Lista honesta; hay más en [skills/litek8s/references/differences.md](../../skills/litek8s/references/differences.md).

- **La autenticación y el RBAC son más simples que los de upstream.** `litek8s
  apiserver` (y por lo tanto `install server`, `cluster apply`) autentica a los
  clientes y aplica RBAC por defecto (`--authorization-mode RBAC`): tokens de
  service account, certificados de cliente firmados por la CA de clientes del
  cluster (CN usuario, O grupos; el kubeconfig que escribe es `system:admin` en
  `system:masters`) y el join token (una credencial de administrador).
  Las peticiones anónimas solo llegan a `/healthz`, `/livez`, `/readyz`,
  `/version` y los documentos de descubrimiento OIDC; todo lo demás recibe 401.
  `--authorization-mode RBAC-audit` decide, registra (`RBAC-audit: DENY ...`,
  y el primer uso que hace cada usuario de un verbo y un recurso como `ALLOW`)
  y permite, para encontrar el RBAC que les falta a los charts de un cluster
  antes de aplicarlo; `AlwaysAllow` es el comportamiento anterior. Los clusters
  de desarrollo (`litek8s dev`) siguen en AlwaysAllow en `127.0.0.1`. Lo que
  falta frente a upstream: autenticadores OIDC, de token estático y webhook; los
  autorizadores Node y ABAC (los agents de los nodos hablan con el servidor por
  su propia conexión autenticada); revocación de certificados de cliente (los
  certificados valen por diez años; para anular uno, rota la CA de clientes
  reemplazando `<data-dir>/sa.key`, lo que también invalida todos los tokens de
  service account); los API servers agregados (metrics-server) no reciben
  identidad front-proxy, así que su autorización delegada ve peticiones
  anónimas; el chequeo de escalamiento compara las reglas una por una (un rol
  cuyos verbos están cubiertos por varias reglas del autor en conjunto puede
  ser rechazado). Firma los certificados de tus propios usuarios con
  `<data-dir>/client-ca.{crt,key}` (una CA Ed25519; `extendedKeyUsage =
  clientAuth`). De todas formas, mantén el puerto de la API (y el de Raft, 6444)
  fuera de redes no confiables. Actualizar un cluster creado con una versión
  anterior y arreglar los 401/403: [auth.md](../auth.md).
- Proyecto de una sola persona, con semanas de vida: las APIs, los flags y los
  formatos en disco pueden cambiar entre versiones 0.x sin migración.
- No todo el comportamiento del kubelet: no hay eventos `Unhealthy`/`BackOff`,
  un `emptyDir` en memoria es un tmpfs por cada montaje en un contenedor.
- Las NetworkPolicies se aplican solo en nodos con crun (Linux); el runtime de
  Docker (servidores macOS) las guarda sin efecto. El tráfico de un nodo hacia
  sus propios pods (probes, relays de NodePort y LoadBalancer, que corren en el
  nodo) siempre se permite, y los pods con `NET_ADMIN` pueden falsificar
  direcciones de origen en el bridge de su nodo
  ([networking.md](](../networking.md#networkpolicies))).
- Los `imagePullSecrets` de los pods no se usan: los nodos descargan con las
  credenciales de su configuración de registries ([registries.md](../registries.md)),
  y la API de Docker todavía no puede hacer `docker push`.
- Las métricas cubren contenedores, nodos y estado del cluster; los endpoints
  `/metrics` de las aplicaciones no se recolectan (apunta un Prometheus a ellos).
- La VM de macOS nunca le devuelve a macOS la memoria que ya tocó (un límite de
  Virtualization.framework); en cambio, litek8s mantiene el guest pequeño
  ([dev.md](../dev.md#memoria-en-macos)).
- Los servidores macOS usan Docker para los pods y están pensados para
  desarrollo; corre servidores y nodos reales en Linux.
