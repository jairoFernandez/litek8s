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

No está: la interfaz web de Argo CD, la API gRPC que usa el CLI `argocd`,
SSO/Dex, el controlador de notificaciones, los config management plugins, y
los generadores de ApplicationSet SCM provider y pull request. Para saber si
tus manifiestos usan alguno, corre `litecd check`
([abajo](#revisa-tus-manifiestos)).

## Instalación en un cluster de litek8s

Los clusters de desarrollo de litek8s sirven la API de Docker Engine, así
que puedes construir la imagen directo en el almacén de imágenes del
cluster, sin registry:

```bash
litek8s dev up                     # el contexto de kubectl apunta a él
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock
git clone https://github.com/ideascoldigital/litecd && cd litecd
deploy/build.sh                    # litecd:dev, queda dentro del cluster
kubectl apply --server-side -R -f deploy/install
kubectl -n argocd rollout status deploy/litecd
```

O usa una imagen publicada en vez de construirla:

```bash
kubectl apply --server-side -R -f deploy/install
kubectl -n argocd set image deploy/litecd litecd=ghcr.io/ideascoldigital/litecd:v0.2.0
```

`deploy/install` trae los CRDs de Argo CD (sin cambios), el RBAC,
`argocd-cm` y el Deployment de litecd, todo en el namespace `argocd`. De ahí
en adelante es el ciclo GitOps de siempre:

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

Los mismos pasos sirven en un cluster de servidores de litek8s
([Clusters por SSH](../clusters.md)) con la imagen publicada.

## Migrar desde Argo CD

litecd conserva el rastreo de Argo CD (la anotación
`argocd.argoproj.io/tracking-id`, o la etiqueta que pida `argocd-cm`) y su
field manager, así que los recursos que Argo CD sincronizó siguen teniendo
dueño tras el cambio, y volver atrás son los mismos pasos al revés.

1. **Pruébalo primero sin escribir nada.** `--observe`
   (`LITECD_OBSERVE=true`) compara cada Application como lo haría litecd y
   registra en el log dónde su status difiere del que guardó Argo CD. Puede
   correr junto a Argo CD, incluso desde una laptop con un kubeconfig:

   ```bash
   litecd controller --observe --cache-dir /tmp/litecd-observe
   ```

2. **Instala litecd.** No le disputa una app a Argo CD: mientras el
   application controller de Argo CD corra en su namespace, litecd no escribe
   nada para ninguna Application (ni status, sync, prune, finalizer ni
   refresh por webhook) y registra que se hace a un lado. Los ApplicationSets
   esperan de la misma forma al applicationset controller de Argo CD. Una
   Application con la anotación `litecd.dev/manage: "true"` es de litecd en
   cualquier caso; Argo CD también actúa sobre ella, así que úsala solo para
   una app de prueba.

3. **Escala a 0 las cargas de Argo CD, o desinstálalas, conservando sus
   CRDs, Applications y Secrets.** litecd toma el control en cuanto
   desaparece el último pod del controlador, y lo registra.

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
