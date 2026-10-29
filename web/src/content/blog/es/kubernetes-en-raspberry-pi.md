---
title: "Kubernetes en una Raspberry Pi y una laptop vieja con litek8s"
description: "Convierte una Raspberry Pi, una laptop vieja y una VM barata en la nube en un solo cluster de Kubernetes con litek8s: un binario por máquina, instalado por SSH."
date: 2026-10-28
updated: 2026-10-29
lang: es
translation: raspberry-pi-kubernetes-cluster
tags: [home lab, raspberry pi, multi-nodo]
docs: [clusters, multi-node, networking, platforms]
---

Casi todos los home labs empiezan igual: una Raspberry Pi en un cajón, una
laptop demasiado lenta para el día a día, quizá una VM barata en la nube.
Cada una es muy chica por sí sola, y una distribución de Kubernetes "de
verdad" pide más de lo que tienen. litek8s se escribió justamente para este
hardware.

## Por qué litek8s cabe en máquinas chicas

litek8s sirve la API real de Kubernetes, así que `kubectl`, Helm y Argo CD
funcionan sin cambios, pero el API server, los controladores, el scheduler,
el DNS, el proxy de servicios y el agente de nodo son **un solo proceso de
Rust**. En una VM x86_64 de 1 GiB todo el plano de control queda en reposo
con **23 MiB**, contra 567 MiB de un k3s mínimo
([los números y cómo se midieron](/es/docs/numbers/)). En una Pi con 1–4 GiB
de RAM, esa diferencia es lo que queda para tus pods.

Los nodos corren contenedores con [crun](https://github.com/containers/crun)
y sin daemon de contenedores. Cada máquina necesita Linux en x86_64 o aarch64
(o sea, un sistema de 64 bits en la Pi), systemd, y root o sudo. Nada más de
antemano: litek8s, crun e iptables se instalan si faltan.

## Una máquina a la vez

Desde tu laptop, con tu propio `ssh` (aplican tu `~/.ssh/config`, tus llaves
y tu agente):

```bash
# la Pi es el servidor; también corre pods
litek8s install server --ssh pi@192.168.1.10

# la laptop vieja se le une, con el token que imprimió el servidor
litek8s install agent --ssh me@old-laptop \
  --join https://192.168.1.10:6443 --token K10<hash>::<secret>
```

El contexto del servidor se agrega a tu kubeconfig y queda como actual, con
la CA del cluster verificada contra el join token, así que `kubectl get nodes`
funciona enseguida sobre TLS verificado. Si sudo pide contraseña, litek8s te
la pide una vez por máquina y se la pasa a `sudo -S` por SSH.

Cada paso es un comando SSH cuya salida ves: inspeccionar la máquina, poner
el binario en su lugar (se conserva si ya es el correcto; si no, se sube o se
descarga del release de GitHub y se verifica contra su `SHA256SUMS`),
instalar paquetes, y después escribir y arrancar una unidad de systemd.

## Todo el cluster en un archivo

Cuando ya hay tres máquinas, un archivo es más fácil que recordar flags:

```yaml
apiVersion: litek8s.dev/v1
kind: Cluster
metadata:
  name: home                 # el contexto de kubeconfig
servers:
  - ssh: pi@192.168.1.10
agents:
  - ssh: me@old-laptop
    labels: {disk: ssd}
  - ssh: ubuntu@ec2-host
    name: ec2
    nodeIp: 10.0.0.7
    taints: ["burst=true:NoSchedule"]
```

```bash
litek8s cluster apply -f cluster.yaml    # instala o actualiza cada máquina
litek8s cluster status -f cluster.yaml   # servicio, versión, estado, pods por máquina
```

`apply` es idempotente: córrelo de nuevo y responde
`cluster home is up to date: nothing changed`, sin reiniciar nada. Cambia una
etiqueta y solo cambia esa etiqueta. Saca una máquina del archivo y corre
`apply --prune`: se acordona, sus pods se mueven a otro lado y litek8s se
desinstala de ella. Las etiquetas y taints que pusiste a mano no se tocan.

## Máquinas en lugares distintos

La laptop de la casa y la VM de la nube casi nunca están en la misma red. Los
pods de nodos distintos hablan por una malla WireGuard que litek8s arma solo,
y cuando dos nodos no se alcanzan directamente (dos casas detrás de NAT, por
ejemplo), su tráfico pasa por un nodo al que ambos llegan. Dale a una VM en la
nube su dirección pública con `--node-external-ip`; la
[guía de redes](/es/docs/networking/) explica el resto.

## Qué tener en cuenta

litek8s es joven, y lo dice. Desde la 0.2.12 un servidor autentica clientes y
aplica RBAC (el contexto que agregan `install --ssh` y `cluster apply` lleva
un certificado de cliente de admin), pero todavía no hay login OIDC ni
revocación de certificados, así que mantén el puerto 6443 en una red de
confianza o detrás de una VPN. Si tu cluster se creó con una versión
anterior, lee [Autenticación y RBAC](/es/docs/auth/) antes de actualizar. La
lista completa está en [Limitaciones](/es/docs/limitations/).

La instalación por SSH y el cluster file están probados de punta a punta en
tres VMs con Ubuntu 24.04 (`bench/cluster-e2e.sh`): apply, un re-apply que no
reinicia nada, pods entre nodos, `--prune` y delete.
