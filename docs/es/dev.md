# `litek8s dev`: un cluster de Kubernetes local para desarrollo

`litek8s dev` es un cluster de Kubernetes local para tu laptop, en el papel
que cumplen kind, k3d, minikube o el Kubernetes de Docker Desktop, sin Docker;
tantos como quieras, cada uno con tantos nodos como quieras, todos en una
pequeña VM (macOS) o un único sandbox rootless (Linux).
Es el mismo `litek8s apiserver` (API server, controladores, scheduler,
DNS del cluster, proxy de servicios) con un agente de nodo sobre crun,
empaquetado para que:

```bash
litek8s dev up          # ~0.1 s en Linux, ~1.5 s en macOS; fusiona el contexto litek8s-dev en ~/.kube/config
kubectl create deployment web --image=nginx
kubectl expose deployment web --type=NodePort --port 80
curl localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}')
litek8s dev load myapp:dev       # una imagen de tu Docker/Podman local, o un archivo de `docker save`
litek8s dev status               # modo, servidor, pods, memoria, disco
litek8s dev down                 # detener; se conservan objetos e imágenes
litek8s dev delete               # detener y borrar todo, y el contexto de kube (`reset` hace lo mismo)
litek8s dev up --name ci --nodes 3   # otro cluster: contexto litek8s-ci, 3 nodos
litek8s dev list                 # todos los clusters locales: estado, puerto de la API, nodos, memoria, antigüedad
```

Las cifras frente a kind y k3d están en [bench/RESULTS.md](](../../bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d));
la puntuación del corpus, en [compat/RESULTS.md](../../compat/RESULTS.md).

## Linux: un sandbox rootless

El cluster corre en namespaces propios, como tu usuario:

| namespace | qué aísla |
|---|---|
| user | tu uid conserva su número dentro; tu rango de `/etc/subuid` respalda todos los demás ids, root incluido, así que las capas de las imágenes conservan a sus dueños y los archivos que un contenedor crea en un bind mount como tu uid son tuyos en el host (`LITEK8S_DEV_IDMAP=root` te hace root dentro, al estilo de Docker rootless) |
| network | el bridge `litek8s0`, los veths de los pods y **todas** las reglas de iptables (NAT de pods, DNAT de Services) |
| mount | un `/run` nuevo (namespaces de red de los pods, estado de crun), sysfs, un montaje cgroup2 con raíz en el cgroup del cluster |
| PID | el PID 1 es un init pequeño; detenerlo detiene todos los procesos de los pods |
| cgroup, UTS, IPC | el árbol de cgroups del nodo, hostname `litek8s-dev` |

