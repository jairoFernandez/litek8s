# Autenticación y RBAC

Desde esta versión, `litek8s apiserver` (y por lo tanto `litek8s install server`
y `litek8s cluster apply`) autentica a cada cliente y aplica RBAC, como lo hace
un API server de Kubernetes. Antes, cualquier cliente que llegara al puerto era
administrador del cluster, con o sin token. Los clusters de desarrollo
(`litek8s dev`) no cambian: siguen abiertos (`AlwaysAllow`) en 127.0.0.1.

## Modos

`--authorization-mode` en `litek8s apiserver` (y en `install server`, y en los
`serverArgs` de un cluster file):

| Modo | Peticiones sin autenticar / denegadas | Para qué |
|---|---|---|
| `RBAC` (por defecto en servidores) | 401 / 403 | operación normal |
| `RBAC-audit` | permitidas, registradas como `RBAC-audit: would reject ...` / `RBAC-audit: DENY ...` | encontrar el RBAC que les falta a los charts de un cluster antes de aplicarlo |
| `AlwaysAllow` | permitidas, sin chequeos | el comportamiento anterior; clusters de desarrollo |

## Quién es quién

- **Tú (kubectl, Helm, `litek8s ui`)**: un certificado de cliente firmado por la
  CA de clientes del cluster. `CN` es el usuario, cada `O` un grupo. El
  kubeconfig que escribe un servidor (`/etc/litek8s/kubeconfig.yaml`) y el
  contexto que agregan `install --ssh` y `cluster apply` contienen
  `system:admin` en `system:masters` (puede todo), válido por diez años.
- **Pods**: el token de su service account (se verifican la firma, la cuenta,
  el pod al que está ligado y su audiencia, como en TokenReview). Un token para
  otra audiencia (uno emitido para Vault) se rechaza.
- **Nodos y peers de HA**: el join token, una credencial de `system:masters`.
- **Impersonation**: cabeceras `Impersonate-User` / `-Group`, permitidas con el
  verbo `impersonate` (`kubectl --as`).
- **Anónimo**: solo `/healthz`, `/livez`, `/readyz`, `/version`, `/cacerts` y
  los documentos de descubrimiento OIDC; todo lo demás es 401.

Las ClusterRoles por defecto (cluster-admin, admin/edit/view con aggregation,
system:discovery, system:basic-user, system:public-info-viewer,
system:auth-delegator, system:monitoring, …) y sus bindings se crean en
clusters nuevos y existentes. `kubectl auth can-i` y `kubectl auth whoami`
responden de verdad.

## Actualizar un cluster creado con una versión anterior

Un servidor actualizado a esta versión arranca en modo `RBAC`. Dos cosas pueden
romperse:

### 1. Tu kubeconfig recibe 401 (`You must be logged in to the server`)

Los contextos que agregaban las versiones anteriores llevan un token de relleno,
que ahora se rechaza. Compruébalo:

```bash
kubectl --context <name> auth whoami
```

Arréglalo de cualquiera de estas formas:

- **Vuelve a ejecutar la herramienta que creó el contexto, con la CLI de esta
  versión**: pone el certificado de admin en lugar del token:

  ```bash
  litek8s cluster apply -f cluster.yaml        # clusters de un cluster file (también lkcloud)
  litek8s install server --ssh root@host ...   # un servidor instalado por SSH
  ```

  Una CLI `litek8s` anterior vuelve a escribir el token de relleno, así que
  actualiza la CLI de tu máquina (`brew upgrade litek8s`, o el script de
  instalación) antes de ejecutarlo. lkcloud llama al `litek8s` de tu `PATH` (o
  `LITEK8S_BIN`), así que lo mismo vale para `lkcloud up`.

