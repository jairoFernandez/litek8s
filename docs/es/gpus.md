# GPUs y otros dispositivos (CDI)

Un nodo con GPUs (o cualquier otro dispositivo que un fabricante describa con
la [Container Device Interface](https://github.com/cncf-tags/container-device-interface))
se las ofrece a los pods como recursos extendidos, sin device plugin,
operador ni DaemonSet: el agente del nodo lee los specs CDI del nodo, anuncia
una unidad por dispositivo y le da a cada contenedor los dispositivos que
pidió. Nodos Linux con el runtime crun (servidores y agentes); no el runtime
Docker de los servidores macOS.

## En el nodo

Instala el driver del fabricante y su herramienta CDI, y genera el spec.
NVIDIA:

```bash
# el driver, luego el NVIDIA Container Toolkit (nvidia-ctk)
sudo nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml
nvidia-ctk cdi list        # nvidia.com/gpu=0, nvidia.com/gpu=1, nvidia.com/gpu=all, ...
```

AMD (`amd-ctk cdi generate`) e Intel escriben el suyo igual. Después
(re)inicia el litek8s del nodo (`sudo systemctl restart litek8s-agent`, o
`litek8s-server` en un servidor): registra `2 nvidia.com/gpu device(s) from
CDI specs` y el Node las muestra:

```console
$ kubectl describe node gpu1 | grep -A6 Capacity
Capacity:
  cpu:             16
  memory:          65536000Ki
  nvidia.com/gpu:  2
```

El `kind` de cada spec (`nvidia.com/gpu`, `amd.com/gpu`,
`intel.com/gpu`...) se vuelve un recurso con una unidad por dispositivo con
nombre; `all` no cuenta. Los specs se leen de `/etc/cdi` y `/var/run/cdi`
(para un dispositivo definido dos veces gana el archivo posterior);
`LITEK8S_CDI_SPEC_DIRS=/a:/b` en el entorno del servicio lee otros en su
lugar, y un valor vacío apaga CDI.

## En un pod

Se pide en `limits`, como cualquier recurso de un device plugin:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: cuda-check
spec:
  restartPolicy: Never
  containers:
    - name: cuda
      image: nvcr.io/nvidia/cuda:12.6.3-base-ubuntu24.04
      command: ["nvidia-smi", "-L"]
      resources:
        limits:
          nvidia.com/gpu: 1
```

El scheduler pone el pod en un nodo con una GPU libre (o informa
`Insufficient nvidia.com/gpu`, y desaloja pods de menor prioridad para
conseguir una). En el nodo el contenedor recibe los edits CDI de sus
dispositivos: los device nodes (permitidos en su cgroup de dispositivos), las
librerías del driver montadas desde el nodo, el entorno y los hooks del
fabricante (`nvidia-ctk hook ...`). Nada en la imagen necesita saber de
litek8s.

- **Exclusivo.** Un dispositivo pertenece a un solo pod a la vez. Los
  contenedores de un pod reciben dispositivos distintos; los init containers
  reutilizan los del pod.
- **Persistente.** Qué pod tiene qué dispositivo se guarda en
  `<directorio del nodo>/cdi-allocations.json`, así que reiniciar el agente
  le devuelve a cada pod en ejecución los mismos dispositivos; los
  dispositivos de un pod quedan libres cuando el pod desaparece.
- **Leído al arrancar el contenedor.** Los edits salen de los specs tal como
  están cuando el contenedor arranca, así que regenerar el spec tras
  actualizar el driver no requiere reiniciar; agregar o quitar dispositivos
  sí (la capacidad se lee al arrancar).

Para mantener otros pods fuera de los nodos con GPU, ponles un taint
(`gpu=true:NoSchedule`, también en un
[cluster file](clusters.md#un-cluster-file)) y tolera el taint en los pods
con GPU.

## No soportado

- Time-slicing, particiones MIG y otras formas de compartir un dispositivo
  entre varios pods (los dispositivos MIG que `nvidia-ctk` lista como
  dispositivos CDI propios son unidades separadas, como cualquier otro).
- Dynamic Resource Allocation (ResourceClaims de `resource.k8s.io`) y device
  plugins: los specs CDI son la única fuente de dispositivos.
- Elegir dispositivos por topología (NUMA, NVLink): un pod recibe los
  dispositivos libres en el orden del spec.
- Métricas de GPU: corre el DCGM exporter de NVIDIA y apunta un Prometheus a
  él.
