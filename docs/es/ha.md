# Alta disponibilidad: varios API servers

`litek8s apiserver` puede correr como un cluster de 3 (o 5) servidores, como
k3s con etcd embebido: cualquiera de los servidores puede caerse sin perder
escrituras ni disponibilidad. Las mediciones están en
[bench/HA.md](../../bench/HA.md).

## Ponerlo en marcha

En máquinas Linux con systemd, `install.sh` (o `litek8s install`, como root)
configura cada servidor como el servicio `litek8s-server` e imprime los
comandos para los demás:

```bash
INSTALL=https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh

# The first server starts the cluster (a single server's objects move into it).
# Servers reach each other on the API port and the Raft port (API port + 1).
curl -fsSL $INSTALL | sh -s -- server --cluster-init

# Two more join it with the token the first prints (also in
# /var/lib/litek8s/apiserver/join-token).
curl -fsSL $INSTALL | sh -s -- server --server https://192.168.1.10:6443 --token K10...::...

# Agents join any server and learn the others.
curl -fsSL $INSTALL | sh -s -- agent --join https://192.168.1.10:6443 --token K10...::...

litek8s server list --server http://192.168.1.10:6443
litek8s server remove lk3 --server https://192.168.1.10:6443 --token K10...::...
```

Desde una laptop, lo mismo por SSH con un solo archivo
([docs/clusters.md](../clusters.md)): con tres `servers`, `cluster apply`
arranca el primero con `--cluster-init` y une los otros dos a él.

```yaml
apiVersion: litek8s.dev/v1
kind: Cluster
metadata: {name: home}
servers: [{ssh: pi@192.168.1.10}, {ssh: pi@192.168.1.11}, {ssh: pi@192.168.1.12}]
agents: [{ssh: me@old-laptop}]
```

`litek8s install server` completa `--listen 0.0.0.0:6443 --data-dir
/var/lib/litek8s/apiserver --node-dir /var/lib/litek8s/node-agent --runtime
crun --kubeconfig /etc/litek8s/kubeconfig.yaml` y el `--node-ip` detectado
(pasa `--node-ip` en máquinas con varias direcciones), guarda el token en
`/etc/litek8s/server.env` (0600) en lugar de en la línea de comandos,
comprueba que los puertos 6443/6444 estén libres y espera a `/readyz`. Sin
systemd, ejecuta lo mismo a mano:

```bash
litek8s apiserver --data-dir /var/lib/litek8s/apiserver --listen 0.0.0.0:6443 \
  --node-ip 192.168.1.10 --cluster-init
LITEK8S_TOKEN=K10...::... litek8s apiserver --data-dir /var/lib/litek8s/apiserver --listen 0.0.0.0:6443 \
  --node-ip 192.168.1.11 --server https://192.168.1.10:6443
```

Los flags solo hacen falta la primera vez: un directorio de datos que
pertenece a un cluster reinicia como ese miembro (en ese caso `--cluster-init`
y `--server` se ignoran). Quita un servidor (`litek8s server remove`) antes de
reinstalarlo con un directorio de datos vacío; un servidor que vuelve con un
directorio de datos nuevo bajo el mismo nombre o la misma dirección reemplaza
su identidad anterior.

**Una sola dirección para los clientes.** kubectl acepta un servidor por
cluster. Corre el balanceador de carga integrado donde están los clientes (una
laptop, o cada agent):

```bash
litek8s lb --listen 127.0.0.1:6443 --server http://192.168.1.10:6443
kubectl --server http://127.0.0.1:6443 get nodes
```

Aprende todos los servidores a partir del que se le da, revisa el `/readyz` de
cada uno cada segundo y pasa conexiones TCP completas (HTTP o TLS) a uno que
esté listo, quedándose con el mismo servidor mientras siga listo.
(`--https-check` revisa `/readyz` por TLS en su lugar, para kube-apiservers
que permiten `/readyz` anónimo; `--tcp-check` solo se conecta.) Una IP virtual
(keepalived) o un nombre DNS con la dirección de cada servidor también
funcionan: cada servidor sirve la API completa, y `/readyz` indica si se puede
usar.

Los agents no necesitan nada de eso: reciben la lista de servidores al unirse
y en cada conexión, la guardan en `<node-dir>/servers.json`, sondean el
servidor en uso cada 2 s y pasan al siguiente cuando no se puede alcanzar o no
está listo. Los pods llegan a la API por el Service `kubernetes`, cuyos
endpoints son todos los servidores vivos (cada uno renueva un Lease
`kube-system/litek8s-apiserver-<name>`; el líder publica los endpoints a
partir de los que están al día, como hace el lease endpoint reconciler de
kube-apiserver).

## Cómo funciona

