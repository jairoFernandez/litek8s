# Números

Medidos contra k3s, kind y k3d con los scripts de este repositorio; los archivos
enlazados tienen la configuración, los datos crudos y las salvedades.

| | litek8s | referencia | fuente |
|---|---|---|---|
| Control plane completo, en reposo, sin pods (PSS, v0.2.1, VM x86_64 de 1 GiB) | **23.2 MiB** | k3s mínimo: 567 MiB | [bench/RESULTS.md](](../../bench/RESULTS.md#footprint-of-the-v021-release-2026-10-21-x86_64kvm)) |
| Orquestador completo, 5 pods de nginx (igual) | **33.5 MiB** | k3s mínimo: 579 MiB | igual |
| Argo CD + una app sincronizada, memoria del orquestador | **90 MiB** | k3s mínimo: 900 MiB | [bench/RESULTS.md](](../../bench/RESULTS.md#argo-cd-on-k3s-and-on-litek8s-2026-10-19-x86_64kvm)) |
| `dev up` → nodo Ready, Linux / macOS | **0.17 s / 1.28 s** | kind 31.6 s / 34.3 s, k3d 11.1 s / 10.4 s | [bench/RESULTS.md](](../../bench/RESULTS.md#local-dev-cluster-litek8s-dev-vs-kind-and-k3d-2026-10-18)) |
| Memoria en reposo de un cluster de desarrollo, Linux | **47 MiB** | kind 526 MiB, k3d 398 MiB | igual |
| Job de GitHub Actions, e2e de un chart de Helm (setup, instalación, prueba, upgrade) | **21 s**, cluster listo en 2.6 s | kind (helm/kind-action): 79 s, 44.7 s | [docs/ci.md](../ci.md#cifras) |
| Métricas, logs y alertas, memoria agregada | **+1.8 MiB** | kube-prometheus-stack + Loki: 1,371 MiB | [bench/RESULTS.md](](../../bench/RESULTS.md#observability-built-in-vs-kube-prometheus-stack--loki-2026-10-20-x86_64kvm)) |
| Corpus de compatibilidad (kubectl, CRDs, workloads, redes, 47 charts de Helm) | **119/119** | k3s: 119/119 | [compat/RESULTS.md](](../../compat/RESULTS.md#without-docker-2026-10-19-119-cases)) |
| Conformidad con Kubernetes v1.33 (419 specs) | **413/419** | k3s: 416/419 | [compat/CONFORMANCE.md](../../compat/CONFORMANCE.md) |

En macOS un cluster de desarrollo corre en una VM Linux, y el Monitor de
Actividad muestra la VM completa: 238 MiB recién creada, ~380 MiB después de
descargar y correr 5 pods de nginx (de eso, el cluster en sí son 30 y 79 MiB).
macOS le sigue cobrando a la VM cada página que su guest haya tocado hasta que
la VM se detiene ([docs/dev.md](../dev.md#memoria-en-macos)).

Los 6 fallos de conformidad: 5 son exclusivos de macOS (DNS por TCP en el puerto
53, un binding de hostPort) y pasan en Linux; 1 era un watch que ya se corrigió.
Un control plane HA de 3 servidores se mantiene linealizable cuando se matan
líderes y hay particiones ([bench/HA.md](../../bench/HA.md)); un cluster de 3
nodos corrió 21 h bajo churn sin fugas ([bench/SOAK.md](../../bench/SOAK.md)).