- **Copia el kubeconfig del propio servidor** (ya tiene el certificado; su
  dirección es `127.0.0.1`, así que conserva la de tu contexto):

  ```bash
  ssh root@<server> cat /etc/litek8s/kubeconfig.yaml > /tmp/admin.yaml
  user=$(kubectl config view -o jsonpath='{.contexts[?(@.name=="<name>")].context.user}')
  KUBECONFIG=/tmp/admin.yaml kubectl config view --raw -o jsonpath='{.users[0].user.client-certificate-data}' | base64 -d > /tmp/admin.crt
  KUBECONFIG=/tmp/admin.yaml kubectl config view --raw -o jsonpath='{.users[0].user.client-key-data}' | base64 -d > /tmp/admin.key
  kubectl config set-credentials "$user" --client-certificate=/tmp/admin.crt --client-key=/tmp/admin.key --embed-certs=true
  kubectl config unset "users.$user.token"
  rm /tmp/admin.yaml /tmp/admin.crt /tmp/admin.key
  kubectl --context <name> auth whoami          # system:admin, system:masters
  ```

- **Gana tiempo**: arranca el servidor con `--authorization-mode RBAC-audit` (o
  `AlwaysAllow`) hasta que los contextos estén arreglados.

### 2. Workloads que dependían de ser admin reciben 403

Charts, operadores y jobs que llaman a la API sin traer su propia
Role/ClusterRole funcionaban solo porque todos eran admin. Casos típicos: un pod
que usa la service account `default` del namespace (Jobs con `kubectl` sueltos,
hooks de Helm, workflows de n8n o de CI que llaman a la API), runners de CI que
crean Jobs o hacen exec en pods, motores de workflows que escriben sus propios
CRs.

1. Corre el servidor en modo audit un tiempo (un día cubre la mayoría de los
   CronJobs):

   ```yaml
   # cluster file (litek8s cluster apply), o `litek8s.serverArgs` de lkcloud
   serverArgs: [--authorization-mode, RBAC-audit]
   ```

2. Lee lo que se denegaría, una línea por usuario y tipo de acceso:

   ```bash
   ssh root@<server> 'journalctl -u litek8s-server --since -24h | grep "RBAC-audit: DENY"'
   ```

   Las líneas `RBAC-audit: would reject ...` son clientes sin credenciales
   válidas (un kubeconfig viejo, un token para otra audiencia).

3. Dale a cada una de esas service accounts una Role/RoleBinding (o
   ClusterRole/ClusterRoleBinding) con exactamente lo que hizo, en el chart o
   en tu repositorio GitOps, y compruébalo:

   ```bash
   kubectl auth can-i create jobs -n ci --as=system:serviceaccount:ci:runner
   ```

4. Cambia a `RBAC` (`serverArgs: [--authorization-mode, RBAC]`, luego
   `litek8s cluster apply`). Las denegaciones se siguen registrando, una vez por
   usuario y tipo de acceso:

   ```bash
   ssh root@<server> 'journalctl -u litek8s-server | grep -E "RBAC: forbidden|authentication failed"'
   ```

Los workloads que ya traen su RBAC (cert-manager, external-secrets,
ingress-nginx, el chart de Vault con `system:auth-delegator`, la instalación de
Argo CD o de litecd) funcionan sin cambios. Los ClusterRoleBindings a `admin` o
`edit` a nivel de cluster, inofensivos antes, ahora conceden exactamente eso:
leer Secrets en todos los namespaces. Redúcelos a RoleBindings en los
namespaces que los necesitan.

## Tus propios usuarios

Firma certificados con la CA de clientes, en el servidor (una CA Ed25519,
OpenSSL 3):

```bash
cd /var/lib/litek8s/apiserver            # el --data-dir del servidor
openssl genpkey -algorithm ed25519 -out jane.key
openssl req -new -key jane.key -subj "/CN=jane/O=dev-team" -out jane.csr
openssl x509 -req -in jane.csr -CA client-ca.crt -CAkey client-ca.key -CAcreateserial \
  -days 365 -extfile <(printf "extendedKeyUsage=clientAuth") -out jane.crt
```

Después vincula a `jane` (o al grupo `dev-team`) a un rol, por ejemplo
`kubectl create rolebinding jane-edit -n team --clusterrole=edit --user=jane`.
No hay revocación: para invalidar todos los certificados, reemplaza
`<data-dir>/sa.key` (lo que también invalida todos los tokens de service
account).

## Todavía no

Autenticadores OIDC, de token estático y webhook; los autorizadores Node y
ABAC; revocación de certificados; identidad front-proxy para API servers
agregados (la autorización propia de metrics-server ve peticiones anónimas).
Ver [limitations.md](../limitations.md).