Los pods llegan a Internet a través de [pasta](https://passt.top) (o
slirp4netns), una pila de red en espacio de usuario del lado del host, como lo
hace Podman rootless; `LITEK8S_DEV_NETWORK=slirp4netns` elige slirp4netns donde
ambos están instalados (lo que usa [CI](../ci.md#pasta-slirp4netns-y-los-runners-de-github)). El lado del host (`litek8s dev supervise`) enlaza el puerto de la API en
`127.0.0.1` antes de que exista el sandbox, y enlaza los NodePorts y los puertos
de LoadBalancer en `127.0.0.1` cuando el cluster los pide (recibe los sockets
en escucha como descriptores de archivo), así que `localhost:<nodePort>` funciona
sin los `extraPortMappings` de kind.

**Lo que nunca toca:** las interfaces de red del host, las rutas y las reglas
de iptables/nftables (las de Docker o las de quien sea), `/run`, los archivos
de otros usuarios. Todo lo que escribe está bajo `~/.local/share/litek8s/dev`.

### Qué necesita

| necesidad | por qué | cómo |
|---|---|---|
| `crun` | ejecuta los contenedores | `apt install crun` / `dnf install crun` |
| `newuidmap`, `newgidmap` (setuid) | mapean tus ids subordinados en el user namespace | paquete `uidmap` (Debian/Ubuntu) o `shadow-utils`; normalmente ya instalados |
| un rango subuid/subgid de 65536 ids | dueños dentro de las imágenes | `/etc/subuid`, `/etc/subgid`; `useradd` los crea en las distribuciones actuales |
| `pasta` o `slirp4netns` | salida de los pods | paquete `passt` (o `slirp4netns`) |
| `ip`, `iptables-restore` | se usan solo dentro del sandbox | `iproute2`, `iptables` (backend nft o legacy) |
| cgroup v2 + una sesión de usuario de systemd | un cgroup delegado para los límites de recursos del cluster | `dev up` arranca el supervisor en un scope `systemd-run --user --scope -p Delegate=yes` |
| Linux ≥ 5.11 | overlayfs en un user namespace (`userxattr`) | |
| **solo Ubuntu ≥ 23.10:** `sudo "$(command -v litek8s)" dev setup`, una vez | AppArmor restringe los user namespaces sin privilegios; esto instala `/etc/apparmor.d/litek8s` (un perfil unconfined con `userns`, como el perfil propio de Ubuntu para crun) para la ruta del binario de litek8s | vuelve a ejecutarlo si mueves el binario |

`litek8s dev setup` (sin sudo) comprueba todo esto y dice qué falta. Como
root, además instala los paquetes que faltan con apt, dnf, yum, zypper, apk o
pacman (y el perfil de AppArmor donde haga falta); pásale a sudo la ruta
completa del binario, ya que su PATH deja fuera `~/.local/bin`. Esa es la
lista completa: sin daemon, sin pertenencia a grupos, sin capabilities en el
binario, sin litek8s setuid. Ejecutado como root, `dev up` usa el mismo
sandbox sin el user namespace (cgroup `/sys/fs/cgroup/litek8s-dev`).

## macOS: una pequeña VM sobre Virtualization.framework

`dev up` arranca una VM Linux con Virtualization.framework de Apple (el
proceso `litek8s dev-vm`; sin Docker Desktop, OrbStack, Lima ni vfkit). El
guest es el kernel `linux-virt` de Alpine y un disco raíz de solo lectura (una
imagen romfs de crun, iptables, iproute2, e2fsprogs y busybox alrededor de un
litek8s estático que corre como PID 1; un initrd de 0.75 MiB la monta bajo un
overlay tmpfs y le cede el control); un disco disperso (sparse) guarda
imágenes y objetos. El lado del Mac:

- reenvía `127.0.0.1:<port>` al API server (6443 cuando está libre);
- observa los Services y escucha en `127.0.0.1:<nodePort>` para cada
  NodePort y en `127.0.0.1:<port>` para los puertos de LoadBalancer
  (`EXTERNAL-IP` es `127.0.0.1`, como con el Kubernetes de Docker Desktop);
- comparte `$HOME` (o `--share <dir>`) por virtiofs en la **misma ruta**, así
  que los volúmenes `hostPath: /Users/you/project` funcionan y los cambios se
  ven en vivo (también `/private/tmp` y `/private/var/folders`, donde están
  `/tmp` y `$TMPDIR`);
- pasa a la VM los cambios de archivos bajo directorios montados con bind
  (FSEvents en el Mac, IN_ATTRIB en el guest), así que los watchers de inotify
  ven las ediciones hechas en el Mac;
- comparte Rosetta cuando está instalado, así que las imágenes `linux/amd64`
  corren;
- sirve la API de Docker en `~/.litek8s/docker.sock` (ver [docker.md](../docker.md));
- usa la consola como canal de control (el guest informa su dirección
  y su memoria, `dev down` lo apaga limpiamente);
- mantiene al guest con tan poca memoria como necesite, ampliándola
  según demanda ([Memoria](#memoria-en-macos)).

El framework requiere el entitlement `com.apple.security.virtualization`.
Los binarios de release vienen firmados ad hoc con él; con un binario que no
lo tiene (un `cargo build`), `dev up` ejecuta una copia firmada ad hoc
(`codesign --sign -`), así que nada necesita `sudo` ni una cuenta de
desarrollador de Apple. Tamaño de la VM: `--memory` (MiB, por defecto 4096,
un límite superior hasta el que crece el guest, ver más abajo) y `--cpus`
(por defecto, la mitad de las del Mac).

El guest (kernel, imagen raíz, initrd) viene con el archivo de release de
macOS, en `share/litek8s/vm` junto a `bin/litek8s` (install.sh lo pone en
`~/.local/share/litek8s/vm`); `dev up` busca ahí primero, así que un binario
de release siempre arranca el guest de la misma release. Desde el código
fuente, construye el guest una vez (necesita una API de Docker o cualquier
máquina Linux para el build con musl; un `litek8s dev` en ejecución sirve una,
así que litek8s puede construir su próximo guest por sí mismo, ver
[develop-without-docker.md](../develop-without-docker.md)):

```bash
dev/vm/build.sh   # litek8s estático, luego kernel + imagen raíz + initrd a partir de paquetes de Alpine
```

### Memoria en macOS

Virtualization.framework nunca le devuelve a macOS la memoria de una VM: cada
página que el guest tocó una vez sigue cargada al proceso de la VM (su
footprint, lo que muestra el Monitor de Actividad) hasta que la VM se detiene,
por mucho que el guest libere. Descartar 1.5 GB de page cache del guest no
movió el footprint en nada; su dispositivo balloon tampoco ayuda (no ofrece
free page reporting, y las páginas que se le entregan siguen cargadas:
inflarlo 3 GiB movió el footprint 0 MB, o +540 MB con el `init_on_alloc` de
Alpine), y guardar y restaurar la VM vuelve a traer toda su memoria (4.2 GB
de footprint para un guest que usaba 300 MB). Solo detener la VM devuelve
memoria; OrbStack y Docker Desktop consiguieron memoria dinámica dejando el
framework por sus propios VMM sobre Hypervisor.framework.

Lo que hace litek8s en cambio es mantener al guest lejos de la memoria que no
necesita, para que haya menos que cargar:

- **Un techo que crece según demanda.** La VM arranca con 1 GiB (un cuarto
  de `--memory` si eso es más) para el guest; el balloon toma el resto
  antes de que el guest lo toque, lo que no cuesta nada (el kernel del guest
  corre con `init_on_alloc=0`, así que no pone en cero lo que entrega).
  El guest informa su memoria por la consola cada 2 s cuando cambia
  (cada 250 ms cuando queda poca) y de inmediato cuando se dispara un trigger
  de PSI (bloqueos de memoria de 100 ms en un segundo); `litek8s dev-vm`
  aumenta el techo a la mitad más (al menos 512 MiB) cuando MemAvailable baja
  de 256 MiB (1/16 de la VM) o cuando hay bloqueos de memoria con poca memoria
  libre (los bloqueos con mucha memoria libre vienen de otra parte, por ejemplo
  un pod en su propio límite de memoria), y a la VM entera cuando está casi
  agotada.
  Se reduce de nuevo, la mitad del excedente cada vez, una vez que el guest
  lleva 2 minutos tranquilo con más del doble de eso disponible: así el page
  cache se queda dentro de la memoria que el guest ya tiene en lugar de
  extenderse por toda ella (un pull de imagen ya no deja la VM cargada con
  gigabytes), y el guest descarta primero su cache más antiguo (MGLRU); el
  cache caliente se manifiesta como bloqueos y vuelve a subir el techo.
- **Sin livelock bajo el techo.** Con el sistema de archivos raíz en un disco,
  el código que todo ejecuta es page cache recuperable; una ráfaga de
  asignaciones solía hacer que el guest lo desalojara y lo volviera a leer
  durante un minuto (el agente de nodo perdía su conexión) en lugar de quedarse
  sin memoria. El guest protege su working set del último segundo
  (`lru_gen/min_ttl_ms=1000`), así que el kernel toma de inmediato su camino de
  OOM, y ahí el balloon se desinfla (el balloon del framework puede desinflarse
  ante un OOM) antes de que se mate nada; mientras tanto, el host aumenta el
  techo.
- **Un costo fijo menor.** El sistema de archivos raíz es una imagen de disco
  que el guest lee a medida que la necesita (un initramfs se descomprime en la
  RAM y se queda ahí: 56 MiB, más el archivo de 25 MiB que el framework carga
  al arrancar), y el kernel se salta sus 64 MiB de bounce buffers
  (`swiotlb=noforce`: aquí los dispositivos virtio alcanzan toda la memoria).

`LITEK8S_DEV_DYNAMIC_MEMORY=0 litek8s dev up` le da al guest todo
`--memory` desde el inicio. El techo y lo que lo movió están en el log de la
VM (`litek8s dev-vm: memory ceiling 1024 -> 1536 MiB (little available;
...)`, `env.log` en el directorio de datos de dev), y `litek8s ui` lo muestra
con el desglose de memoria. Para devolver la memoria que la VM usó alguna vez
(después de un build grande, por ejemplo), reiníciala:
`litek8s dev down && litek8s dev up` (se conservan objetos, imágenes y
volúmenes; los pods se reinician).

Lo que macOS le carga a la VM (`--cpus 4 --memory 4096`, un cluster nuevo
cada vez, `bench/vm-memory.sh`; antes: el guest previo a estos cambios,
un initramfs y sin techo):

| | antes | ahora |
|---|---|---|
| en reposo, nada desplegado | 384 MB | 295 MB |
| en reposo, un pod | 461 MB | 389 MB |
| Argo CD instalado (7 pods) | 1847 MB | 1140 MB (el techo se quedó en 1 GiB) |
| luego un pull de una imagen de 1 GB | 3296 MB | 1200 MB |
| luego 5 minutos en reposo | 3296 MB | 1210 MB |
| luego 1 GiB asignado de golpe en un pod | 0.39 s, 4083 MB | 0.36 s, 1596 MB (techo de 1 → 4 GiB); ningún OOM kill en ninguno de los dos casos |
| 1 GiB escrito, leído y borrado en un contenedor | +1058 MB, para siempre | +733 MB, para siempre |

Después de la ráfaga de 1 GiB, el techo volvió a bajar en menos de 4 minutos
(4096 → 2471 → 1662 → 1257 → 1052 MiB) mientras un archivo de 600 MB leído
cada 10 s se mantuvo todo el tiempo en el cache del guest (125 ms por lectura,
sin refaults). Como comparación, la VM de OrbStack (su propio VMM) creció
2.3 GB para la misma escritura de 1 GiB en un contenedor y 30 s después estaba
de vuelta donde empezó: eso, el framework no lo permite. Detalles y los
experimentos detrás de esto: [bench/RESULTS.md](](../../bench/RESULTS.md#the-macos-vms-memory-a-ceiling-instead-of-a-ratchet-2026-10-19)).

### Disco en macOS

El disco de datos de la VM es un archivo disperso, `vm-state/disk.img` en el
directorio de datos de dev (64 GiB por defecto, `disk` en la configuración;
solo ocupa espacio lo que el guest escribe). A diferencia de su memoria, el
framework sí devuelve espacio en disco: el disco virtio acepta discards (TRIM)
y perfora los bloques descartados del archivo. Por eso el guest hace trim de
su sistema de archivos (FITRIM en `/var/lib/litek8s`) al arrancar y luego cada
vez que el disco lleva 30 s sin actividad, como mucho una vez por minuto; ext4
solo descarta los grupos de bloques liberados desde el último trim, así que un
trim en reposo no cuesta nada. (Montar con `discard` descartaría en cada
borrado, en medio de pulls y de la rotación de contenedores.) `docker rmi` y
`docker image prune` liberan de inmediato las capas de la imagen (las capas
escritas en los últimos 10 minutos esperan al GC de imágenes del nodo, por si
un pull todavía las necesita), y el Mac recupera el espacio en un minuto más o
menos:

| `disk.img` en el Mac (`du`) | antes | ahora |
|---|---|---|
| un contenedor escribe 1 GiB, luego `docker rm` | se queda en 2.6 GB | de vuelta en 1.3 GB 55 s después |
| `docker rmi python:3.13` (1.1 GB descomprimida) | se queda en 1.54 GB | 1.29 → 0.78 GB en menos de 3 min; 123 MB después del siguiente GC de imágenes del nodo (9 min después) |
| arranque con 362 MB borrados de un guest anterior | – | devueltos por el primer trim |

Lo que cuenta es el `du` de `disk.img`; `ls` muestra el tamaño de 64 GiB de la
imagen.

#### Más disco (o CPU, memoria) para la VM

Las imágenes, los volúmenes, las capas de los contenedores y los objetos de
todos los clusters viven en ese único disco de datos. Home en `litek8s ui`
muestra qué tan lleno está
(`disk  60.0/64.0 GiB · Mac 150 GB free · grow: 8`), y también lo hacen
`litek8s dev status` y `dev list`:

```
  disk:     60.2 of 64 GiB (the VM's data disk, a sparse image, the Mac has 140.3 GiB free); more: `disk: <GiB>` in ~/.litek8s/config.yaml or `dev up --disk <GiB>`, applied when the VM starts
```

Para darle más, define el nuevo tamaño y reinicia la VM (se detiene con el
último cluster):

```bash
litek8s dev down --all       # la VM se apaga con el último cluster
litek8s dev up --disk 128    # o `disk: 128` en ~/.litek8s/config.yaml (Settings, 8, en la UI)
```

`dev up` amplía `disk.img` al nuevo tamaño en el lugar antes de que arranque
la VM (sigue siendo disperso: el espacio libre del Mac solo baja a medida que
el guest escribe), y el guest amplía su sistema de archivos ext4 para que
coincida justo después de montarlo (`resize2fs` en línea, un segundo más o
menos; el log de la VM dice `growing the data disk's filesystem from 63.0 to
128.0 GiB`). Los clusters, objetos, imágenes y volúmenes se quedan donde
están. Un disco solo crece: un `disk` menor que la imagen se ignora con una
nota (reducirlo cortaría el sistema de archivos); para empezar con uno más
pequeño, haz `litek8s dev delete` de cada cluster, detén la VM y elimina
`vm-state/disk.img`. Con clusters todavía en ejecución, `dev up` dice que el
nuevo tamaño espera (`note: the VM the clusters share is running with other
disk; they apply once every cluster is down`), y Settings dice `Restart
needed for disk`.

Ampliar el disco de la VM no ayuda cuando al propio Mac le falta espacio: la
imagen solo puede tomar lo que el disco del Mac tiene libre, así que en ese
caso Home dice `Mac short` en lugar de `grow`; libera espacio en el Mac (o
elimina imágenes: Free space, 6, en la UI, `docker image prune`, ver arriba).
Las CPU (`cpus`, `--cpus`) y la memoria (`memory`, `--memory`, MiB) cambian
de la misma forma, en el siguiente arranque de la VM, sin la regla de un solo
sentido.

### Directorio de datos

El home de dev es `dev/` en el directorio de datos de litek8s:
`~/Library/Application Support/dev.litek8s` en macOS,
`~/.local/share/litek8s` en Linux (`$LITEK8S_DEV_HOME` sobrescribe el home de
dev). Los assets del guest construidos desde el código fuente van en `vm/`
junto a él (`$LITEK8S_DEV_VM_ASSETS` lo sobrescribe).

`LITEK8S_HOME=<dir>` pone en cambio todos los archivos por usuario de litek8s
bajo `<dir>`: el directorio de datos pasa a ser `<dir>/data` (así que la VM,
su disco y los clusters están en `<dir>/data/dev`, y el guest en
`<dir>/data/vm`), los sockets de Docker y `config.yaml` están en `<dir>/...`
en lugar de `~/.litek8s/...`, el kubeconfig es `<dir>/kubeconfig` (a menos que
`KUBECONFIG` esté definido) y los contextos son `lkdev-<cluster>`
(`$LITEK8S_CONTEXT_PREFIX`). Dos litek8s con homes distintos corren dos VM que
no saben nada la una de la otra; así es como un build desde un checkout
(`dev/lk`) corre junto a una release instalada
([develop-without-docker.md](../develop-without-docker.md#usar-litek8s-vs-desarrollarlo)).

Los builds anteriores al cambio de nombre usaban
`~/Library/Application Support/dev.civojairo.litek8s` en macOS. litek8s sigue
usando ese directorio mientras exista y `dev.litek8s` no, así que una VM
existente, su disco y sus clusters nunca se mueven a tus espaldas. Para pasar
al nuevo nombre:

```bash
litek8s dev list            # anota los clusters que están arriba
litek8s dev down --all      # todos los clusters; la VM se apaga con el último
pgrep -fl 'litek8s dev-vm'  # no queda nada
mv ~/Library/Application\ Support/dev.civojairo.litek8s ~/Library/Application\ Support/dev.litek8s
litek8s dev up              # los mismos clusters, objetos e imágenes
```

## Configuración

`dev up` toma sus valores por defecto de `~/.litek8s/config.yaml`
(`$LITEK8S_CONFIG`), que edita `litek8s ui` ([ui.md](../ui.md)); los flags
tienen prioridad sobre él:

| ajuste | flag | macOS | Linux |
|---|---|---|---|
| `cpus` | `--cpus` | las CPU de la VM (por defecto: la mitad de las del Mac, como máximo 8) | un límite de CPU para el sandbox (por defecto: ninguno) |
| `memory` (MiB) | `--memory` | el techo de la VM (por defecto 4096) | un límite de memoria para el sandbox (por defecto: ninguno) |
| `disk` (GiB) | `--disk` | el disco de datos de la VM (por defecto 64, de 16 a 4096): un tamaño mayor lo amplía en el lugar en el siguiente arranque de la VM, conservando los datos; nunca se reduce ([Más disco](#más-disco-o-cpu-memoria-para-la-vm)) | - (el sandbox usa el disco del host) |
| `shares` | `--share` (repetible) | directorios compartidos en la misma ruta (por defecto `$HOME`) | - (el sandbox ve los archivos del host) |
| `port` | `--port` | puerto del API server del cluster por defecto (por defecto 6443, uno libre si está ocupado; los otros clusters conservan el que obtuvieron) | igual |
| `publish` | `--publish` | `localhost` (por defecto) o `all`: dónde escuchan los NodePorts, los puertos de LoadBalancer y los puertos de `docker run -p` | igual |

El archivo se lee en el `up`; `env.json` registra con qué arrancó la VM o el
sandbox, y el `dev.json` de cada cluster, su propio puerto y sus nodos.
`cpus`, `memory`, `disk`, `shares` y `publish` pertenecen a la VM o al sandbox
que comparten todos los clusters: se aplican cuando el primer cluster lo
arranca.

## Imágenes

- `litek8s dev load <image>` exporta la imagen desde el Docker local (o desde
  la API de Docker de Podman, vía `DOCKER_HOST`) y la importa; `litek8s dev load
  file.tar` acepta archivos de `docker save` / `podman save` en formato Docker u
  OCI, incluidos los archivos multiplataforma de Docker 25+ (solo se usa la
  plataforma de este host). A las imágenes cargadas nunca se les hace pull
  encima, sea cual sea la pull policy, así que `myapp:latest` funciona sin
  `imagePullPolicy: IfNotPresent`.
- Un registry en el `localhost` de tu máquina funciona tal cual:
  `image: localhost:5000/myapp:1` alcanza el puerto 5000 del host desde dentro
  del sandbox (pasta mapea el gateway al loopback del host) o de la VM (el
  gateway NAT del framework; el registry debe escuchar en todas las
  interfaces, cosa que hace `docker run -p 5000:5000 registry:2`).
  Otros registries por HTTP plano: `LITEK8S_INSECURE_REGISTRIES=host:port`.
- Registries privados (una CA propia, credenciales, HTTP plano) y
  mirrors: `~/.litek8s/registries.yaml`, en el formato de k3s, que `dev up` les
  pasa a los nodos del cluster en cada arranque ([registries.md](../registries.md)).

## Varios clusters

kind y k3d le dan a cada cluster su propio contenedor de nodo (un systemd o un
k3s completo con su propio containerd, ~400-600 MB cada uno); `litek8s dev` le
da unos pocos procesos en la VM o el sandbox que ya está en ejecución:

```bash
litek8s dev up --name ci                 # contexto litek8s-ci, su propio puerto de la API, datos y socket de Docker
litek8s dev up --name multi --nodes 3    # 3 nodos: litek8s-multi, -worker, -worker2
litek8s dev list
NAME         STATE    API               NODES  MEMORY     AGE   CONTEXT
dev          running  127.0.0.1:6443    1      4.2 MiB    7m    litek8s-dev
ci           running  127.0.0.1:36097   1      3.9 MiB    1m    litek8s-ci
multi        running  127.0.0.1:41011   3      17.7 MiB   0s    litek8s-multi

shared sandbox (rootless, 62.3 MiB in use)
litek8s dev down --name ci               # detiene uno; la VM/el sandbox se detiene con el último
litek8s dev delete --name ci             # y borra sus datos y su contexto
litek8s dev down --all
```

`dev up|down|status|delete|reset|load` aceptan `--name` (por defecto `dev`,
que conserva las rutas, el puerto, el contexto y el socket que litek8s siempre
usó, así que un cluster existente no se migra). Lo que obtiene cada cluster:

| | por defecto (`dev`) | `--name <n>` |
|---|---|---|
| contexto de kubectl | `litek8s-dev` | `litek8s-<n>` (`--no-switch` lo agrega sin hacerlo el actual) |
| puerto de la API | 6443, uno libre si está ocupado | el que tuvo la última vez, si no uno libre |
| socket de Docker | `~/.litek8s/docker.sock` | `~/.litek8s/<n>/docker.sock` (contexto de docker `litek8s-<n>`) |
| datos (Linux) | `~/.local/share/litek8s/dev/cluster` | `~/.local/share/litek8s/dev/clusters/<n>/data` |
| datos (macOS) | `/var/lib/litek8s/dev` en el disco de la VM | `/var/lib/litek8s/<n>` en el disco de la VM |
| log | `dev.log` en el directorio de datos de dev | `clusters/<n>/dev.log` ahí mismo (el propio de la VM o del sandbox: `env.log`) |
| nombres de nodos | `litek8s-dev` | `litek8s-<n>`, `litek8s-<n>-worker`, `-worker2`, ... |
| NodePorts desde | 30000 | 30100-32700, según el nombre (así dos clusters rara vez quieren el mismo puerto de `localhost`) |

**Cómo.** El primer `dev up` arranca la VM (macOS) o el sandbox (Linux);
cada cluster es luego una *celda* dentro de él, creada por el mismo código que
crea el sandbox de Linux: su propio network namespace (su bridge de pods
`litek8s0`, reglas de iptables, DNS, puertos del kubelet y de NodePorts: todos
los clusters usan los habituales 10.42.0.0/16 para pods y 10.43.0.0/16 para
Services sin conflicto), mount namespace (`/run`, estado de crun), PID
namespace (detenerlo detiene todos sus procesos), cgroup namespace
(`/cells/<cluster>/<node>`: la memoria de un cluster es la de ese directorio),
y namespaces UTS e IPC. Las celdas se alcanzan entre sí y salen al exterior a
través de un bridge en la VM o el sandbox (`lkcells0`, 10.231.0.0/24, con
masquerade). Un pequeño gestor dentro (`litek8s dev __cells`) arranca y
detiene las celdas; el proceso de la VM o el supervisor de fuera enlaza en el
host el puerto de la API y el socket de Docker de cada cluster (Linux:
entregados a la celda como descriptores, así que las conexiones no se copian;
macOS: retransmitidos por vsock al network namespace de la celda) y reenvía
sus NodePorts. Cuando se detiene el último cluster, la VM o el sandbox
también se va.

**Varios nodos.** `--nodes N` (1-10, se recuerda) ejecuta N-1 workers, cada
uno en una celda propia, unidos con el flujo de join real (`litek8s agent --join
https://<control plane>:6443 --token ...`, ver
[multi-node](../../compat/cases/M1-multinode.sh)) y conectados por el mismo
overlay de pods WireGuard que usan los nodos reales: prueba en tu laptop la
distribución de pods, la afinidad, los DaemonSets, el drenado o la caída de
un nodo:

```bash
litek8s dev down --name multi --node litek8s-multi-worker2   # sus pods mueren; NotReady tras el periodo de gracia (40 s)
litek8s dev up --name multi --node litek8s-multi-worker2     # de vuelta, Ready en cuanto se reconecta
```

En Linux el overlay necesita el módulo de kernel `wireguard` cargado
(`lsmod | grep wireguard`; un sandbox rootless no puede cargarlo; la mayoría
de las distribuciones lo cargan en el primer uso por parte de root, por
ejemplo `sudo modprobe wireguard` una vez).

**Aislamiento, con honestidad.** Las celdas aíslan nombres, redes, archivos
bajo `/run`, procesos y cgroups, como lo hacen los contenedores; no aíslan el
kernel. Todos los clusters comparten:

- las CPU y la memoria de la VM (macOS) o los límites del sandbox (Linux,
  `cpus`/`memory` en la configuración): se definen para todos ellos, y un
  cluster ocupado ralentiza a los demás (todavía no hay límites por cluster);
- el disco de la VM: una sola imagen dispersa para todos los clusters; lo que
  borran vuelve al Mac en un minuto más o menos (ver [Disco en macOS](#disco-en-macos));
- en Linux, un único user namespace: los mismos ids subordinados se mapean a
  los mismos ids del host en todos los clusters;
- los puertos del host: los NodePorts, los puertos de LoadBalancer y los
  puertos de `docker run -p` de todos los clusters escuchan en el mismo
  `localhost`; cuando dos clusters quieren un mismo puerto, el primero lo
  obtiene y el log del otro dice "not exposed";
- el almacén de imágenes es por cluster: una imagen a la que se le hizo pull o
  `dev load` en uno no está en los demás (`dev load --name <n>`).

Un cluster borrado mientras la VM está apagada tiene sus datos borrados en el
siguiente arranque de la VM.

### Memoria: 1, 3 y 5 clusters

Cada cluster con un nodo, en reposo 60 s después de `dev up` (o de `kind create
cluster`, `k3d cluster create`), y luego con un deployment de `nginx:alpine` de
3 réplicas en cada uno (60 s después del rollout). La memoria de un cluster se
cuenta como `docker stats` cuenta la de un contenedor: el uso de su cgroup
menos el page cache que el kernel descartaría primero. Medido el 2026-10-19
con el build de release.

**macOS** (MacBook serie M, 14 núcleos, 36 GB; VM de litek8s con `--cpus 4
--memory 4096`; kind v0.33.0 y k3d v5.8.3 con sus imágenes por defecto sobre
OrbStack, tal como se instalan):

| clusters | litek8s: VM completa¹ | litek8s: los clusters² | kind: contenedores de nodo³ | k3d: contenedores de servidor + LB³ |
|---|---|---|---|---|
| 1, en reposo | 375 MiB | 2.4 MiB | 564 MiB | 614 MiB |
| 3, en reposo | 378 MiB | 6.1 MiB | 1574 MiB | 1707 MiB |
| 5, en reposo | 386 MiB | 9.2 MiB | 2857 MiB | 2818 MiB |
| 1, 3 pods cada uno | 452 MiB | 28 MiB | 671 MiB | 649 MiB |
| 3, 3 pods cada uno | 697 MiB | 82 MiB | 2033 MiB | 1925 MiB |
| 5, 3 pods cada uno | 898 MiB | 135 MiB | 3080 MiB | 3208 MiB |

¹ El footprint físico de la VM tal como lo cuenta macOS (lo que muestra el
Monitor de Actividad para ella): el kernel del guest, el initramfs (el binario
de litek8s, crun, iptables) retenido en RAM, el page cache y todos los
clusters. ² La suma de las celdas de los clusters. ³ `docker stats`, sin la
propia VM de OrbStack, que las columnas de kind y k3d no incluyen (corre de
todos modos para Docker). Crear los clusters tomó 9/32/54 s con kind,
17/50/83 s con k3d, y 0.1-0.2 s cada uno con litek8s una vez que la VM estaba
arriba (su arranque: unos 1.5-2.5 s, una sola vez).

**Linux** (omarchy: x86_64, 16 hilos, 32 GB, kernel 7.2, rootless; kind
y k3d no están instalados ahí, así que solo litek8s):

| clusters | litek8s: sandbox completo⁴ | litek8s: los clusters² |
|---|---|---|
| 1, en reposo | 38 MiB | 4 MiB |
| 3, en reposo | 52 MiB | 13 MiB |
| 5, en reposo | 63 MiB | 20 MiB |
| 1, 3 pods cada uno | 91 MiB | 57 MiB |
| 3, 3 pods cada uno | 205 MiB | 166 MiB |
| 5, 3 pods cada uno | 317 MiB | 273 MiB |

⁴ El cgroup del sandbox: el supervisor, pasta, el gestor de celdas y todos los
clusters (y, en Linux, el page cache del binario, cargado a quien lo tocó
primero).

Lo que significa: un cluster en reposo cuesta 2-4 MiB más allá del primero y un
cluster de kind o k3d unos 550 MiB, así que cinco clusters de litek8s en reposo
caben en la memoria de un nodo de kind y sobra espacio. Con pods, dominan los
pods: nginx arranca un worker por cada CPU que ve la celda (4 en la VM, 16 en
la máquina Linux). El footprint de la VM crece más rápido que la suma de sus
clusters con workloads: cada cluster hace pull de sus propias imágenes y las
descomprime (el almacén de imágenes es por cluster), y el page cache del guest
se queda en el footprint hasta que macOS lo pide de vuelta.

## Límites

- Sin autenticación en la API (`--authorization-mode AlwaysAllow`: escucha
  solo en 127.0.0.1; dentro de la VM o del sandbox, en la celda del cluster, y
  en los clusters multinodo también en la dirección de su celda, que solo
  alcanzan las otras celdas). Los objetos de RBAC se almacenan, no se aplican;
  los servidores instalados con `litek8s install` o `cluster apply` sí los
  aplican.
- Los hostPorts no se exponen en el localhost del host (los NodePorts sí);
  rootless no puede enlazar puertos del host por debajo de 1024.
- macOS: el primer arranque formatea el disco de datos (~5 s); el DHCP del NAT
  del framework a veces tarda 2–3 s cuando ninguna otra VM ha corrido en un
  tiempo.
