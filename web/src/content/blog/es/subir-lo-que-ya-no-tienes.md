---
title: "Subir lo que ya no tienes: docker push desde capas descomprimidas"
description: "litek8s guarda las capas descomprimidas, no como los blobs en que llegaron. Cómo docker push sube igual los mismos digests, y qué comprueba un registry."
date: 2026-11-14
tags: [docker, registries, imágenes, oci]
docs: [docker, registries]
lang: es
translation: pushing-what-you-no-longer-have
---

Hasta 0.2.24, `docker push` contra el socket Docker de litek8s respondía
`501 Not Implemented`. Podías construir y correr imágenes, pero para
publicarlas necesitabas otro Docker. En 0.2.25 ya funciona. La mayor parte
del trabajo no fue la subida en sí, sino un problema del almacén: litek8s
ya no tiene los archivos que el registry espera recibir.

## Qué guarda un registry

Una imagen en un registry son tres tipos de objeto, cada uno nombrado por
el SHA-256 de sus bytes:

- **Capas**: un tar con los archivos, casi siempre comprimido con gzip. Ese
  archivo comprimido es el *blob*, y su digest es el nombre con que lo
  conoce el registry.
- **La config**: un JSON con el entorno, el comando y, en
  `rootfs.diff_ids`, el digest de cada tar **sin comprimir**. El digest de
  la config es el ID de imagen que muestra `docker images`.
- **El manifest**: un JSON que lista la config y los blobs. Su digest es el
  `sha256:...` que fijas en un despliegue.

Hay dos digests por capa: el del blob (comprimido) y el diff ID (sin
comprimir). Gzip no es determinista entre implementaciones, así que el
mismo tar puede dar blobs distintos, con digests distintos, según quién lo
comprima.

## Qué guarda litek8s

El almacén de imágenes de litek8s, que comparten los pods y la API Docker,
guarda cada capa **descomprimida**, como un directorio. Es lo que necesita
overlayfs para montar un contenedor, y evita tener cada capa dos veces. El
blob comprimido con que se descargó se tira después de descomprimirlo.

Para subir una imagen hacen falta esos blobs. La solución obvia, hacer un
tar de cada directorio y comprimirlo, da archivos con otros digests. Así,
la imagen subida ya no sería la que bajaste: otro digest de manifest, y
otro ID de imagen si el tar no sale idéntico byte a byte. Para una imagen
que construiste aquí da igual. Para un `alpine:3.20` que copias a tu propio
registry no: esperas el mismo digest que en Docker Hub.

## Cuatro maneras de subir una capa

Para cada capa, `push.rs` prueba, en este orden:

1. **El registry ya la tiene.** Un `HEAD` del blob. Si responde 200, no hay
   nada que mandar: `Layer already exists`.
2. **Montarla desde otro repositorio del mismo registry.** Si la capa vino
   de ese mismo registry, `POST /blobs/uploads/?mount=<digest>&from=<repo>`
   le pide que enlace el blob que ya tiene, sin mandar un byte:
   `Mounted from <repo>`.
3. **Volver a bajarla de donde vino.** El almacén recuerda de qué registry y
   repositorio se bajó cada capa. Si el destino es otro registry, se
   descarga otra vez el blob original, se comprueba contra su digest y se
   sube. Es el mismo archivo, así que conserva su digest.
4. **Fabricarla aquí.** Si nada de lo anterior funciona, hace un tar del
   directorio y lo comprime con gzip. El blob no se guarda (duplicaría el
   disco), pero su digest sí, junto a la capa, para que el siguiente push de
   esa imagen sean solo `HEAD`s.

El orden importa: las tres primeras conservan los digests originales, y
solo la última puede cambiarlos.

## El bug que estaba en el pull

Para subir una imagen bajada con el mismo digest hace falta también el
mismo **manifest**, byte a byte. Y ahí había un problema: al hacer pull,
litek8s parseaba el manifest y lo guardaba serializado otra vez. Mismo
contenido, pero otros bytes (orden de claves, espacios), así que el archivo
guardado ya no coincidía con su propio digest. Ahora el pull guarda el
manifest exactamente como lo mandó el registry, y el push de una imagen sin
cambios manda esos mismos bytes.

## Probarlo

En Linux (x86_64), con la release 0.2.25, un `litek8s dev up` rootless y el
CLI de docker 29 apuntando al socket de litek8s. El registry es un
`registry:2` que corre el propio litek8s:

```text
$ docker run -d --name reg -p 5000:5000 registry:2
$ docker pull alpine:3.20
$ docker tag alpine:3.20 localhost:5000/blog/alpine:3.20
$ docker push localhost:5000/blog/alpine:3.20
The push refers to repository [localhost:5000/blog/alpine]
08bc4e534116: Preparing
08bc4e534116: Waiting
08bc4e534116: Pushed
3.20: digest: sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e size: 1023
```

