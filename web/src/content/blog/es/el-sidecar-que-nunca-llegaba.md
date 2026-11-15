---
title: "El sidecar que nunca llegaba: webhooks de admisión y los pods de los controladores"
description: "Los inyectores de sidecars parcheaban un pod de kubectl run, no los de un Deployment. Por qué los controladores saltaban la admisión y qué cambió en 0.2.25."
date: 2026-11-14
tags: [admisión, webhooks, controladores, sidecars]
docs: [limitations, auth]
lang: es
translation: the-sidecar-that-never-arrived
---

Istio, Linkerd y Vault Agent añaden su contenedor a tus pods con un webhook
de admisión mutante. En litek8s 0.2.24 y anteriores eso funcionaba con un
pod creado a mano con `kubectl run`, pero no con los pods de un Deployment.
Esos salían sin sidecar, y las políticas de Kyverno o Gatekeeper tampoco se
les aplicaban.

Este artículo explica por qué, cómo se arregló en 0.2.25 y qué pasa ahora
cuando el webhook se cae.

## Qué hace un webhook mutante

Cada escritura en la API de Kubernetes pasa por los mismos pasos antes de
guardarse:

1. **Autenticación**: quién pregunta.
2. **Autorización**: si puede hacerlo (RBAC).
3. **Admisión mutante**: webhooks que pueden cambiar el objeto. Un inyector
   de sidecars responde con un parche JSON que añade un contenedor.
4. **Validación** del objeto contra su esquema.
5. **Admisión validante**: webhooks y ValidatingAdmissionPolicies que solo
   pueden decir sí o no. Ahí viven Kyverno y Gatekeeper.
6. **El almacén**.

Cuando creas un Deployment, no eres tú quien crea sus pods. El controlador
de Deployments crea un ReplicaSet, y el de ReplicaSets crea los pods. En
Kubernetes upstream esos controladores corren en kube-controller-manager,
que es un cliente más de la API: manda los pods por HTTP igual que kubectl,
como el usuario `system:kube-controller-manager`. Así que sus pods pasan por
los mismos seis pasos y el inyector los ve.

## Por qué en litek8s se la saltaban

litek8s corre el API server, los controladores y el scheduler en un solo
proceso. Los controladores no hablan con la API por HTTP: llaman
directamente a las funciones internas del servidor (`create_object`,
`update_object`). Así se ahorran serializar cada escritura, abrir una
conexión y comprobar un token en cada una. Pero la admisión estaba en el
handler HTTP, y el camino directo no pasaba por ella.

Resultado: todo lo que el servidor escribía por su cuenta se saltaba los
pasos 3 y 5: los ReplicaSets, los pods, los pods de los Jobs, los Events.
Nadie lo había notado porque, sin webhooks ni políticas, no falta nada. Solo
se nota cuando instalas uno.

Había un segundo fallo, más pequeño: los webhooks recibían siempre
`system:admin` como usuario de la petición, la hiciera quien la hiciera. Eso
rompía las políticas que miran el usuario, como `matchConditions` o una
ValidatingAdmissionPolicy que revisa `request.userInfo`.

## Reproducirlo

No hace falta Istio para verlo. Un webhook mutante es un servidor HTTPS que
recibe un `AdmissionReview` y responde con un parche. Este, en Python, añade
un contenedor llamado `sidecar` y anota quién le mandó cada pod:

```python
def do_POST(self):
    review = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
    req = review['request']
    print(req['operation'], req['kind']['kind'], req['userInfo']['username'], flush=True)
    patch = [{"op": "add", "path": "/spec/containers/-",
              "value": {"name": "sidecar", "image": "envoy:v1"}}]
    # ... responder con allowed: true, patchType: JSONPatch y el parche en base64
```

Se registra con una `MutatingWebhookConfiguration` para `CREATE` de pods en
los namespaces con la etiqueta `inject=enabled`, con `failurePolicy: Fail`.
Después, un pod suelto y un Deployment de dos réplicas:

```bash
kubectl create namespace demo
kubectl label namespace demo inject=enabled
kubectl -n demo run solo --image=nginx
kubectl -n demo create deployment web --image=nginx --replicas=2
```

En 0.2.24 (con `apiserver --no-node`, así que no se ejecuta nada; solo
miramos lo que se guarda):

```text
NAME                   CONTAINERS
solo                   solo,sidecar
web-7957885594-p72ck   nginx
web-7957885594-rbdpb   nginx

el webhook vio:
CREATE Pod system:admin
```

El webhook recibió un pod, el que creamos nosotros. De los dos del
Deployment nunca supo nada.

## El arreglo

Toda escritura que hace el propio servidor pasa ahora por la admisión antes
de llegar al almacén, como el usuario `system:kube-controller-manager`,
igual que en upstream. Va en `admit_internal`, que llaman `create_object`,
`update_object`, `update_status` y los borrados:

