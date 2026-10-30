# Desarrollar litek8s sin Docker

Todo aquello para lo que el proyecto usaba Docker (OrbStack, Docker Desktop)
en una Mac ahora corre sobre el propio litek8s: `litek8s dev` (una VM de
Virtualization.framework con crun, [dev.md](../dev.md)) y la API de Docker
Engine que sirve en `~/.litek8s/docker.sock` ([docker.md](../docker.md)).

| qué | necesita | en litek8s |
|---|---|---|
| el corpus de compatibilidad | un cluster | el propio cluster de `litek8s dev` (`KUBECONFIG`) |
| el binario guest de la VM de desarrollo (`dev/vm/build-guest-binary.sh`), los binarios de benchmark para Linux (`bench/build-linux.sh`) | `docker run rust:*` con el checkout montado con bind mount | los mismos scripts, `DOCKER_HOST` en litek8s |
| la referencia de k3s (`compat/baseline-k3d.sh`) | k3d: contenedores privilegiados con containerd anidado | k3d sobre la API de Docker de litek8s, o k3s en una VM de qemu (`compat/baseline-k3s-vm.sh`) |

OrbStack (o Docker Desktop) puede quedarse instalado como respaldo: nada de
esto lo detiene, lo reconfigura ni lo usa. Quita `DOCKER_HOST` y `docker`
vuelve a hablar con él.

## Usar litek8s vs. desarrollarlo

En una misma Mac pueden convivir hasta tres litek8s sin compartir nada:

| | qué | binario | `LITEK8S_HOME` | contextos |
|---|---|---|---|---|
| **usarlo** | el release | `brew install jairoFernandez/tap/litek8s` (o install.sh) | sin definir: `~/.litek8s`, el directorio de datos, `~/.kube/config` | `litek8s-<cluster>` |
| **desarrollarlo** | este checkout | `dev/lk` (compila `target/release/litek8s` cuando está desactualizado) | `~/.litek8s-dev` | `lkdev-<cluster>` |
| **probar con él** | una revisión fijada | `dev/stable.sh` (= `dev/lk stable`) | `~/.litek8s/stable` | `lkstable-<cluster>` |

`LITEK8S_HOME=<dir>` mueve bajo `<dir>` todas las rutas por usuario: los
sockets de Docker (`docker.sock`, `<cluster>/docker.sock`), `config.yaml`,
`ui.yaml` y el estado de la UI, el kubeconfig que escribe `dev up`
(`<dir>/kubeconfig` salvo que `KUBECONFIG` esté definido), el directorio de
datos (`<dir>/data`: la VM de desarrollo, su disco, los clusters, `vm/` para el
guest). Los contextos reciben el prefijo `lkdev-` (`LITEK8S_CONTEXT_PREFIX`) y
el docker context del cluster por defecto es `lkdev`. Así, `dev/lk dev up`
arranca una VM propia, junto a la del release, y `litek8s ui` / `dev/lk ui`
muestran cada uno solo sus propios clusters. Ambos binarios dicen cuál son: un
build desde un checkout imprime `litek8s 0.1.0 DEV <commit> (LITEK8S_HOME=...)`
y la cabecera de la UI muestra `DEV <commit> · ~/.litek8s-dev`; los builds de
release (`LITEK8S_RELEASE=1`, definido por el workflow de release) imprimen la
versión sin más.

```bash
dev/lk --version                    # builds target/release/litek8s first if a source is newer
dev/lk build-vm                     # the VM's guest from this checkout, into ~/.litek8s-dev/data/vm
dev/lk dev up --cpus 4 --memory 4096
dev/lk ui
eval "$(dev/lk env)"                # DOCKER_HOST, KUBECONFIG of ~/.litek8s-dev for this shell
dev/lk dev delete                   # and rm -rf ~/.litek8s-dev to forget it entirely
```

