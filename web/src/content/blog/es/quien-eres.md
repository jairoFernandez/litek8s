---
title: "¿Quién eres?: usuarios de OIDC, tokens estáticos y un webhook"
description: "Los servidores litek8s solo conocían certificados de su propia CA. 0.2.25 acepta los flags de OIDC, token file y webhook de kube-apiserver."
date: 2026-11-14
tags: [autenticación, oidc, rbac, seguridad]
docs: [auth, limitations]
lang: es
translation: who-are-you
---

Desde 0.2.12, un servidor litek8s autentica cada petición y aplica RBAC.
Pero solo conocía dos tipos de cliente: un certificado firmado por la CA
del cluster (tú, con kubectl) y un token de service account (tus pods).
Para una persona alcanza. Para un equipo es incómodo: cada persona necesita
un certificado firmado con la clave de la CA, no hay forma de revocar uno,
y cuando alguien se va hay que rotar la CA.

Casi todos los equipos ya tienen un sitio donde viven sus usuarios: Google,
Entra ID, Keycloak, Dex, GitHub a través de Dex. 0.2.25 acepta los tres
flags de kube-apiserver para traerlos. Este artículo explica cada uno, en
qué orden se prueba un token, y una prueba contra un emisor OIDC pequeño.

## Tres formas de entrar

| flag | qué es | para qué |
|---|---|---|
| `--token-auth-file` | un CSV de tokens fijos: `token,user,uid,"grupo1,grupo2"` | un runner de CI, un script, un laboratorio |
| `--oidc-issuer-url`, `--oidc-client-id` | ID tokens firmados por un proveedor OpenID Connect | personas, con el login que ya tienen |
| `--authentication-token-webhook-config-file` | un servicio tuyo que responde un `TokenReview` por cada token | tokens que solo tú sabes comprobar |

Los flags, y sus valores por defecto, son los de kube-apiserver, así que
estos flags de una línea de comandos de `kube-apiserver` que ya tengas sirven
sin cambios. Entre
quien entre, RBAC lo trata como a cualquier otro usuario: enlazas su nombre
o sus grupos a un rol.

## En qué orden se prueba un token

Una petición llega con `Authorization: Bearer <token>`. litek8s lo mira en
el mismo orden que kube-apiserver:

```rust
if let Some(user) = tokens.static_user(token) {
    return Ok(Some(user));
}
if token.matches('.').count() == 2 {            // parece un JWT
    if let Some(oidc) = tokens.oidc_for(token) {  // su `iss` es el emisor OIDC
        return oidc.authenticate(token).await.map(Some);
    }
    return match (state.authenticate_service_account(token), tokens.webhook()) {
        (Ok(user), _) => Ok(Some(user)),
        (Err(_), Some(webhook)) => webhook.authenticate(token).await.map(Some),
        (Err(e), None) => Err(e),
    };
}
// después los tokens de join y de agente de los nodos, y el webhook al final
```

Un JWT se reparte por su emisor (`iss`, leído sin fiarse todavía): a un
lado las service accounts de este cluster, al otro el proveedor OIDC. La
comprobación de la firma viene después. Al webhook solo llega un token que
nadie reconoce, así que no recibe una petición por cada pod del cluster.

## Qué comprueba OIDC

Un ID token es un JWT: una cabecera, los claims y una firma. Para fiarse de
él, litek8s:

1. Baja el documento de discovery del emisor
   (`<issuer>/.well-known/openid-configuration`), comprueba que nombra al
   mismo emisor, y baja las claves públicas de su `jwks_uri` (solo HTTPS).
2. Verifica la firma con la clave que nombra el token (`kid`). RS256 por
   defecto; `--oidc-signing-algs` permite RS/PS de 256 a 512, ES256, ES384
   y EdDSA.
3. Comprueba el emisor, la audiencia (tiene que incluir
   `--oidc-client-id`), `exp` y `nbf`, y cada `--oidc-required-claim`.
4. Saca el usuario de `--oidc-username-claim` (por defecto `sub`) y los
   grupos de `--oidc-groups-claim`, con sus prefijos.

Las claves se bajan la primera vez que hacen falta y se guardan. Si un
token nombra un `kid` que no está entre ellas, lo probable es que el
proveedor haya rotado sus claves, así que se vuelven a bajar, como mucho
una vez cada 10 segundos. Ese límite impide que alguien haga que el
servidor golpee al proveedor mandando tokens con `kid`s inventados.

Los prefijos están para evitar choques. Por defecto el usuario es
`<issuer>#<sub>`, así que un `admin` de tu proveedor no es el mismo que un
`admin` local. Con `email` como claim no hay prefijo, y `email_verified` no
puede ser false. Se rechazan los nombres de usuario que empiezan por
`system:`, para que nadie se haga pasar por un nodo o un controlador.

La firma se comprueba con ring, la misma librería que el binario ya usaba
para TLS. No entró ninguna dependencia nueva para esto.

