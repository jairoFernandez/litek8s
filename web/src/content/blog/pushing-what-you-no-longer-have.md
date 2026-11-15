---
title: "Pushing what you no longer have: docker push from unpacked layers"
description: "litek8s keeps image layers unpacked, not as the blobs they came in. How docker push still sends the same digests, and what a registry checks."
date: 2026-11-14
tags: [docker, registries, images, oci]
docs: [docker, registries]
translation: es/subir-lo-que-ya-no-tienes
---

Until 0.2.24, `docker push` against litek8s's Docker socket answered
`501 Not Implemented`. You could build and run images, but to publish them
you needed another Docker. 0.2.25 adds it. Most of the work was not the
upload itself but a problem with the store: litek8s no longer has the files
the registry expects to receive.

## What a registry stores

An image in a registry is three kinds of object, each named by the SHA-256
of its bytes:

- **Layers**: a tar of the files, almost always gzipped. That compressed
  file is the *blob*, and its digest is what the registry knows it by.
- **The config**: a JSON with the environment, the command and, under
  `rootfs.diff_ids`, the digest of each layer's tar **uncompressed**. The
  config's digest is the image ID that `docker images` shows.
- **The manifest**: a JSON that lists the config and the blobs. Its digest
  is the `sha256:...` you pin in a deployment.

There are two digests per layer: the blob's (compressed) and the diff ID
(uncompressed). Gzip is not deterministic between implementations, so the
same tar can produce different blobs, with different digests, depending on
who compresses it.

## What litek8s keeps

litek8s's image store, shared by pods and the Docker API, keeps each layer
**unpacked**, as a directory. That is what overlayfs needs to mount a
container, and it avoids keeping every layer twice. The compressed blob it
was downloaded as is thrown away after unpacking.

To push an image you need those blobs. The obvious solution, making a tar
of each directory and compressing it, gives files with other digests. So
the pushed image would no longer be the one you pulled: a different
manifest digest, and a different image ID if the tar does not come out
byte-identical. For an image you built here, that does not matter. For an
`alpine:3.20` you are copying to your own registry, it does: you would
expect the same digest as on Docker Hub.

## Four ways to get a layer up

For each layer, `push.rs` tries, in this order:

1. **The registry already has it.** A `HEAD` of the blob. If it answers
   200, there is nothing to send: `Layer already exists`.
2. **Mount it from another repository of the same registry.** If the layer
   came from that same registry, `POST /blobs/uploads/?mount=<digest>&from=<repo>`
   asks the registry to link the blob it already has, without sending a
   byte: `Mounted from <repo>`.
3. **Fetch it again from where it came from.** The store remembers which
   registry and repository each layer was pulled from. If the destination
   is a different registry, the original blob is downloaded again, checked
   against its digest and uploaded. It is the same file, so it keeps its
   digest.
4. **Make it here.** If none of the above works, it tars and gzips the
   directory. The blob is not kept (that would double disk use), but its
   digest is, next to the layer, so the next push of that image is just
   `HEAD`s.

The order matters: the first three keep the original digests, and only the
last one can change them.

## The bug that was on the pull side

To push a pulled image with the same digest, you also need the same
**manifest**, byte for byte. And there was a problem there: when pulling,
litek8s parsed the manifest and saved it serialized again. Same content,
but other bytes (key order, spaces), so the stored file no longer matched
its own digest. Pulls now save the manifest exactly as the registry sent
it, and a push of an unchanged image sends those same bytes.

## Trying it

On Linux (x86_64), with the 0.2.25 release, a rootless `litek8s dev up`
and the docker 29 CLI pointed at litek8s's socket. The registry is a
`registry:2` that litek8s itself runs:

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

The layer went up by option 3: fetched again from Docker Hub. The manifest
digest is the one Docker Hub publishes for `alpine:3.20` on linux/amd64:

```text
$ docker buildx imagetools inspect alpine:3.20 --raw   # the amd64 entry
sha256:c64c687cbea9300178b30c95835354e34c4e4febc4badfe27102879de0483b5e
```

Then an image built on top of it, with the classic builder:

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

The base was mounted (option 2), and the two new layers were made here
(option 4). Pushing it again sends nothing:

```text
08bc4e534116: Layer already exists
1c5396d940ac: Layer already exists
5c63336c4717: Layer already exists
1: digest: sha256:e51efb0c4c055b2ddd588117b53f74737c1f42525d22479485aeb5a2fce29257 size: 747
```

## What the registry checks, and what we checked

A registry checks that each blob matches its digest. It does not check that
the uncompressed layer matches the config's diff ID. If that is wrong, the
push works and the image breaks for whoever pulls it. So a script went
through the registry API and checked everything:

```text
manifest sha256 matches Docker-Content-Digest: True
config digest: sha256:3fe7e9a523e858b7b657062c9674c5d4e58120457aa8ccf7699d14e324b1075f
 layer sha256:25f1d6b1951a digest+size ok: True gunzip = diff_id: True
 layer sha256:c5424411fe89 digest+size ok: True gunzip = diff_id: True
 layer sha256:4e6a97d134b6 digest+size ok: True gunzip = diff_id: True
```

The config digest is the image ID that `docker build` gave locally
(`3fe7e9a5...`): the built image keeps its ID through the push. That holds
because the layers litek8s makes at build time and at push time come from
the same tar writer, so their diff IDs match.

The final test was the host's real Docker, which already had `alpine:3.20`
from Docker Hub:

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

`25f1d6b1951a: Already exists`: Docker recognized the base layer as the one
it already had from Docker Hub. It is the same blob, byte for byte.

## When the digest changes anyway

If a pulled layer's original blob can no longer be reached (the registry is
down, or it was a private registry you no longer have access to), it falls
through to option 4. The tar made here rarely has the original's diff ID:
it has the same files, but another tar writer orders and pads them its own
way. The pushed config then names the new diff IDs, so the image works,
but its ID changes. `docker save` does the same. The content does not
change.

## What it does not do yet

- **Multi-platform indexes.** A tag in the store holds one platform, so a
  push sends that platform's manifest, never an index.
- **Chunked uploads.** Each blob goes in a single `PUT`. Up to three layers
  go up at once.
- **Real token servers.** Bearer token auth was tested against an
  in-memory registry in the unit tests, and Basic auth against an
  htpasswd `registry:2`, `docker login` included. We have not yet pushed
  to Docker Hub or GHCR. If you do, we would like to know how it goes.
- **macOS.** The code path is the same, since the Docker API runs in the
  dev VM's Linux guest, but these tests were run on Linux.

Credentials come from `docker login`, or else from the registry
configuration ([registries.md](/docs/registries/)). `localhost` registries
speak plain HTTP, as with Docker.
