# Quickstart

```bash
litek8s dev up
kubectl create deployment web --image=nginx
kubectl expose deployment web --type=NodePort --port 80
curl localhost:$(kubectl get svc web -o jsonpath='{.spec.ports[0].nodePort}')

litek8s ui                                   # browse it all
export DOCKER_HOST=unix://$HOME/.litek8s/docker.sock
docker run --rm hello-world                  # the same cluster speaks the Docker API

litek8s dev up --name ci --nodes 3           # another cluster, three nodes
litek8s dev down                             # stop; objects and images are kept
litek8s dev delete                           # remove everything
```

Every cluster, and every node of a multi-node cluster, runs in one small
VM on macOS or one rootless sandbox on Linux, started with the first
cluster and stopped with the last. Each cluster gets its own API port,
kubectl context and Docker socket:

```mermaid
flowchart LR
    subgraph host["your machine"]
        you["kubectl · docker · litek8s ui"]
        subgraph env["one VM / sandbox"]
            subgraph dev["cluster dev"]
                d1["node"]
            end
            subgraph ci["cluster ci, 3 nodes"]
                c1["node 1<br/>API server"]
                c2["node 2"]
                c3["node 3"]
                c2 & c3 -->|join| c1
            end
        end
    end
    you -->|"127.0.0.1:6443<br/>context litek8s-dev"| d1
    you -->|"127.0.0.1:port<br/>context litek8s-ci"| c1
    you -.->|"docker.sock"| d1
```
