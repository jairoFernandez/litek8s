---
title: "Un home lab a través de NATs: un cluster con un PC de casa, una VM en la nube y la máquina de un amigo"
description: "Tres máquinas detrás de NATs distintos, un solo cluster litek8s. Cómo el overlay WireGuard busca caminos directos, cuándo usa un relay y cuánto cuesta."
date: 2026-10-30
tags: [home lab, red, wireguard, nat, multi-nodo]
docs: [networking, clusters, multi-node, observability]
lang: es
translation: home-lab-across-nats
---

Las máquinas que uno tiene de verdad casi nunca están en la misma red. Un PC
de escritorio en la casa, detrás del router del proveedor de internet. Una VM
barata en la nube cuya dirección propia es privada, con una pública mapeada
encima. Un amigo que te ofreció el equipo que tiene sin usar debajo del
escritorio, detrás de otro router, en otra ciudad.

La mayoría de las guías resuelven eso con "primero monta una VPN delante de
todo". Nosotros queríamos que litek8s armara ese único cluster por su cuenta:
que cada pod llegue a cualquier otro pod, que los Services funcionen entre las
tres máquinas y que nadie tenga que tocar un router. Este post cuenta cómo lo
hace, qué medimos y el caso que no puede resolver.

## El montaje

| máquina | dónde | rol |
|---|---|---|
| `cloud` | una VM barata en la nube, dirección pública con NAT 1:1 sobre una privada | servidor, relay |
| `home-a` | un PC con Linux en la casa, detrás del router | nodo |
| `home-b` | la máquina Linux de un amigo, detrás de su router | nodo |

Las tres corren Linux (x86_64 o aarch64) con systemd: servidores y nodos
necesitan Linux, macOS es para los clusters de desarrollo. No hace falta
instalarles nada antes: litek8s, crun e iptables se instalan si faltan.

## Armarlo

Desde un portátil en la casa, que llega por SSH a la VM y al PC, un archivo
cubre las dos máquinas que controlamos:

```yaml
apiVersion: litek8s.dev/v1
kind: Cluster
metadata:
  name: home                        # el contexto del kubeconfig
joinUrl: https://203.0.113.7:6443   # la dirección pública de la VM, no la privada
servers:
  - ssh: ubuntu@203.0.113.7
    name: cloud
    args: [--node-external-ip, 203.0.113.7]
agents:
  - ssh: me@desktop
    name: home-a
```

```bash
litek8s cluster apply -f cluster.yaml           # install / update, idempotent
litek8s cluster status -f cluster.yaml          # service, version, node, readiness, pods per machine
```

Hay dos detalles importantes. La VM solo conoce su dirección privada, así que
`--node-external-ip` le da la pública: esa pasa a ser el endpoint de WireGuard
que anuncia el nodo y el `ExternalIP` del Node. Y `joinUrl` reemplaza el valor
por defecto (la IP de nodo del primer servidor), que sería justamente esa
dirección privada. En el security group de la VM hay que abrir UDP 51820 para
WireGuard y TCP 6443 para la API.

El amigo no tiene que darnos SSH. Corre el comando de unión que imprimió el
servidor, que se ve así:

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh -s -- \
  agent --join https://203.0.113.7:6443 --token K10<hash>::<secret>