```
 client ─┐                       ┌──────── leader ───────┐
 kubectl │  write  ┌─ follower ─┐│ HTTP handler          │
         └───────► │ forward ───┼┼► admission, defaults  │
                   │            ││   store.update ──► Raft log ──► followers
                   │ wait until ││                       │          (append,
                   │ applied ◄──┼┼── answer + log index  │           fsync)
                   └────────────┘└───────────────────────┘
```

- **Store replicado.** El almacén de objetos (`src/kube/store.rs`) no cambia
  en modo de un solo servidor (WAL en disco, mismo rendimiento y formato). En
  modo HA cada mutación se convierte en una [`Mutation`] autocontenida
  (create / update / delete, el objeto, la precondición de resourceVersion)
  que se agrega al log de Raft (openraft). La máquina de estados de cada
  servidor aplica el log a su store en memoria, comprobando ahí la
  precondición, así que cada servidor toma la misma decisión, guarda los
  mismos objetos con los mismos resourceVersions y registra el mismo historial
  de watch. La semántica de list, get y watch, el formato de resourceVersion y
  el comportamiento 410/reanudación son los mismos que en un solo servidor.
- **resourceVersion** sigue siendo el contador de mutaciones aplicadas del
  store, que es una función determinista del log: monótono en todo el cluster,
  y los watches se reanudan desde cualquier servidor con el mismo número.
  (Usar en su lugar el índice del log de Raft habría dejado huecos por
  precondiciones fallidas, entradas en blanco y de membresía, y habría
  cambiado la numeración de un solo servidor.)
- **Log y snapshots.** El log está en disco en archivos de segmento con
  verificación CRC; un hilo de flush lo agrega y lo hace durable, sincronizando
  con un solo `fdatasync` todos los appends que se encolaron mientras tanto
  (group commit); nada se confirma antes de ser durable en una mayoría. Cada
  20 000 entradas un snapshot (el formato de registro del WAL con una
  cabecera) reemplaza el log anterior a él (se conservan 2 000 entradas para
  que un follower ausente por poco tiempo se ponga al día desde el log). Al
  arrancar, un servidor carga su snapshot más reciente y reproduce el log.
  `--cluster-init` sobre un directorio de datos con el `objects.wal` de un
  solo servidor mueve esos objetos al primer snapshot.
- **Escrituras en un follower.** Las peticiones de escritura de los clientes
  (POST/PUT/PATCH/DELETE) se reenvían completas al líder, para que los
  asignadores en memoria (ClusterIPs, NodePorts, CIDRs de pods) y la admisión
  corran en un solo lugar. Las escrituras que hace el propio follower (estado
  de nodos de sus agents, leases) se reenvían como propuestas. En ambos casos
  el follower responde solo cuando ya aplicó la entrada (el líder devuelve su
  índice aplicado), así que un cliente que escribe y luego lee a través del
  mismo servidor ve su escritura. Las propuestas llevan un id (session, seq) y
  la máquina de estados recuerda los resultados hasta que el proponente los
  confirma, así que una propuesta reintentada después de que el líder murió
  entre el commit y la respuesta se aplica una sola vez.
- **Lecturas.** Gets, lists y watches se sirven desde el store local. Con
  `resourceVersion=""` (el valor por defecto, "el estado más reciente") pasan
  antes por una barrera de lectura: el líder confirma con una ronda de
  heartbeats que sigue siendo líder e informa su índice de commit (ReadIndex),
  y el servidor espera hasta haber aplicado hasta ahí. Esa es la lectura
  linealizable que kube-apiserver obtiene de etcd
  (`--consistent-list-from-cache` hace lo mismo: lee la revisión por quórum y
  luego sirve desde la caché). Los lectores concurrentes comparten una ronda de
  heartbeats. `resourceVersion="0"` lee lo que tenga el servidor (sin
  barrera), y `resourceVersion=N` espera hasta 3 s a que el servidor llegue a
  N (si no, `504 Too large resource version`, como kube-apiserver). Los
  handlers de escritura leen primero el objeto actual (para responder 404,
  fusionar un patch), así que una escritura que atiende un servidor también
  pasa por la barrera: sin ella, un líder depuesto aislado de los demás
  respondía 404 desde un estado desactualizado (el chequeo de
  linealizabilidad lo encontró).
- **Singletons.** Los controllers, el scheduler, el ciclo de vida de los
  nodos, la expiración de Events, los objetos de bootstrap y el reconciliador
  de endpoints de la API corren solo en el líder; se inician después de que
  confirmó su término y aplicó todo lo anterior, y se detienen cuando pierde
  el liderazgo. Sus escrituras nunca se reenvían: si el servidor ya no es el
  líder fallan, así que los controllers de un líder depuesto no pueden actuar
  sobre un estado desactualizado. Todos los servidores corren la parte de
  descubrimiento de la agregación de APIs (cada uno necesita los tipos
  agregados en su registro); solo el líder escribe el status de los
  APIService.
