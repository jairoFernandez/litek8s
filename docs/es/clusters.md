# Clusters de máquinas por SSH

Configura y mantén un cluster de máquinas reales (una Raspberry Pi, una laptop
vieja, una VM barata en la nube) desde tu laptop, sin iniciar sesión en cada
una:

- `litek8s install server|agent --ssh user@host` instala una máquina;
- `litek8s cluster apply -f cluster.yaml` instala o actualiza cada máquina de
  un archivo, y no hace nada cuando nada cambió.

Ambos usan tu propio `ssh`, así que `~/.ssh/config` (alias de Host,
`ProxyJump`, puertos, llaves), el agente SSH y `known_hosts` se aplican como
en cualquier `ssh` que escribas. Las máquinas necesitan Linux (x86_64 o
aarch64) con systemd, y root o sudo. No hace falta nada en ellas de antemano:
litek8s, crun e iptables se instalan si faltan.

## Una máquina

```bash
# a server (it runs pods too); adds the kubeconfig context and prints the join command
litek8s install server --ssh pi@192.168.1.10

# a node
litek8s install agent --ssh me@old-laptop --join https://192.168.1.10:6443 --token K10<hash>::<secret>

litek8s uninstall --ssh me@old-laptop [--purge]
```

Qué pasa, un comando `ssh` por paso, con su salida transmitida de vuelta:

1. **Sondeo**: sistema operativo y arquitectura (`uname -s -m`), si sudo pide
   contraseña, systemd, y qué litek8s (si hay alguno) está instalado.
2. **Binario**: se conserva si el `/usr/local/bin/litek8s` de la máquina ya es
   el que se quiere. Si no, en este orden: se sube `--binary PATH` (un build de
   Linux, verificado para que coincida con la arquitectura de la máquina); se
   sube este mismo litek8s cuando es un build estático de Linux de la misma
   arquitectura; si no, la máquina descarga con su propio `curl` el archivo
   del release de GitHub de esta versión y lo verifica contra los `SHA256SUMS`
   del release (`--release v0.2.0` elige otro release; `LITEK8S_DOWNLOAD_URL`
   apunta a un mirror). Un build de desarrollo (`litek8s --version` dice `DEV`)
   no tiene release, así que necesita `--binary` (`bench/build-linux.sh arm64`
   compila uno).
3. **Paquetes**: crun e iptables desde el gestor de paquetes de la máquina,
   cuando faltan y el nodo va a correr pods con crun.
4. **Instalación**: `sudo litek8s install server|agent <flags>` en la máquina,
   exactamente como lo ejecutaría [la instalación local](../multi-node.md): los
   mismos valores por defecto, chequeos y unidad de systemd. El join token va
   en un archivo 0600 (luego `/etc/litek8s/<role>.env`), nunca en una línea de
   comandos.
5. **Kubeconfig** (servidor): el contexto se agrega a tu kubeconfig
   (`$KUBECONFIG` o `~/.kube/config`) y se vuelve el actual, con la CA del
   cluster (verificada contra el join token) y `tls-server-name:
   kubernetes.default.svc`, así que a la API se llega por TLS verificado en la
   propia dirección de la máquina.

Volver a ejecutarlo con el mismo binario y los mismos flags deja el servicio
corriendo (`... already runs this binary with these flags: nothing to
change`); otros flags u otro binario reescriben la unidad y la reinician, y los
pods siguen corriendo durante el reinicio.

Los flags del lado SSH pueden ir en cualquier parte después de
`server`/`agent`; todos los demás son del servicio (`litek8s apiserver
--help`, `litek8s agent --help`):

| Flag | |
|---|---|
| `--ssh USER@HOST` | la máquina (o un Host de `~/.ssh/config`) |
| `-i FILE`, `--ssh-port N`, `--ssh-option K=V` | se pasan a `ssh` (`-i`, `-p`, `-o`) |
| `--binary PATH`, `--release TAG` | de dónde sale el binario (ver arriba) |
| `--context NAME` | servidor: el contexto del kubeconfig (por defecto: su nombre de nodo) |
| `--api-address HOST[:PORT]` | servidor: la dirección para el kubeconfig (por defecto: el host al que se conecta `ssh`, después de aplicar `~/.ssh/config`; si no, `--advertise-address`) |

**sudo con contraseña**: litek8s la pide una vez por máquina, sin eco, y se la
pasa a `sudo -S` por la conexión SSH. Sin terminal (CI, un pipe) se detiene
mostrando la línea que debes agregar a `/etc/sudoers.d/` para usar sudo sin
contraseña, en lugar de quedarse colgado.