```

Eso instala el binario, revisa la máquina (cgroup v2, WireGuard en el kernel,
puertos libres) y arranca una unidad systemd `litek8s-agent`.

## Cómo se llega a un nodo detrás de NAT

Cada nodo recibe un `/24` de `10.42.0.0/16` y una interfaz `litek8s-wg`, con
todos los demás nodos como peers de WireGuard. Las direcciones de los pods
nunca se enmascaran entre nodos, así que un pod ve la dirección real del pod
que le habla. Para lidiar con el NAT hay tres mecanismos:

1. **Keepalive.** Cada peer tiene un keepalive de 25 s. Un nodo al que nadie
   puede llegar igual funciona: envía primero, y el WireGuard del otro lado
   aprende de dónde vienen sus paquetes. Así es como `cloud` llega a las dos
   casas.
2. **Endpoints que ven los demás.** La dirección desde la que llegan los
   paquetes de un nodo, vista por los otros nodos, se ofrece como endpoint
   extra para probar. Una casa cuyo router reenvía el puerto de WireGuard, o
   lo mapea igual para todos los destinos, queda alcanzable directamente.
3. **Un relay.** Un par sin ningún camino directo pasa por un nodo al que
   ambos llegan. Por defecto es el servidor: aquí, `cloud`.

Para saber qué pares necesitan el relay, cada agente le manda cada 10 s un
datagrama UDP pequeño, por WireGuard, a cada peer que enruta directo.
WireGuard responde un paquete de datos con un keepalive, así que un camino que
funciona lleva tráfico en los dos sentidos. El agente clasifica cada peer:

| estado | significado |
|---|---|
| `direct` | llegó algo en los últimos 40 s por una sesión de menos de 180 s |
| `unreachable` | observado durante 40 s sin que pase eso |
| `unknown` | observado menos de 40 s; se mantiene el último estado conocido |

El servidor guarda esos reportes en los objetos Node y calcula las rutas. Un
par se relaya solo cuando un lado reporta al otro como `unreachable`, así que
los pares que funcionan directo nunca pasan por el relay. Mientras está
relayado, la entrada directa sigue configurada sin allowed IPs y sus
keepalives siguen probando el camino; cuando ningún lado reporta
`unreachable`, el par vuelve a directo.

## Ver qué pasó

```bash
kubectl get node home-a -o jsonpath='{.metadata.annotations.litek8s\.dev/wireguard-routes}'
# {"cloud":"direct","home-b":"via cloud"}
litek8s tree
# ├─ nodes
# │  ├─ Node cloud   Ready, 3 pods, pods 10.42.0.0/24
# │  ├─ Node home-a  Ready, 2 pods, pods 10.42.1.0/24, overlay: home-b via cloud
```

Las dos casas llegan directo a `cloud`; entre ellas pasan por ahí. Las
métricas incluidas dicen cuánto cuesta eso:

```bash
litek8s query 'rate(litek8s_overlay_relayed_bytes_total[5m])'
litek8s query 'litek8s_overlay_peer_relayed == 1'
```

## Lo que cuesta el relay

El camino de los datos se queda en el kernel. En `home-a`, el peer `cloud`
recibe el pod CIDR de `home-b` en sus allowed IPs; `cloud` descifra el
paquete, su tabla de rutas lo devuelve a `litek8s-wg` y WireGuard lo cifra
hacia `home-b`. Sin proxy en espacio de usuario ni demonios extra.

Lo que se paga:

- **El ancho de banda del relay.** Cada byte relayado entra y sale de
  `cloud`. En una VM en la nube, eso es su factura de egress.
  `litek8s_overlay_relayed_bytes_total` dice cuántos bytes reenvió para otros.
- **Un salto más.** El tráfico entre las casas va hasta la VM y vuelve, sin
  importar qué tan cerca estén entre sí.
- **Tiempo para darse cuenta.** Un camino que se rompe pasa al relay después
  de más o menos un minuto (40 s sin respuestas, luego el siguiente reporte y
  el push). Un camino que vuelve se usa otra vez cuando un handshake logra
  pasar por él, lo que puede tardar hasta el siguiente rekey de WireGuard,
  de 2 a 3 minutos.

El relay no es un reenviador abierto. Solo reenvía entre peers de WireGuard y
solo entre direcciones de pods (cadena de filtro `LITEK8S-OVERLAY`), y los
peers son los propios nodos del cluster, que se unieron con el token.

## Medido, no supuesto

`bench/nat-e2e.sh` arma exactamente esta forma: tres VMs, un servidor con
dirección pública y dos casas, cada una con su agente en un network namespace
detrás de su propio NAT (las dos casas en `10.99.0.0/24`), sin forma de
llegar una a la otra. Verifica tráfico pod a pod y Services en los dos
sentidos a través del relay; después agrega un reenvío de puerto en uno de
los routers, corta el camino y lo restaura.

Última corrida (2026-10-21, qemu + KVM en un host Linux, invitados Ubuntu
24.04 amd64 con 2 vCPU cada uno):

| evento | tiempo |
|---|---|
| agentes arrancan → casas relayadas | 44 s |
| se agrega el reenvío de puerto → directo | 27 s |
| se corta el camino directo → tráfico otra vez por el relay | 45 s |
| se restaura el camino → directo otra vez | 141 s (el siguiente handshake en la entrada directa) |

Después corrieron entre los NATs los niveles multi-nodo (M1) y relay (M2) del
corpus de compatibilidad: 5/5 y 2/2.

## Lo que no puede hacer

- **Dos NATs estrictos se quedan relayados.** Entre dos NATs que no dejan
  entrar paquetes no hay hole punching con el que se pueda contar. Los
  routers Linux, por ejemplo, le dan un puerto nuevo a un flujo cuando un
  paquete sin respuesta del otro lado llegó primero, así que ninguno de los
  dos ve nunca el puerto al que tiene que contestar. Si las dos casas están
  detrás de routers así, `home-a` ⇄ `home-b` pasa por `cloud` para siempre. La
  solución es reenviar el puerto UDP 51820 en uno de los dos routers: el par
  pasa a directo sin nada relayado, que es lo que mide la fila de 27 s.
- **Un solo salto de relay.** Un par solo se relaya a través de un nodo al
  que ambos lados llegan directo. Sin cadenas.
- **Solo IPv4**, un puerto de WireGuard por nodo.
- **Todos los nodos deben correr 0.2 o posterior.** Un agente más viejo nunca
  usa un relay.
- **El puerto de la API queda en internet.** Las casas se unen por el 6443 de
  `cloud`. Los servidores autentican clientes y aplican RBAC, pero todavía no
  hay OIDC ni revocación de certificados, y la documentación dice que el 6443
  y el puerto de Raft no deben quedar en redes no confiables. Restringe el
  security group todo lo que puedas.
- **La máquina de tu amigo guarda una credencial de admin.** El token de unión
  es una credencial de administrador y vive en cada nodo, en
  `/etc/litek8s/agent.env`. Une solo máquinas a cuyo root le confiarías el
  cluster.

## Más relays, o ninguno en el servidor

Si el servidor no debe cargar con tráfico ajeno, o quieres que una segunda VM
lo comparta, etiqueta los nodos:

```bash
kubectl label node my-cloud-vm litek8s.dev/relay=true
kubectl label node my-server litek8s.dev/relay=false
```

El relay de un par se elige con rendezvous hashing entre los relays a los que
llegan ambos lados, así que los pares se reparten entre relays y no se mueven
cuando se agrega otro.

## Pruébalo

```bash
curl -fsSL https://raw.githubusercontent.com/jairoFernandez/litek8s/main/install.sh | sh
```

Después apunta un archivo de cluster a tus propias máquinas. Todo el
mecanismo, con las anotaciones y las métricas, está en
[Red a través de NATs](/es/docs/networking/); la instalación por SSH y el
archivo de cluster en [Clusters por SSH](/es/docs/clusters/); el comando de
unión y HA en [Varios nodos y HA](/es/docs/multi-node/).