La capa subió por la opción 3: bajada otra vez de Docker Hub. El digest del
manifest es el que publica Docker Hub para `alpine:3.20` en linux/amd64:

```text
$ docker buildx imagetools inspect alpine:3.20 --raw   # la entrada amd64
sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e
```

Después, una imagen construida encima, con el builder clásico:

```dockerfile
FROM alpine:3.20
RUN echo built-by-litek8s > /hello
COPY note.txt /note.txt
```

```text
$ docker push localhost:5000/blog/app:1
The push refers to repository [localhost:5000/blog/app]
...
08bc4e534116: Mounted from blog/alpine
1c5396d940ac: Pushed
5c63336c4717: Pushed
1: digest: sha256:e51efb0c4c055b2ddd588117b53f74737c1f42525d22479485aeb5a2fce29257 size: 747
```

La base se montó (opción 2) y las dos capas nuevas se fabricaron aquí
(opción 4). Subirla otra vez no manda nada:

```text
08bc4e534116: Layer already exists
1c5396d940ac: Layer already exists
5c63336c4717: Layer already exists
1: digest: sha256:e51efb0c4c055b2ddd588117b53f74737c1f42525d22479485aeb5a2fce29257 size: 747
```

## Qué comprueba el registry, y qué comprobamos nosotros

Un registry comprueba que cada blob coincide con su digest. No comprueba
que la capa descomprimida coincida con el diff ID de la config. Si eso está
mal, el push funciona y la imagen se rompe para quien la baje. Así que un
script recorrió la API del registry y lo comprobó todo:

```text
manifest sha256 matches Docker-Content-Digest: True
config digest: sha256:3fe7e9a523e858b7b657062c9674c5d4e58120457aa8ccf7699d14e324b1075f
 layer sha256:25f1d6b1951a digest+size ok: True gunzip = diff_id: True
 layer sha256:c5424411fe89 digest+size ok: True gunzip = diff_id: True
 layer sha256:4e6a97d134b6 digest+size ok: True gunzip = diff_id: True
```

El digest de la config es el ID de imagen que dio `docker build` en local
(`3fe7e9a5...`): la imagen construida conserva su ID a través del push. Eso
se cumple porque las capas que litek8s fabrica al construir y al subir
salen del mismo escritor de tar, así que sus diff IDs coinciden.

La prueba final fue el Docker real del host, que ya tenía `alpine:3.20` de
Docker Hub:

```text
$ docker run --rm localhost:5000/blog/app:1 cat /hello
1: Pulling from blog/app
4e6a97d134b6: Pulling fs layer
25f1d6b1951a: Pulling fs layer
c5424411fe89: Pulling fs layer
4e6a97d134b6: Download complete
25f1d6b1951a: Already exists
c5424411fe89: Download complete
...
built-by-litek8s
```

`25f1d6b1951a: Already exists`: Docker reconoció la capa base como la que
ya tenía de Docker Hub. Es el mismo blob, byte a byte.

## Cuando el digest cambia igual

Si el blob original de una capa bajada ya no se puede alcanzar (el registry
está caído, o era uno privado al que ya no tienes acceso), cae en la opción
4. El tar fabricado aquí rara vez tiene el diff ID del original: tiene los
mismos archivos, pero otro escritor de tar los ordena y rellena a su
manera. La config subida nombra entonces los diff IDs nuevos, así que la
imagen funciona, pero su ID cambia. `docker save` hace lo mismo. El
contenido no cambia.

## Lo que todavía no hace

- **Índices multiplataforma.** Un tag del almacén guarda una plataforma, así
  que el push manda el manifest de esa plataforma, nunca un índice.
- **Subidas por trozos.** Cada blob va en un solo `PUT`. Suben hasta tres
  capas a la vez.
- **Servidores de tokens reales.** La autenticación por token (Bearer) se
  probó contra un registry en memoria en los tests unitarios, y la Basic
  contra un `registry:2` con htpasswd, `docker login` incluido. Todavía no
  hemos subido a Docker Hub ni a GHCR. Si lo haces, nos gustaría saber cómo
  te va.
- **macOS.** El código es el mismo, porque la API Docker corre en el
  invitado Linux de la VM de desarrollo, pero estas pruebas se hicieron en
  Linux.

Las credenciales salen de `docker login` o, si no, de la configuración de
registries ([registries.md](/es/docs/registries/)). Los registries en
`localhost` hablan HTTP plano, como con Docker.
