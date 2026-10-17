# L2: workload controllers and the node agent actually running containers.

case_def deploy_ready L2 180 "a Deployment rolls out and reports Available"
t_deploy_ready() {
    use_ns
    k -n "$NS" create deployment web --image=nginx:alpine --port=80 >/dev/null
    k -n "$NS" rollout status deployment/web --timeout=150s >/dev/null
    expect_eq "$(k -n "$NS" get deploy web -o jsonpath='{.status.availableReplicas}')" 1
}

case_def deploy_scale L2 180 "kubectl scale to 3 replicas, then back to 1"
t_deploy_scale() {
    use_ns
    k -n "$NS" create deployment web --image=nginx:alpine >/dev/null
    k -n "$NS" scale deployment web --replicas=3 >/dev/null
    k -n "$NS" rollout status deployment/web --timeout=120s >/dev/null
    expect_eq "$(k -n "$NS" get pods -l app=web --field-selector=status.phase=Running --no-headers | wc -l | tr -d ' ')" 3
    k -n "$NS" scale deployment web --replicas=1 >/dev/null
    wait_for 60 bash -c "[[ \$(kubectl -n $NS get pods -l app=web --no-headers 2>/dev/null | wc -l | tr -d ' ') == 1 ]]"
}

case_def deploy_rollout_undo L2 240 "image update rolls out a new ReplicaSet; undo restores it"
t_deploy_rollout_undo() {
    use_ns
    k -n "$NS" create deployment web --image=nginx:1.27-alpine >/dev/null
    k -n "$NS" rollout status deployment/web --timeout=120s >/dev/null
    k -n "$NS" set image deployment/web nginx=nginx:1.28-alpine >/dev/null
    k -n "$NS" rollout status deployment/web --timeout=120s >/dev/null
    expect_eq "$(k -n "$NS" get rs -l app=web --no-headers | wc -l | tr -d ' ')" 2
    k -n "$NS" rollout undo deployment/web >/dev/null
    k -n "$NS" rollout status deployment/web --timeout=120s >/dev/null
    expect_eq "$(k -n "$NS" get deploy web -o jsonpath='{.spec.template.spec.containers[0].image}')" nginx:1.27-alpine
}

case_def pod_logs L2 120 "kubectl logs returns container stdout"
t_pod_logs() {
    use_ns
    k -n "$NS" run talker --image=busybox:1.36 --restart=Never -- sh -c 'echo hello-compat; sleep 3600' >/dev/null
    k -n "$NS" wait --for=condition=Ready pod/talker --timeout=90s >/dev/null
    wait_for 20 bash -c "kubectl -n $NS logs talker | grep -q hello-compat"
}

case_def pod_exec L2 120 "kubectl exec runs a command in the container"
t_pod_exec() {
    use_ns
    client_pod
    expect_eq "$(k -n "$NS" exec client -- echo inside)" inside
}

case_def port_forward L2 150 "kubectl port-forward reaches the pod"
t_port_forward() {
    use_ns
    k -n "$NS" run web --image=nginx:alpine --port=80 >/dev/null
    k -n "$NS" wait --for=condition=Ready pod/web --timeout=90s >/dev/null
    local port=$((20000 + RANDOM % 20000))
    with_timeout 40 kubectl -n "$NS" port-forward pod/web "$port:80" >/dev/null 2>&1 &
    wait_for 20 curl -fsS "http://127.0.0.1:$port/"
    kill %1 2>/dev/null || true
}

case_def job_complete L2 150 "a Job runs to completion"
t_job_complete() {
    use_ns
    k -n "$NS" create job once --image=busybox:1.36 -- sh -c 'echo done' >/dev/null
    k -n "$NS" wait --for=condition=Complete job/once --timeout=120s >/dev/null
}

case_def cronjob_schedules L2 150 "a CronJob creates a Job on schedule"
t_cronjob_schedules() {
    use_ns
    k -n "$NS" create cronjob tick --image=busybox:1.36 --schedule='*/1 * * * *' -- true >/dev/null
    wait_for 130 bash -c "[[ -n \$(kubectl -n $NS get jobs -o name 2>/dev/null) ]]"
}

case_def statefulset_ordinals L2 240 "a StatefulSet creates stable, ordered pod names"
t_statefulset_ordinals() {
    use_ns
    cat <<EOF | k apply -f - >/dev/null
apiVersion: v1
kind: Service
metadata: {name: db, namespace: $NS}
spec: {clusterIP: None, selector: {app: db}, ports: [{port: 80}]}
---
apiVersion: apps/v1
kind: StatefulSet
metadata: {name: db, namespace: $NS}
spec:
  serviceName: db
  replicas: 2
  selector: {matchLabels: {app: db}}
  template:
    metadata: {labels: {app: db}}
    spec:
      containers: [{name: web, image: nginx:alpine}]
EOF
    k -n "$NS" rollout status statefulset/db --timeout=200s >/dev/null
    k -n "$NS" get pod db-0 db-1 >/dev/null
}

