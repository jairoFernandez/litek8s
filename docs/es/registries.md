# Registries privados y mirrors

Los nodos descargan las imágenes por su cuenta (el runtime crun y la API de
Docker de un cluster de desarrollo comparten un mismo mecanismo de descarga). De
fábrica confían en las CAs públicas, hablan HTTP plano solo con `localhost` /
`127.0.0.1` y descargan de forma anónima. Una **configuración de registries**
agrega, por cada host de registry: una CA privada, un certificado de cliente,
`insecure_skip_verify`, credenciales, HTTP plano y mirrors (una caché
pull-through para `docker.io`, un registry propio delante de otro). Es el
formato de `/etc/rancher/k3s/registries.yaml` de k3s, así que un archivo de k3s
funciona tal cual.

```yaml
mirrors:
  docker.io:                          # pulls of docker.io/... try these first, in order,
    endpoint:                         # then docker.io itself
      - "https://cache.lan:5000"
  registry.lan:5000:                  # plain HTTP: an http:// endpoint of its own name
    endpoint: ["http://registry.lan:5000"]
  "*":                                # every registry without its own entry
    endpoint: ["https://harbor.lan/proxy"]
    rewrite:                          # the repository at the mirrors (not at the registry)
      "^library/(.*)": "dockerhub/$1"
configs:
  gitea.platform.lan:                 # host[:port] of a registry or of a mirror endpoint
    auth:
      username: ci
      password: s3cret                # or auth: <base64 of user:password>, or identitytoken: <refresh token>
    tls:
      ca_file: /etc/litek8s/platform-ca.pem   # trusted next to the public CAs
  cache.lan:5000:
    tls:
      ca: |                           # the PEM inline (litek8s; k3s has only ca_file)
        -----BEGIN CERTIFICATE-----
        ...
      cert_file: /etc/litek8s/client.pem      # a client certificate (or cert: / key: inline)
      key_file: /etc/litek8s/client-key.pem
  "*":
    tls:
      insecure_skip_verify: false
```

Cómo la usa una descarga:

- El registry de `image:` (`docker.io` para `nginx`, `gitea.platform.lan` para
  `gitea.platform.lan/team/app:1`) elige la entrada de `mirrors`, o si no, la de
  `"*"`. Sus endpoints se prueban en orden y el registry mismo al final; el
  primero que tenga el manifiesto sirve la imagen completa. Cuando todos fallan,
  el error muestra el de cada uno.
- Un endpoint es `https://host[:port][/path]` (sin esquema significa https). La
  raíz de la API es `<endpoint>/v2`, o la ruta misma cuando termina en `/v2`.
- Un endpoint con el propio nombre del registry solo define su esquema: así es
  como un registry pasa a HTTP plano (`registry.lan:5000: {endpoint:
  ["http://registry.lan:5000"]}`), y como `localhost:5000` pasa a HTTPS.
  `LITEK8S_INSECURE_REGISTRIES=host:port,...` en el entorno del servidor o del
  agent también sigue funcionando.
- `configs` se busca por el `host:port` del endpoint (solo `host` para el puerto
  443), luego, para el registry mismo, por el nombre del registry
  (`docker.io`), y luego `"*"`. Las credenciales funcionan con registries que
  piden Basic auth (registry:2 con htpasswd) y con servicios de tokens (Docker
  Hub, GHCR, Gitea, Harbor); `identitytoken` se intercambia como un refresh
  token de OAuth2.
- Las credenciales de `docker login` de la API de Docker tienen prioridad sobre
  el archivo para el registry mismo (no para sus mirrors).
- Los nombres de archivo relativos (`ca_file: ca.pem`) son relativos al archivo.

Los `imagePullSecrets` de los pods no se usan: las credenciales vienen solo de
la configuración de los nodos. La API de Docker todavía no soporta
`docker push`.

## Dónde va

**Un servidor o agent que corres tú mismo**: `litek8s apiserver` y
`litek8s agent` leen `/etc/litek8s/registries.yaml` cuando existe, o el archivo
que indique `--registry-config FILE`. Todo nodo que descargue imágenes lo
necesita, servidores incluidos a menos que usen `--no-node`.

**Instalado con `litek8s install`**: `--registries FILE` lo instala como
`/etc/litek8s/registries.yaml` (modo 0600; los archivos que menciona se leen y
se incluyen inline) y reinicia el servicio cuando cambió:

```bash
sudo litek8s install server --registries registries.yaml
litek8s install agent --ssh pi@192.168.1.20 --join https://192.168.1.10:6443 --token K10... --registries registries.yaml
```

Con `--ssh` el archivo se lee en esta máquina (con sus archivos de CA y de
llave) y se sube igual que el join token. En un [cluster file](../clusters.md#un-cluster-file),
`registries: registries.yaml` (relativo al cluster file) va a todas las
máquinas.

**Clusters de desarrollo**: `litek8s dev up` lee `~/.litek8s/registries.yaml`
(`$LITEK8S_HOME/registries.yaml` con `LITEK8S_HOME`) y se lo pasa, con los
archivos incluidos inline, a cada nodo del cluster que arranca (la VM de
desarrollo no ve tus archivos). Ahí nada lee el `/etc/litek8s/registries.yaml`
de la máquina. Un registry en el `localhost` de tu Mac o PC ya funciona sin
configuración ([dev.md](../dev.md#imágenes)).

## Los cambios requieren un reinicio

La configuración se lee cuando arranca el servidor o el agent:

- instalado: `litek8s install ... --registries FILE` otra vez (reinicia el
  servicio), o, después de editar `/etc/litek8s/registries.yaml` a mano,
  `sudo systemctl restart litek8s-server` (o `litek8s-agent`) en cada máquina;
- clusters de desarrollo: `litek8s dev down --name N && litek8s dev up --name N`.

Los pods de un servicio instalado siguen corriendo durante su reinicio; las
imágenes ya descargadas se conservan en ambos casos.

## Cómo comprobarlo

El servidor o el agent registra `registries: 2 mirror(s), 1 config(s) from
/etc/litek8s/registries.yaml` al arrancar, y se niega a arrancar con un archivo
inválido (un campo desconocido, una CA sin certificado, `username` sin
`password`). Una descarga que falla dice por qué en los eventos del pod:

```text
Failed to pull image "gitea.platform.lan/team/app:1": ... TLS handshake with gitea.platform.lan (10.0.0.5:443) failed: invalid peer certificate: UnknownIssuer
Failed to pull image "...": gitea.platform.lan requires authentication: give it credentials in the registry configuration (registries.yaml), or `docker login` for the Docker API
Failed to pull image "...": gitea.platform.lan refused the credentials the registry configuration (registries.yaml) or `docker login` gave it
```

`bench/registry-e2e.sh` corre un registry:2 con TLS de una CA privada y Basic
auth en un cluster de desarrollo y comprueba que un pod descarga de él solo con
la configuración.
