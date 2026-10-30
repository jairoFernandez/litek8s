# GitOps con litecd

[litecd](https://github.com/ideascoldigital/litecd) es entrega continua
GitOps en un solo binario pequeño de Rust que lee los manifiestos
`Application`, `AppProject` y `ApplicationSet` de Argo CD sin cambios y
escribe el mismo `.status`. Es un proyecto aparte, construido con litek8s como
primer objetivo: en un cluster cuyo control plane entero cabe en unas pocas
decenas de MiB, los pods de Argo CD son lo más grande que corre. litecd es ese
mismo ciclo GitOps en un pod de pocos MiB.

litek8s no depende de litecd, y litecd usa solo la API pública de
Kubernetes, así que corre en cualquier cluster conforme. Esta página trata de
usar los dos juntos.

## Por qué en litek8s

Lado a lado en un cluster de desarrollo de litek8s con 20 Applications (cada
una un ConfigMap, un Service y un Deployment, sync automático con prune y
self-heal), según el
[bench](https://github.com/ideascoldigital/litecd/blob/main/bench/RESULTS.md)
de litecd:

| | Argo CD v3.5.5 (5 pods) | litecd (1 pod) |
|---|---|---|
| Memoria, sin apps | 194 MiB | 2.2 MiB |
| Memoria, 20 apps sincronizadas, en reposo | 310 MiB | 6.9 MiB |
| Memoria, pico durante una ráfaga | 326 MiB | 9.4 MiB |
| CPU en reposo, 20 apps | 0.38 % de una CPU | 0.15 % |
| Instalación → listo | 28.7 s | 0.6 s |
| 19 apps a la vez → todas Synced + Healthy | 5.7 s | 3.3 s |
| App creada → Synced + Healthy (mediana) | 0.46 s | 0.46 s |

El [ejemplo de plataforma GitOps](../../examples/platform/README.md) de
litek8s (12 Applications en waves, charts de Helm, Kustomize, un app of apps
que se gestiona a sí mismo, webhooks de Gitea, CI, Atlantis) levanta con
litecd en lugar de Argo CD y pasa su smoke test completo, con el controlador
de CD en 28 MiB frente a los 723 MiB de Argo CD.

De dónde sale la diferencia: el application controller de Argo CD observa
cada tipo de recurso que sirve el cluster y guarda el cluster en memoria, y
cada uno de sus procesos lleva su propio runtime de Go y sus cachés. litecd
observa solo los tipos que aparecen en los manifiestos de alguna app,
descarta al llegar los objetos que no rastrea, y corre como un solo proceso.

## Qué soporta

Los mismos manifiestos que Argo CD v3.5, comprobados con un corpus de
compatibilidad cuyos resultados esperados salen de Argo CD real (38 casos,
todos pasan):

- **Fuentes:** directorios de git (YAML, JSON, `recurse`, `include`,
  `exclude`), charts de Helm en git o en un repositorio de Helm, Kustomize (y
  sus opciones), jsonnet; una o varias fuentes con value files `$ref`.
- **Sync:** sync automático, prune, self-heal, sync waves, hooks (PreSync,
  Sync, PostSync, SyncFail, los hooks install/upgrade de Helm, políticas de
  borrado), reintentos, `PruneLast`, `ApplyOutOfSyncOnly`, `Replace`,
  server-side apply, `CreateNamespace`, `FailOnSharedResource`.
- **Diff:** `ignoreDifferences` (JSON pointers, rutas jq, field managers),
  desde la app y desde `argocd-cm`.
- **Salud:** checks incorporados, la propia biblioteca Lua de Argo CD y
  checks Lua desde `argocd-cm`; el patrón app of apps.
- **ApplicationSet:** generadores list, clusters, git (directorios y
  archivos), matrix y merge; fasttemplate y goTemplate.
- **Proyectos y clusters:** restricciones de AppProject y ventanas de sync,
  Applications fuera del namespace de Argo CD, destinos en otros clusters a
  partir de los Secrets de cluster de Argo CD.
- **Webhooks:** `/api/webhook` para GitHub, Gitea, Gogs y GitLab detrás de
  un Service llamado `argocd-server`, así que los webhooks hechos para Argo
  CD siguen funcionando; las firmas se validan contra `argocd-secret` como
  lo hace Argo CD.

No está: una interfaz web (se puede conservar la de Argo CD, [abajo](#la-interfaz-web-de-argo-cd-una-compatibilidad-que-nace-deprecada)), la API gRPC que usa el CLI `argocd`,
SSO/Dex, el controlador de notificaciones, los config management plugins, y
los generadores de ApplicationSet SCM provider y pull request. Para saber si
tus manifiestos usan alguno, corre `litecd check`
([abajo](#revisa-tus-manifiestos)).

## Instalación

En el cluster de tu contexto de kube actual, desde la última release de
litecd:

```bash
curl -fsSL https://raw.githubusercontent.com/ideascoldigital/litecd/main/install.sh | sh
```

El script valida el manifiesto de la release contra su `SHA256SUMS`, lo
aplica con server-side apply y espera a litecd; córrelo otra vez para
actualizar, y `LITECD_VERSION=v0.2.2` fija una release. El manifiesto
depende del cluster:

- **sin Argo CD:** `install.yaml`, con los CRDs de Argo CD (sin cambios),
  RBAC, `argocd-cm`, el Deployment de litecd y un Service llamado
  `argocd-server` para webhooks, en el namespace `argocd` (lo crea si no
  existe);
- **con Argo CD instalado en `argocd`:** `takeover.yaml`, solo el
  ServiceAccount, el RBAC y el Deployment de litecd, sin tocar los objetos
  de la instalación de Argo CD. litecd se hace a un lado hasta que migres
  ([abajo](#migrar-desde-argo-cd)).

Hay un ejemplo listo, un cluster de desarrollo con litecd y un app of apps
(un chart de Helm y una app de Kustomize en su propio proyecto, en sync
waves), en [`examples/litecd/`](../../examples/litecd/README.md):
`examples/litecd/up.sh`. A mano, en un cluster de desarrollo nuevo, son dos
comandos:

```bash
litek8s dev up
curl -fsSL https://raw.githubusercontent.com/ideascoldigital/litecd/main/install.sh | sh
```

Y luego el ciclo GitOps de siempre:

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Application
metadata:
  name: guestbook
  namespace: argocd
spec:
  project: default
  source:
    repoURL: https://github.com/argoproj/argocd-example-apps
    path: guestbook
    targetRevision: HEAD
  destination:
    server: https://kubernetes.default.svc
    namespace: guestbook
  syncPolicy:
    automated: {prune: true, selfHeal: true}
    syncOptions: [CreateNamespace=true]
```

```bash
kubectl apply -f guestbook.yaml
kubectl -n argocd get applications     # SYNC STATUS, HEALTH STATUS como con Argo CD
```

Lo mismo sirve en un cluster de servidores de litek8s
([Clusters por SSH](../clusters.md)) o en cualquier otro cluster conforme.
Para probar un cambio en litecd mismo, construye la imagen directo en el
almacén de imágenes de un cluster de desarrollo a través de su API de
Docker, sin registry:

```bash
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock
git clone https://github.com/ideascoldigital/litecd && cd litecd
deploy/build.sh                    # litecd:dev, queda dentro del cluster
kubectl create namespace argocd
kubectl apply --server-side -R -f deploy/install
```

## Migrar desde Argo CD

litecd conserva el rastreo de Argo CD (la anotación
`argocd.argoproj.io/tracking-id`, o la etiqueta que pida `argocd-cm`) y su
field manager, así que los recursos que Argo CD sincronizó siguen teniendo
dueño tras el cambio, y volver atrás son los mismos pasos al revés. Además
no le disputa una app a Argo CD: mientras el application controller de
Argo CD corra en su namespace, litecd no escribe nada para ninguna
Application (ni status, sync, prune, finalizer ni refresh por webhook) y
registra que se hace a un lado; los ApplicationSets esperan de la misma
forma al applicationset controller de Argo CD.

1. **Revisa los manifiestos.** `litecd check -f <repo gitops>` lista las
   funciones que usan y si litecd soporta cada una.
2. **Observa, sin escribir nada.** `--observe` (`LITECD_OBSERVE=true`)
   compara cada Application como lo haría litecd y registra dónde su status
   difiere del que guardó Argo CD. Corre junto a Argo CD, desde una laptop
   con un kubeconfig o dentro del cluster; dentro del cluster renderiza
   igual que Argo CD (mismo DNS, misma versión mayor de Helm), así que lo
   que difiere ahí es real:

   ```bash
   litecd controller --observe --cache-dir /tmp/litecd-observe
   ```

3. **Instala litecd** con el script de arriba: detecta Argo CD, aplica
   `takeover.yaml` y se hace a un lado.
4. **Si Argo CD se gestiona a sí mismo** (un app of apps cuyos hijos
   instalan Argo CD), apaga el sync automático de esas apps mientras Argo CD
   todavía corre, o el self-heal vuelve a levantar su controlador:

   ```bash
   kubectl -n argocd patch application <app> --type merge -p '{"spec":{"syncPolicy":{"automated":null}}}'
   ```

5. **Escala a 0 el application controller de Argo CD.** litecd toma el
   control en cuanto desaparece su último pod, y lo registra:

   ```bash
   kubectl -n argocd scale statefulset/argocd-application-controller --replicas=0
   kubectl -n argocd logs deploy/litecd | grep coexistence
   ```

6. **Conserva o quita el resto.** argocd-server, su repo server y Redis
   pueden seguir sirviendo la interfaz web de Argo CD y sus webhooks (que
   ponen la anotación de refresh que litecd respeta). Notificaciones, el
   controlador de ApplicationSets (si no hay ApplicationSets) y Dex pueden
   ir a 0 si no se usan.

Vuelta atrás: escala litecd a 0 y espera a que se vaya su pod, vuelve a
escalar las cargas de Argo CD (el repo server antes que el controlador) y
reactiva el sync automático (los hijos antes que la app raíz). Anota las
réplicas y las Applications antes del paso 4.

### Una migración en producción, medida

Un cluster k3s de 6 nodos (unas 4 CPUs y 5 GiB asignables cada uno)
corría Argo CD v3.4 en HA (Redis HA, dos repo servers, dos API servers,
login por OIDC): 59 Applications en un AppProject, ~690 recursos
gestionados, desde un repositorio git por SSH (directorios, Kustomize con
una base remota, umbrellas de Helm locales) y 13 charts de repositorios de
Helm, con sync automático, prune y self-heal en todas, sync waves, hooks de
Helm, `ignoreDifferences` (JSON pointers y jq), checks de salud en Lua y un
app of apps que instala al propio Argo CD.

- **Check:** 54 apps necesitaban solo funciones de la fase 1, 5 de la
  fase 2; nada sin soporte.
- **Observe, dentro del cluster:** 58 de 59 apps coincidieron con el status
  que guardó Argo CD. La 59 renderiza un Secret con `randAlphaNum`,
  distinto en cada render: Argo CD lo oculta cacheando manifiestos, litecd
  lo sincroniza una vez por reinicio.
- **Cambio:** los pasos 4 y 5 de arriba, conservando argocd-server, los
  repo servers y Redis para la interfaz web; luego notificaciones,
  ApplicationSet y Dex a 0. No se borró nada; el chart aleatorio se
  sincronizó una vez.

| medido con metrics-server, muestreado durante horas | antes | después |
|---|---|---|
| Memoria usada, namespace de Argo CD | 1,322 MiB, 15 pods | 594 MiB, 11 pods |
| Memoria reservada (requests) | 1,216 MiB | 608 MiB |
| CPU | ~293m | ~198m |
| El application controller | 666 MiB, 100m | litecd ~100 MiB, 0–50m en reposo |

Despliegues a través de litecd después: un commit detectado por polling en
13 s a 2 min (ese repositorio no tenía webhook), syncs de menos de un
segundo a 49 s con hooks, rollouts con todas las réplicas disponibles todo
el tiempo, y sin presión de memoria, evicciones ni reinicios en los nodos.
Quitar las cargas restantes de Argo CD (sin interfaz web) liberaría unos
600 MiB más.

## La interfaz web de Argo CD: una compatibilidad que nace deprecada

litecd todavía no tiene interfaz web propia. Mientras tanto, un cluster
puede conservar la de Argo CD (argocd-server, su repo server y Redis): las
listas de Applications, el sync y la salud funcionan igual, pero el árbol
de recursos (Deployments, ReplicaSets, Pods) queda vacío, porque
argocd-server lo lee de una caché en Redis que solo llena el controlador de
Argo CD.

Un modo de compatibilidad llena esa caché: la imagen con tag
`vX.Y.Z-argocd-ui`, `--argocd-ui-cache redis://...` (contraseña del Secret
`argocd-redis`) y una NetworkPolicy para que litecd llegue al Redis de Argo
CD. **Nace deprecado**, y existe solo hasta que litecd tenga su propia
interfaz:

- no está en el build ni en la imagen por defecto, está apagado salvo que
  se pase el flag, y avisa en el log al arrancar;
- depende del formato interno de la caché de Argo CD: no escribe nada si no
  reconoce el formato, y rechaza un árbol con un nodo que no puede
  describir por completo;
- la pestaña Diff de Argo CD sigue vacía incluso con él (es otra caché del
  controlador de Argo CD); usa `D` en `litek8s ui` ([abajo](#en-litek8s-ui));
- se quitará cuando litecd muestre él mismo los recursos de una app,
  calculados bajo demanda desde la API de Kubernetes.

Detalles: [README de litecd](https://github.com/ideascoldigital/litecd#deprecated-compatibility-argo-cds-web-ui).

## En `litek8s ui`

La vista **Apps** (`0`) de la interfaz de terminal lista las Applications
que corre Argo CD o litecd, dice qué controlador las corre y su versión, y
las sincroniza, refresca y muestra su diff solo a través de la API de
Kubernetes: ver [Apps: Argo CD y litecd](../ui.md#apps-argo-cd-y-litecd). `D`
muestra el diff con git, que litecd publica para cada app fuera de sync como
un Secret `<app>.diff` junto a ella; force y terminate son solo de Argo CD.

## Revisa tus manifiestos

`litecd check` lee un directorio de manifiestos y dice qué funciones usan y
si litecd soporta cada una:

```bash
cargo run -- check -f ruta/a/gitops      # en un checkout de litecd
```

## Más

- El [README de litecd](https://github.com/ideascoldigital/litecd#readme) y
  su [plan y diseño](https://github.com/ideascoldigital/litecd/blob/main/docs/spec.md).
- [`e2e/platform.sh`](https://github.com/ideascoldigital/litecd/blob/main/e2e/platform.sh):
  el ejemplo de plataforma de litek8s con litecd en lugar de Argo CD, de
  punta a punta.

litecd no está afiliado al proyecto Argo; "Argo" y "Argo CD" son marcas
registradas de The Linux Foundation.