case_def daemonset_all_nodes L2 180 "a DaemonSet runs one pod per node"
t_daemonset_all_nodes() {
    use_ns
    cat <<EOF | k apply -f - >/dev/null
apiVersion: apps/v1
kind: DaemonSet
metadata: {name: agent, namespace: $NS}
spec:
  selector: {matchLabels: {app: agent}}
  template:
    metadata: {labels: {app: agent}}
    spec:
      containers: [{name: pause, image: busybox:1.36, command: [sleep, "3600"]}]
EOF
    k -n "$NS" rollout status daemonset/agent --timeout=150s >/dev/null
    expect_eq "$(k -n "$NS" get ds agent -o jsonpath='{.status.numberReady}')" "$(node_count)"
}

case_def multi_container_pod L2 150 "init container + two containers sharing an emptyDir"
t_multi_container_pod() {
    use_ns
    cat <<EOF | k apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: duo, namespace: $NS}
spec:
  volumes: [{name: shared, emptyDir: {}}]
  initContainers:
    - {name: seed, image: busybox:1.36, command: [sh, -c, "echo seeded > /data/init"], volumeMounts: [{name: shared, mountPath: /data}]}
  containers:
    - {name: writer, image: busybox:1.36, command: [sh, -c, "echo hi > /data/msg; sleep 3600"], volumeMounts: [{name: shared, mountPath: /data}]}
    - {name: reader, image: busybox:1.36, command: [sleep, "3600"], volumeMounts: [{name: shared, mountPath: /data}]}
EOF
    k -n "$NS" wait --for=condition=Ready pod/duo --timeout=120s >/dev/null
    wait_for 15 bash -c "kubectl -n $NS exec duo -c reader -- cat /data/msg | grep -q hi"
    expect_eq "$(k -n "$NS" exec duo -c reader -- cat /data/init)" seeded
}

case_def config_injection L2 150 "ConfigMap/Secret as env vars and mounted files"
t_config_injection() {
    use_ns
    k -n "$NS" create configmap cfg --from-literal=MODE=fast >/dev/null
    k -n "$NS" create secret generic sec --from-literal=token=s3cr3t >/dev/null
    cat <<EOF | k apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: cfg, namespace: $NS}
spec:
  volumes:
    - {name: sec, secret: {secretName: sec}}
  containers:
    - name: app
      image: busybox:1.36
      command: [sleep, "3600"]
      envFrom: [{configMapRef: {name: cfg}}]
      volumeMounts: [{name: sec, mountPath: /etc/sec}]
EOF
    k -n "$NS" wait --for=condition=Ready pod/cfg --timeout=120s >/dev/null
    expect_eq "$(k -n "$NS" exec cfg -- printenv MODE)" fast
    expect_eq "$(k -n "$NS" exec cfg -- cat /etc/sec/token)" s3cr3t
}

case_def http_readiness L2 150 "an httpGet readiness probe gates Ready"
t_http_readiness() {
    use_ns
    cat <<EOF | k apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: probed, namespace: $NS}
spec:
  containers:
    - name: web
      image: nginx:alpine
      readinessProbe: {httpGet: {path: /, port: 80}, periodSeconds: 2}
EOF
    k -n "$NS" wait --for=condition=Ready pod/probed --timeout=120s >/dev/null
}

case_def restart_on_crash L2 150 "a crashing container is restarted (restartCount > 0)"
t_restart_on_crash() {
    use_ns
    k -n "$NS" run crashy --image=busybox:1.36 --restart=Always -- sh -c 'sleep 2; exit 1' >/dev/null
    wait_for 120 bash -c "[[ \$(kubectl -n $NS get pod crashy -o jsonpath='{.status.containerStatuses[0].restartCount}') -ge 1 ]]"
}

case_def pvc_local L2 180 "a PVC binds and keeps data across pod restarts"
t_pvc_local() {
    use_ns
    cat <<EOF | k apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: data, namespace: $NS}
spec:
  accessModes: [ReadWriteOnce]
  resources: {requests: {storage: 16Mi}}
---
apiVersion: v1
kind: Pod
metadata: {name: w, namespace: $NS}
spec:
  volumes: [{name: d, persistentVolumeClaim: {claimName: data}}]
  containers: [{name: c, image: busybox:1.36, command: [sh, -c, "echo kept > /d/f; sleep 3600"], volumeMounts: [{name: d, mountPath: /d}]}]
EOF
    k -n "$NS" wait --for=condition=Ready pod/w --timeout=150s >/dev/null
    k -n "$NS" delete pod w --wait=true >/dev/null
    cat <<EOF | k apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: r, namespace: $NS}
spec:
  volumes: [{name: d, persistentVolumeClaim: {claimName: data}}]
  containers: [{name: c, image: busybox:1.36, command: [sleep, "3600"], volumeMounts: [{name: d, mountPath: /d}]}]
EOF
    k -n "$NS" wait --for=condition=Ready pod/r --timeout=120s >/dev/null
    expect_eq "$(k -n "$NS" exec r -- cat /d/f)" kept
}