`dev/lk build-vm` compila el guest sobre la propia API de Docker de
`~/.litek8s-dev` cuando su VM está corriendo y, si no, sobre `DOCKER_HOST` /
el docker context. El primerísimo guest necesita una VM donde compilarse:
arranca una vez el guest del release con
`LITEK8S_DEV_VM_ASSETS="$(brew --prefix)/share/litek8s/vm" dev/lk dev up`, y
luego `dev/lk build-vm && dev/lk dev down && dev/lk dev up`. `dev/lk env` no
exporta `LITEK8S_HOME`, así que el `litek8s` del PATH en esa shell sigue
siendo el del release.

## La regla: desarrolla con un litek8s fijado

El cluster y la API de Docker **con** los que desarrollas no deben ser el
código que estás **editando**: un build roto tumbaría las herramientas que lo
prueban. `dev/stable.sh` mantiene un litek8s que se sabe que funciona en
`~/.litek8s/stable` (`$LITEK8S_STABLE`): el binario de macOS, el kernel de la
VM, la imagen raíz y el initrd (con el litek8s estático de Linux como `/init`)
y la revisión. El directorio es también su `LITEK8S_HOME`: su propia VM y su
propio cluster (`data/`), `docker.sock` y kubeconfig (`~/.kube/config` no se
toca), aparte del release y de `dev/lk`. Para un binario fijado antes de que
existiera `LITEK8S_HOME`, el script apunta las variables de ruta individual
(`LITEK8S_DEV_HOME`, `LITEK8S_DOCKER_SOCKET`, `LITEK8S_CONFIG`, `KUBECONFIG`)
a los mismos lugares.

```bash
dev/stable.sh install origin/main   # or any rev; builds both binaries, see below
dev/stable.sh up --cpus 4 --memory 4096
eval "$(dev/stable.sh env)"         # DOCKER_HOST and KUBECONFIG for this shell
dev/stable.sh status | down | reset
```

`install` compila el binario de macOS con tu cargo (directorio target
`target/stable-host`) y el binario de Linux de la VM en un contenedor
`rust:1.90-alpine` sobre la API de Docker que esté vigente: la del cluster
estable en ejecución, cuando hay uno, así que después de la primera
instalación litek8s compila su propia siguiente versión. Su caché de cargo
vive en el volumen de Docker `litek8s-guest-build` (dentro del disco de la VM,
no en el checkout). Para actualizar:
`dev/stable.sh install <rev> && dev/stable.sh down && dev/stable.sh up`.

**Primera instalación en una Mac nueva.** El guest es un binario de Linux,
así que el primerísimo necesita algo de Linux: OrbStack/Docker Desktop si está
instalado (deja `DOCKER_HOST` sin definir), una VM de benchmark
(`BENCH_BUILD_VM`, ver [bench/build-linux.sh](../../bench/build-linux.sh)), o el
`target/guest-musl-arm64/release/litek8s` de otra máquina pasado a
`dev/vm/build-guest.py`. Todavía no hay releases publicados; un workflow de
release que suba el binario de macOS y los `Image`/`root.img`/`initrd.gz` de
la VM eliminaría este paso.

Mantén un consumo moderado en una laptop compartida: `--cpus 4 --memory 4096`
(los valores por defecto son la mitad de las CPUs de la Mac y 4096 MiB),
`CARGO_BUILD_JOBS=4` para los builds en la VM, un build a la vez.

## cargo test