## Probarlo

Un emisor pequeño en Python (unas 50 líneas, con el paquete
`cryptography`) sirve el documento de discovery y una clave RSA, y emite dos
ID tokens para `jane@example.com` en el grupo `dev-team`: uno para la
audiencia `litek8s` y otro para otra aplicación. El mismo servidor responde
`TokenReview`s y acepta un token de CI. El API server:

```bash
litek8s apiserver --no-node \
  --token-auth-file tokens.csv \
  --oidc-issuer-url https://127.0.0.1:17672 --oidc-client-id litek8s \
  --oidc-username-claim email --oidc-groups-claim groups --oidc-groups-prefix oidc: \
  --oidc-ca-file cert.pem \
  --authentication-token-webhook-config-file webhook.yaml
```

La línea de arranque dice qué acepta:

```text
apiserver: listening on https://127.0.0.1:17671 (authorization mode RBAC: service account tokens, client certificates, static tokens (1), OIDC (https://127.0.0.1:17672), token webhook (https://127.0.0.1:17672/tokenreview))
```

Token estático:

```text
$ kubectl --token $ALICE auth whoami
ATTRIBUTE   VALUE
Username    alice
UID         1001
Groups      [ops system:authenticated]
```

OIDC. Jane entra, pero RBAC todavía no la deja hacer nada:

```text
$ kubectl --token $JANE auth whoami
ATTRIBUTE   VALUE
Username    jane@example.com
Groups      [oidc:dev-team system:authenticated]

$ kubectl --token $JANE get pods -n default
Error from server (Forbidden): pods is forbidden: User "jane@example.com" cannot list resource "pods" in API group "" in the namespace "default"
```

Enlazar el grupo, no la persona, es lo que hace que escale: a quien el
proveedor ponga en `dev-team` entra con ese rol.

```text
$ kubectl create clusterrolebinding dev-team-view --clusterrole=view --group=oidc:dev-team
$ kubectl --token $JANE get pods -n default
No resources found in default namespace.
```

El mismo token firmado, emitido para otra aplicación, se rechaza. Un token
que tu proveedor le dio a otra app no debe abrir el cluster:

```text
$ kubectl --token $OTHER auth whoami
error: You must be logged in to the server (Unauthorized)
```

El webhook acepta su token y rechaza uno que no conoce:

```text
$ kubectl --token $CI auth whoami
ATTRIBUTE   VALUE
Username    ci-bot
UID         ci-1
Groups      [ci system:authenticated]

$ kubectl --token $BAD auth whoami
error: You must be logged in to the server (Unauthorized)
```

Y el log del emisor muestra la caché funcionando. Tres tokens OIDC, una
descarga del documento de discovery y una de las claves. Al webhook solo
llegaron los dos tokens que nadie más conocía:

```text
GET /.well-known/openid-configuration
GET /keys
TokenReview for ci-runner-7f...
TokenReview for not-a-real-t...
```

## La caché del webhook

Un webhook es una llamada por red, y kubectl hace varias peticiones por
comando. Las respuestas se guardan por token:

- **las aprobaciones**, durante `--authentication-token-webhook-cache-ttl`
  (2 minutos por defecto, como en upstream);
- **los rechazos**, 10 segundos, para que un script roto no inunde el
  servicio;
- **los errores de transporte** no se guardan: si el servicio no responde,
  la siguiente petición vuelve a probar.

El kubeconfig del webhook dice dónde está el servicio y cómo se autentica
el servidor ante él: una CA, un certificado de cliente, o un `token` o
`tokenFile`.

## Para kubectl

kubectl no hace el login OIDC por sí mismo. Un plugin de credenciales como
[kubelogin](https://github.com/int128/kubelogin) abre el navegador, consigue
el ID token y lo renueva. En el kubeconfig es un usuario con `exec`:

```yaml
users:
- name: jane
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1
      command: kubectl
      args: [oidc-login, get-token, --oidc-issuer-url=https://login.example.com, --oidc-client-id=litek8s]
```

## Lo que todavía no hace

- **`--authentication-config`**: la configuración estructurada que permite
  varios emisores OIDC y mapeos de claims con CEL. Por ahora, un emisor.
- **La API `TokenReview`** sigue comprobando solo tokens de service account.
  Un API server agregado que te mande a comprobar el token OIDC de un
  usuario no lo verá aprobado.
- **Un token del emisor configurado** lo juzga solo OIDC: si falla ahí, no
  pasa al webhook.
- **Proveedores reales.** Lo probamos contra el emisor de arriba y en los
  tests unitarios (un servidor HTTPS real con discovery, claves y
  TokenReview: tokens válidos, la firma de otro token, la audiencia
  equivocada, un `kid` desconocido, la caché del webhook), todavía no contra
  Dex, Keycloak ni Google. Si conectas uno, nos gustaría saber cómo te va.

El detalle de cada flag está en [auth.md](/es/docs/auth/).