```rust
fn admit_internal(&self, rt: &ResourceType, operation: &'static str, ns: &str, name: &str,
                  sub: Option<&str>, old: Option<&Value>, obj: Value) -> Result<Value, ApiError> {
    let user = UserInfo { username: INTERNAL_USER.into(), ..Default::default() }.authenticated();
    let attrs = admission::Attributes { rt, operation, namespace: ns, name, sub, old, dry_run: false, user: Some(&user) };
    if !admission::webhooks_configured(self) {
        admission::validating_policies(self, &attrs, &obj)?;
        return Ok(obj);
    }
    // ...
    tokio::task::block_in_place(|| handle.block_on(admission::admit(&me, &attrs, obj)))
}
```

Tres detalles:

- **Sin webhooks, sin coste.** Si no hay ninguna configuración de webhooks,
  solo se comprueban las ValidatingAdmissionPolicies, como ya hacía el
  camino HTTP. Es el caso normal en un cluster de desarrollo, y esas
  escrituras siguen igual de rápidas.
- **Los controladores esperan al webhook.** El código de los controladores
  es síncrono y un webhook es una llamada HTTP. `block_in_place` deja que el
  controlador espere en su hilo sin bloquear el resto del runtime, igual que
  un controlador upstream espera su llamada a la API.
- **Los defaults, después de la mutación.** Un webhook puede añadir un
  contenedor sin `imagePullPolicy` ni `terminationMessagePath`. Los defaults
  del pod se aplican otra vez después del parche, como hace la API.

Además, los webhooks reciben ahora el usuario real de cada petición: el tuyo
para lo que creas tú, `system:kube-controller-manager` para lo que crean los
controladores.

La misma demo en 0.2.25:

```text
NAME                   CONTAINERS
solo                   solo,sidecar
web-7957885594-hhd58   nginx,sidecar
web-7957885594-mtn7r   nginx,sidecar

el webhook vio:
CREATE Pod system:admin
CREATE Pod system:kube-controller-manager
CREATE Pod system:kube-controller-manager
```

## Cuando el webhook se cae

Mandar las escrituras de los controladores a un servidor externo crea una
forma nueva de fallar: con `failurePolicy: Fail`, si el webhook no responde,
los pods no se pueden crear. Es lo que pediste (un pod sin su sidecar puede
ser peor que ningún pod), pero tiene que verse.

Antes, el controlador de ReplicaSets solo registraba un evento si la
creación fallaba con un 403 (una cuota, por ejemplo). Cualquier otro error
solo llegaba al log del servidor. Ahora cualquier error que no sea un
conflicto deja un evento `FailedCreate` y la condición `ReplicaFailure` en
el ReplicaSet, como en upstream, y el controlador reintenta a los 5
segundos.

Con el webhook parado, escalamos el Deployment a 4:

```text
$ kubectl -n demo get events --field-selector reason=FailedCreate
COUNT   OBJECT           MESSAGE
4       web-7957885594   Error creating: Internal error occurred: failed calling webhook "sidecar.example.com": failed to call webhook: Post "https://127.0.0.1:17656/mutate?timeout=10s": failed to connect to 127.0.0.1:17656: Connection refused (os error 61)

$ kubectl -n demo get rs -o jsonpath='{.items[0].status.conditions[?(@.type=="ReplicaFailure")].reason}'
FailedCreate

$ kubectl get --raw /readyz
ok
```

El API server sigue sano: solo se paran las escrituras que necesitan el
webhook. Al volver a levantarlo, los dos pods que faltaban salen en el
siguiente reintento, con su sidecar, y la condición desaparece:

```text
NAME                   CONTAINERS
solo                   solo,sidecar
web-7957885594-7htld   nginx,sidecar
web-7957885594-h5rnr   nginx,sidecar
web-7957885594-rpd8f   nginx,sidecar
web-7957885594-v8qwt   nginx,sidecar
```

Si corres un webhook dentro del mismo cluster al que sirve, vale el consejo
de siempre: deja fuera su propio namespace con un `namespaceSelector`. Si
no, cuando su pod muera nadie podrá crear el siguiente.

## Lo que no probamos

Lo probamos con el webhook de arriba, que habla el mismo protocolo
`AdmissionReview` que los inyectores de Istio o Linkerd, y con tests
unitarios: un webhook mutante que añade un sidecar a un pod que crea el
servidor, uno validante que lo rechaza, actualizaciones y borrados que
llegan al webhook, y una ValidatingAdmissionPolicy que rechaza una creación
interna. Todavía no hemos instalado Istio, Linkerd, Kyverno ni Gatekeeper en
litek8s con este cambio. Si lo haces, nos gustaría saber cómo te va.

## En resumen

Un solo proceso es la ventaja de litek8s: no hay red entre la API y sus
controladores. Pero el atajo tiene que conservar todo lo que pasa en el
camino normal. Aquí se saltaba la admisión, que es justo el paso que más se
extiende. Desde 0.2.25, lo que escriben los controladores pasa por los
mismos webhooks mutantes, webhooks validantes y políticas que lo tuyo.