`cargo test` no necesita ninguna API de Docker: los tests que manejaban el
daemon legacy a través de Docker (`tests/integration.rs`) se fueron con él en
la 0.2.0. Los tests del API server, de HA, de MCP y de observabilidad corren
dentro del proceso ([CONTRIBUTING.md](../../CONTRIBUTING.md#pruebas)); la
ejecución de contenedores del runtime crun se prueba solo en Linux
(bench/vm.sh).

## El corpus de compatibilidad

El cluster de `litek8s dev` es un cluster como cualquier otro:

```bash
eval "$(dev/stable.sh env)"
export HELM_CACHE_HOME=$PWD/compat/.work/helm/cache HELM_CONFIG_HOME=$PWD/compat/.work/helm/config HELM_DATA_HOME=$PWD/compat/.work/helm/data
compat/run.sh --target litek8s-dev-macos-vm
```

Resultado del 2026-10-19: **119/119** (todos los niveles), en
[compat/results/20261019-110349-litek8s-dev-macos-vm](../../compat/results/20261019-110349-litek8s-dev-macos-vm/).
Tardó 37 min con 4 vCPUs mientras la Mac estaba ocupada con otro trabajo
(load average 60–100); el corpus de 101 casos tardó 12.3 min en una Mac
inactiva (compat/RESULTS.md). Un cluster que también usas a mano puede hacer
fallar casos por restos (un release de Helm ya instalado a mano lo hizo);
`litek8s dev reset` te da uno limpio.

Esto mide el litek8s **estable**. Para medir el código que estás editando,
arranca un guest compilado desde tu checkout (siguiente sección) y ejecuta el
mismo comando.

## Builds de Linux

```bash
eval "$(dev/stable.sh env)"
CARGO_BUILD_JOBS=4 dev/vm/build-guest-binary.sh          # static musl arm64, the VM's /init
CARGO_BUILD_JOBS=4 bench/build-linux.sh arm64            # glibc, what `node add --install` runs
```

Ambos montan el checkout con bind mount (por virtiofs dentro de la VM). Con
`LITEK8S_BUILD_VOLUME=<volume>`, el directorio target y el registro de cargo
se quedan en cambio en un volumen de Docker en el disco de la VM, y solo se
copia hacia afuera el binario.

Build en frío = directorio target y registro de cargo vacíos, imagen `rust:*`
ya descargada; incremental = `touch src/lib.rs` después. Misma máquina y
condiciones que arriba, `CARGO_BUILD_JOBS=4`:

| build | litek8s, bind mount | litek8s, `LITEK8S_BUILD_VOLUME` | OrbStack, bind mount |
|---|---|---|---|
| `dev/vm/build-guest-binary.sh` (musl), en frío | 293 s | 254 s | 300 s |
| lo mismo, incremental | 193 s | 199 s | 238 s |
| `bench/build-linux.sh arm64` (glibc), en frío | 130 s | 124 s | 215 s |

virtiofs no es el cuello de botella: el volumen ahorra un 13 % en un build musl
en frío y nada en uno incremental, así que los scripts mantienen el bind mount
por defecto. (El build musl es más lento que el glibc sobre todo porque el
propio rustc corre sobre el allocator de musl.) `dev/stable.sh install`
(build de release de macOS + guest en un volumen + kernel/initramfs) tardó
473 s en frío.

Con el host muy cargado, el mismo build musl en frío tardó 700 s, y uno
incremental 600 s: las 4 vCPUs de la VM compiten con el host como cualquier
otro proceso.

### Trabajar en el guest o en la propia API de Docker

El `/init` de la VM es litek8s, así que los cambios en `src/dev/`,
`src/docker/` o `src/kube/` llegan a la VM solo a través de un guest nuevo.
`dev/lk build-vm` compila uno desde el checkout (sobre la propia API de Docker
de `~/.litek8s-dev` cuando su VM está corriendo y, si no, sobre la del cluster
estable después de `eval "$(dev/stable.sh env)"`) y el siguiente
`dev/lk dev up` lo arranca, mientras el cluster estable sigue corriendo:

```bash
dev/lk build-vm
dev/lk dev down && dev/lk dev up --cpus 4 --memory 4096
# ... test against ~/.litek8s-dev; the stable cluster is untouched
```

El build del guest es la parte lenta de este ciclo: solo el crate `litek8s` con
`opt-level=3` se lleva la mayor parte, incluso de forma incremental.

## La referencia de k3s

Cada caso del corpus tiene que pasar en un k3s real. `compat/baseline-k3d.sh`
(k3d, 1 server + 1 agent) corre sin cambios sobre la API de Docker de
litek8s:

```bash
eval "$(dev/stable.sh env)"
compat/baseline-k3d.sh                    # k3d cluster create ... then compat/run.sh
k3d cluster delete litek8s-compat
```

Los nodos de k3d son contenedores privilegiados que corren su propio
containerd dentro de la VM de desarrollo. Resultado del 2026-10-19:
**119/119**
([compat/results/20261019-114221-k3d-on-litek8s](../../compat/results/20261019-114221-k3d-on-litek8s/)),
`k3d cluster create` en 11 s, L0–L3 en 4.4 min, L4 en 36 min, los dos nodos de
k3s juntos ~1.3 GiB dentro de la VM de 4 GiB. Dos carencias de la API de
Docker estorbaban y están corregidas: Docker evalúa los filtros `name` e `id`
como expresiones regulares (k3d busca sus contenedores y su red con
`^/?k3d-...$`), y `--privileged` le da al contenedor los nodos de dispositivo
del host (el kubelet lee `/dev/kmsg`).

Sin ninguna API de Docker, `compat/baseline-k3s-vm.sh` corre la misma versión
de k3s en una VM desechable de [bench/vm.sh](../../bench/vm.sh) (Ubuntu 24.04
sobre qemu, `brew install qemu`) y tuneliza su API a `127.0.0.1:26443`;
`--down` borra la VM. Validado en L0–L1 (50/50,
[compat/results/20261019-131313-k3s-vm-baseline](../../compat/results/20261019-131313-k3s-vm-baseline/)).
Ventajas y desventajas frente a k3d: un nodo en lugar de dos, una segunda VM
junto a la VM de desarrollo (otros 4 GiB mientras corre), una imagen cloud de
600 MB en el primer uso; el arranque, la instalación de k3s y L0–L1 juntos
tardaron 102 s; pero nada de Docker, y el mismo harness que usan los
benchmarks multinodo.

## Lo que todavía necesita OrbStack

Nada en el ciclo del día a día. Lo que queda:

- **El primerísimo build del guest en una Mac nueva** (ver arriba): algún
  Linux tiene que compilar el primer litek8s estático, hasta que haya releases
  para descargar.
- **`bench/dev-bench-mac.sh`**, que compara litek8s con kind y k3d *sobre* el
  Docker de la Mac: OrbStack es lo que mide.
- **No probado contra la API de Docker de litek8s:** `litek8s apiserver
  --runtime docker` en la Mac (el runtime Docker del node agent de Kubernetes,
  usado antes de crun) y `litek8s dev load` desde un daemon de Docker. Ambos
  solo necesitan una API de Docker; ninguno forma parte del ciclo de arriba.

La CI no se ve afectada: los runners Ubuntu de GitHub tienen Docker, y los
runners de macOS se saltan los tests que dependen de Docker.

## Problemas conocidos

- **Escrituras sin root en directorios compartidos bajo carga.** En el recurso
  compartido virtiofs de la VM de desarrollo, un directorio que cambia rápido
  puede aparecer por un momento como propiedad de root, así que un proceso sin
  root dentro de un contenedor recibe EACCES (un `cargo build -u $(id -u)` en
  paralelo lo encuentra en segundos; root no se ve afectado). Los scripts de
  build corren como root y hacen chown de su salida. La corrección corresponde
  al manejo del recurso compartido de la VM de desarrollo.
- **El disco de la VM solo crece.** Las imágenes y los datos borrados siguen
  ocupando espacio en el archivo de disco disperso (sparse) de la Mac (55 GB
  después del corpus, la referencia de k3d y los builds). `dev/stable.sh reset`
  (borra los datos del cluster, imágenes incluidas) lo devuelve.
- **Los builds del guest son largos para un cambio de una línea** (~3 min con
  4 vCPUs): el perfil de release con `opt-level=3` para el crate `litek8s`.