## Un cluster file

```yaml
apiVersion: litek8s.dev/v1
kind: Cluster
metadata:
  name: home                        # the kubeconfig context
servers:                            # 1, or 3 or 5 for an HA control plane
  - ssh: pi@192.168.1.10
agents:
  - ssh: me@old-laptop
    labels: {disk: ssd}
  - ssh: ubuntu@ec2-host
    name: ec2                       # the Node's name (default: its hostname)
    nodeIp: 10.0.0.7                # the address other nodes reach it at
    taints: ["gpu=true:NoSchedule"] # or {key, value, effect}
    args: [--node-grace-period, 10m]
```

```bash
litek8s cluster apply -f cluster.yaml           # install / update, idempotent
litek8s cluster status -f cluster.yaml          # service, version, node, readiness, pods per machine
litek8s cluster apply -f cluster.yaml --prune   # also remove agents taken out of the file
litek8s cluster delete -f cluster.yaml --yes [--purge]
```

`apply`:

1. sondea cada máquina (y pide las contraseñas de sudo, una a la vez);
2. instala los servidores en orden. Si hay varios, el primero recibe
   `--cluster-init` y los demás `--server <first> --token <its token>`
   ([docs/ha.md](../ha.md));
3. agrega el contexto de kubeconfig `<metadata.name>`;
4. instala los agents, todos a la vez, con `--join <first server>` y su token;
5. define las labels y los taints de cada Node a través de la API, y anota el
   Node con el cluster, su rol y su destino SSH (`cluster.litek8s.dev/*`);
6. informa qué cambió, o `cluster home is up to date: nothing changed`.

Las labels y los taints que no definió `apply` (a mano, por el controller del
ciclo de vida de los nodos) no se tocan; los que definió y ya no están en el
archivo se eliminan. Un agent quitado del archivo se informa; con `--prune` se
hace cordon, se borran sus pods (salvo los de DaemonSets) y se reprograman en
otro lado, se desinstala litek8s de él (`--purge` también borra sus datos) y se
borra su Node. `apply` nunca quita servidores: saca uno de un control plane HA
primero con `litek8s server remove` ([docs/ha.md](../ha.md)).

Todos los campos:

| Campo | |
|---|---|
| `servers[]`, `agents[]` | `ssh` (obligatorio), `name`, `nodeIp`, `labels`, `taints`, `args`, y `sshPort`, `identityFile`, `sshOptions` para esa máquina |
| `serverArgs`, `agentArgs` | flags que recibe cada servidor / agent, antes de sus propios `args` |
| `sshPort`, `identityFile`, `sshOptions` | para todas las máquinas (`sshOptions: [StrictHostKeyChecking=accept-new]`) |
| `apiAddress` | dónde esta máquina llega a la API (`host[:port]`; por defecto: el host SSH del primer servidor) |
| `joinUrl` | dónde se unen los agents y los demás servidores (por defecto: la IP de nodo del primer servidor, `https://<ip>:6443`) |
| `binary`, `release` | el binario a instalar (como `--binary` / `--release`, que tienen prioridad sobre ellos) |
| `registries` | una configuración de registries (mirrors, CAs, credenciales; relativa al cluster file) que cada máquina recibe como `/etc/litek8s/registries.yaml` ([registries.md](../registries.md)) |

`args` no puede definir `--token`, `--join`, `--server`, `--cluster-init`,
`--node-name`, `--node-ip` ni `--registries`: `apply` los define a partir de
los campos. Los campos desconocidos son errores, y `apply` lista todos los
problemas de un archivo de una vez.

### Redes

La laptop necesita SSH hacia cada máquina y HTTPS hacia el puerto 6443 del
primer servidor (detrás de NAT o de un jump host: reenvíalo y define
`apiAddress`). Los agents necesitan el `joinUrl` del primer servidor. Los pods
en nodos distintos se comunican por el overlay WireGuard (UDP 51820 entre
nodos). La API autentica a los clientes y aplica RBAC
([Autenticación y RBAC](../auth.md)); aun así, mantén el 6443 en una red de
confianza o en una tailnet.

## Pruebas

`bench/cluster-e2e.sh` arranca tres VMs Ubuntu 24.04 (`bench/vm.sh`) y corre
el ciclo completo desde la Mac: apply (binario descargado como release desde un
servidor HTTP local), un re-apply que no reinicia nada, pods en los tres nodos
y a través del overlay, `status`, un agent quitado con `--prune`, y
`delete --yes --purge`.