- **Streams.** `logs`, `exec`, `attach`, `port-forward` y el proxy de pods se
  retransmiten al servidor al que está conectado el nodo del pod (el Lease del
  nodo lo nombra); a los webhooks y API servers agregados en esos nodos se
  llega a través de un túnel entre servidores hacia la conexión del agent de
  ese nodo.
- **Transporte.** Los servidores hablan HTTPS anclado a la CA del cluster y
  autenticado con el secreto de unión (el token): Raft (RPCs, propuestas,
  read indexes) en un listener propio, por defecto el puerto de la API + 1
  (`--raft-port`), y las peticiones reenviadas en el puerto de la API. Raft,
  sus conexiones y su listener corren en dos hilos propios, así que una
  ráfaga de trabajo de la API en un servidor pequeño nunca retrasa los
  heartbeats; una petición de Raft que openraft dejó de esperar igual termina
  en segundo plano, de modo que su conexión se reutiliza en lugar de volver a
  abrirse. Un servidor que se une obtiene la CA (verificada contra el hash del
  token), el certificado de servicio y la clave de service account del
  servidor al que se une. Como en k3s por defecto, el token de agent es el
  token de servidor: quien lo tenga puede unir cualquiera de los dos.
- **Temporizadores.** Heartbeats cada 300 ms; un follower convoca una elección
  después del lease del líder (3 s) más un tiempo aleatorio de 1.5–3 s sin
  recibir uno, así que un líder muerto se reemplaza en unos 5 s (bench/HA.md:
  4.7–5.9 s sin poder escribir en la mayoría de las caídas del líder, hasta
  15 s cuando los flushes del log eran lentos). openraft 0.9 además le da a
  cada AppendEntries un intervalo de heartbeat para completarse (las
  peticiones tienen un tope de 512 KiB, así que los objetos grandes van de a
  unos pocos). En discos rápidos dedicados y una red cableada,
  `LITEK8S_RAFT_HEARTBEAT_MS=100` en cada servidor da failovers de ~2 s (ver
  Discos más abajo para saber por qué no es el valor por defecto).

## Modelo de consistencia

| Petición | La atiende | Garantía |
|---|---|---|
| create / update / patch / delete | el líder (reenviada) | linealizable; un 2xx significa que hizo commit en una mayoría |
| get / list / watch, `resourceVersion=""` | cualquier servidor, después de la barrera de lectura | linealizable |
| get / list, `resourceVersion="0"` | cualquier servidor | puede estar desactualizada (como la watch cache de kube-apiserver) |
| get / list / watch, `resourceVersion=N` | cualquier servidor, una vez que llegó a N | no más antigua que N |
| el mismo cliente, escribe y luego lee a través de un servidor | ese servidor | read-your-writes |

Una petición que falla con `504 Timeout` (sin líder durante 15 s, o el
servidor quedó aislado) puede haberse aplicado o no, como con los timeouts de
etcd.

## Discos

Cada servidor escribe el log de Raft con `fdatasync` antes de confirmar una
entrada, como etcd, y el núcleo de openraft 0.9 espera ese flush antes de
atender el siguiente mensaje, heartbeats incluidos. Un follower cuyo disco
tarda segundos en hacer flush (una tarjeta SD, o un disco compartido con pulls
de imágenes que escriben gigabytes) no puede responder heartbeats mientras
tanto, y los demás eligen un nuevo líder. Para que los flushes sean cortos, el
log está preasignado (segmentos de 16 MiB escritos llenos de ceros, como el WAL
de etcd), así que un append es una sobrescritura sin metadatos que el sistema
de archivos tenga que registrar en su journal: en ext4 un commit del journal
esperaría a todos los demás archivos sucios del disco. Por eso los
temporizadores por defecto toleran ~4.5 s sin heartbeat; con discos rápidos
dedicados se pueden acortar (`LITEK8S_RAFT_HEARTBEAT_MS=100`); bench/HA.md
mide ambos casos.

## Límites

- Un servidor aislado de la mayoría sigue sirviendo lecturas con
  `resourceVersion="0"` y watches activos desde su último estado; su `/readyz`
  pasa a false en menos de 3 s para que los balanceadores de carga y los
  agents se alejen.
- La membresía cambia de a un servidor a la vez; 3 o 5 votantes. Los learners
  (servidores sin voto) solo se usan mientras un servidor se une.
- Los clientes de la API se autentican y RBAC se aplica como en un solo
  servidor; una petición que otro servidor reenvía al líder lleva el usuario
  que autenticó (confiable gracias al secreto del cluster), así que los
  certificados de cliente funcionan contra cualquier servidor. Todos los
  servidores derivan la misma CA de clientes a partir de la clave de service
  account compartida.
