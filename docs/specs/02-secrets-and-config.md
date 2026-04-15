# Spec 02: Secrets and Config

- Prioridad: P0
- Estado: Draft
- Depende de: `01-security-baseline`
- Bloquea: despliegues reales con credenciales, tokens y llaves

## Contexto

`litek8s` hoy soporta `env` con pares `name/value`, pero no tiene `Secret`, no cifra datos sensibles en reposo y tampoco distingue entre configuración regular y material secreto.

## Problema

Sin una capa de secrets:

- credenciales terminan en YAML, shell history o estado persistido en claro;
- no hay rotación ni referencias por nombre;
- el futuro `VPN pack` y cualquier integración segura necesitarán llaves que no conviene manejar como strings planas.

## Objetivos

- Introducir un recurso `Secret` usable desde `Deployment`.
- Evitar persistir secretos en claro dentro del estado operativo general.
- Permitir inyección por variables de entorno en la primera fase.
- Diseñar la base para mounts y rotación sin exigirlo todo en v1.

## No objetivos

- No integrar KMS externo en la primera fase.
- No resolver secretos CSI ni proveedores cloud nativos.
- No implementar un sistema de templates complejo.

## UX propuesta

### Recurso `Secret`

```yaml
apiVersion: litek8s.dev/v1alpha1
kind: Secret
metadata:
  name: demo-secrets
type: Opaque
stringData:
  API_KEY: super-secret
  DB_URL: postgres://...
```

### Referencia desde `Deployment`

```yaml
env:
  - name: API_KEY
    valueFrom:
      secretKeyRef:
        name: demo-secrets
        key: API_KEY
```

### CLI futura

- `litek8s get secrets`
- `litek8s describe secret demo-secrets`
- `litek8s delete secret demo-secrets`

En la primera fase, `describe secret` no debe imprimir valores; solo metadata y keys.

## Diseño de persistencia

### Requisitos

- Los secretos no deben quedar mezclados en claro dentro de `state.json`.
- El control plane debe poder replicar la metadata necesaria sin exponer contenido accidentalmente en vistas o eventos.

### Propuesta inicial

- Persistir secrets en un almacén separado, por ejemplo `secrets.json.enc`.
- Cifrado envelope con una clave local del cluster almacenada en disco con permisos restringidos.
- El estado principal conserva solo referencias, checksums y metadata no sensible.

### Fase posterior

- soporte para `age`, `sops` o KMS externo
- rotación de key encryption key

## Cambios de manifiesto

### Fase 1

- `kind: Secret`
- `type: Opaque`
- `stringData`
- `env[].valueFrom.secretKeyRef`

### Fase 2

- mounts de secretos en archivos temporales
- `optional: true`
- `envFrom`

### Fase 3

- `ConfigMap` como recurso complementario
- reload o restart controlado cuando cambia un secret

## Cambios técnicos esperados

Archivos probables:

- `src/manifest.rs`
- `src/store.rs`
- `src/protocol.rs`
- `src/daemon.rs`
- `src/runtime/docker.rs`

Cambios:

- parsear `Secret`
- persistir metadata y material cifrado
- resolver referencias antes de crear contenedor
- evitar fugas en logs, errores y `describe`

## Integración con seguridad

- Los secretos en variables de entorno siguen siendo visibles para procesos con privilegios suficientes en el host; por eso esta spec depende del baseline de seguridad.
- Los mounts de secretos deberían escribirse en `tmpfs` cuando estén disponibles.

## Riesgos

- Mala UX si el cifrado en reposo es demasiado complejo de operar.
- Riesgo de filtrar secretos en mensajes de error o eventos si no se filtran bien.
- La replicación de control plane necesita una decisión clara sobre si replicar ciphertext o solo metadata.

## Decisiones abiertas

- ¿El cifrado se hace por nodo o por cluster?
- ¿El material cifrado forma parte del state replicado o va por un almacén separado?
- ¿`ConfigMap` debe salir junto con `Secret` o en una fase aparte?

## Definition of Done

- Existe `kind: Secret` con parsing y validación.
- Un `Deployment` puede referenciar `secretKeyRef` en `env`.
- Los valores de secrets no se muestran en `describe`, `events` ni errores comunes.
- Existe persistencia cifrada en reposo para el material secreto.
- Hay al menos una prueba de punta a punta de inyección en contenedor.

## Próximo paso recomendado

Resolver el modelo de almacenamiento cifrado y la forma mínima de `secretKeyRef` para arrancar por la fase 1.
