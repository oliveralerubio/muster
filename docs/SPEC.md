## Problem Statement

Quien trabaja con agentes de IA de terminal (Claude Code, Codex, OpenCode y otros) en varias máquinas (laptop, sobremesa, VPS, WSL) tiene que mantener a mano la misma configuración en todas partes: instrucciones (AGENTS.md / CLAUDE.md), subagentes, hooks, servidores MCP, permisos y modelos. Además tiene que mover API keys entre máquinas sin filtrarlas. Hoy eso se resuelve con scripts caseros, symlinks o repos de dotfiles que se rompen al añadir una segunda plataforma, un tercer agente, al rotar una clave o al sumar a otra persona.

Los equipos tienen el mismo problema, multiplicado: cada desarrollador configura sus agentes a su manera, las claves de la empresa se pasan por chat y, cuando alguien se va, nadie sabe qué claves tenía ni en qué máquinas.

Muster ya resuelve esto para su autor, pero depende de servicios externos obligatorios: Bitwarden Secrets Manager para los secretos, chezmoi para aplicar archivos, y Tailscale más un model hub propio. Además está lleno de valores personales. No sirve a nadie más y, en la práctica, es un envoltorio de Bitwarden.

Las herramientas existentes cubren piezas sueltas, pero ninguna junta **máquinas + agentes + secretos**:
- rulesync y Ruler convierten reglas entre agentes, pero solo en una máquina.
- cc-switch gestiona agentes en una máquina.
- chezmoi gestiona dotfiles.
- Los gestores de secretos exigen cuenta y servidor.

## Solution

Muster pasa a ser una herramienta **independiente y local-first** que sincroniza tres cosas entre las máquinas de una persona o de un equipo:

1. **Máquinas:** inventario de la flota, alta de una máquina nueva en un comando, estado y detección de desajustes.
2. **Agentes:** una sola fuente de verdad para la configuración de agentes, que se traduce al formato nativo de cada herramienta sin pisar lo que el usuario edita a mano.
3. **Secretos:** una bóveda propia cifrada que vive en el repositorio Git del usuario o del equipo. Cada máquina tiene su propia llave privada, que nunca sale de ella. No hace falta cuenta, servidor ni pago a terceros. Bitwarden, 1Password, Doppler e Infisical quedan como adaptadores opcionales.

La herramienta imita a propósito los patrones que ya funcionan en el ecosistema y los implementa con código propio, sin depender de esas herramientas en tiempo de ejecución:

| Patrón | Se imita de | Qué aporta |
|---|---|---|
| Formato de cifrado | age | Formato de archivo estándar y revisado |
| Estado deseado → plan → diff → apply | chezmoi | Ver los cambios antes de aplicarlos |
| Fuente única de reglas traducida a cada agente | rulesync | Una sola configuración para varios agentes |
| Generaciones con rollback | Nix | Volver atrás si algo se rompe |
| Ejecución sin agente, idempotente y con dry-run | pyinfra | Repetir sin efectos inesperados |
| `doctor` legible por máquinas | brew doctor | Diagnóstico que también entiende un agente |
| Versiones fijas verificadas para instalar los CLIs | mise | Mismas versiones en todas las máquinas |

La única dependencia externa obligatoria es Git.

Muster se usa por tres vías equivalentes, todas apoyadas en el mismo núcleo:
- **CLI completa:** todo se puede hacer al 100 % desde terminal.
- **Interfaz web de administración:** se sirve en local con `muster ui`. Es una interfaz visual cuidada para gestionar máquinas, miembros, grupos, secretos, agentes, altas pendientes y el historial.
- **Interfaz para agentes:** salida JSON estable, códigos de salida fijos, modo no interactivo y un servidor MCP para que los agentes de IA puedan consultar y operar Muster con permisos limitados.

El mismo producto sirve a dos públicos:
- **Modo individual:** gratis y open source. Una persona y sus máquinas, sobre su propio repo Git.
- **Modo equipo:** un repo Git del equipo con miembros, grupos y permisos, administrado desde la interfaz web o la CLI. Un hub alojado de pago queda fuera de esta spec.

## User Stories

### Primeros pasos e instalación
1. Como dev individual, quiero instalar Muster con un solo comando verificable, para empezar sin instalar otras herramientas.
2. Como dev individual, quiero que la instalación no exija cuentas de Bitwarden, Tailscale ni ningún servicio externo, para usarlo gratis y sin registro.
3. Como dev individual, quiero que `muster init` cree mi repo de configuración, la identidad de esta máquina y una bóveda vacía, para tener todo listo en un paso.
4. Como dev individual, quiero que `init` detecte qué agentes tengo instalados y me ofrezca importar su configuración actual, para no empezar de cero.
5. Como dev individual, quiero que `init` me muestre una sola vez una clave de recuperación offline y me pida confirmar que la guardé, para no perder mis secretos si pierdo todas mis máquinas.
6. Como usuario actual de Muster, quiero importar mi configuración y mis secretos desde Bitwarden Secrets Manager a la bóveda nativa, para dejar de depender de Bitwarden.
7. Como usuario de chezmoi, quiero importar mi repo de chezmoi existente, para migrar sin reescribir mis dotfiles.
8. Como usuario de Cuartel (la herramienta personal de la que nace Muster), quiero un comando `muster import cuartel` que traiga mi configuración existente, para no empezar de cero.
9. Como usuario, quiero desinstalar Muster y que mis archivos de agentes queden en su último estado válido, para no quedarme atrapado.

### Máquinas y flota
10. Como dev individual, quiero dar de alta una máquina nueva con un comando que genere su identidad y una solicitud de alta, para sumarla a mi flota en minutos.
11. Como dev individual, quiero aprobar la solicitud de alta desde otra máquina ya autorizada, para que ninguna máquina se autorice sola.
12. Como dev individual, quiero aprobar un alta usando mi clave de recuperación cuando no tengo otra máquina a mano, para no quedar bloqueado.
13. Como dev individual, quiero ver la lista de mis máquinas con su plataforma, agentes, versión de Muster y última sincronización, para saber el estado de mi flota.
14. Como dev individual, quiero asignar a cada máquina un perfil o rol (por ejemplo "trabajo" o "servidor"), para que cada una reciba solo los agentes y secretos que necesita.
15. Como dev individual, quiero revocar una máquina perdida o robada, para que no pueda leer secretos nuevos.
16. Como dev individual, quiero que al revocar una máquina Muster me liste qué secretos tenía y me ofrezca rotarlos, porque revocar no borra lo que ya descargó.
17. Como dev individual, quiero que la sincronización se ejecute sola de forma periódica en Linux, WSL y macOS, para no acordarme de hacerlo.
18. Como dev individual, quiero detectar cuándo una máquina se desajusta del estado deseado y ver exactamente qué difiere, para corregirlo antes de que me dé problemas.

### Agentes y configuración
19. Como dev individual, quiero escribir mis instrucciones una sola vez en AGENTS.md y que Muster las haga llegar a Claude Code, Codex y OpenCode en su formato nativo, para no mantener copias.
20. Como dev individual, quiero declarar mis servidores MCP una sola vez y que aparezcan en cada agente y en cada máquina, para no copiarlos a mano.
21. Como dev individual, quiero que los servidores MCP reciban sus claves desde la bóveda en el momento de aplicar, para que ningún token quede en texto plano en el repo.
22. Como dev individual, quiero sincronizar subagentes, skills, hooks, permisos y elección de modelo, para tener el mismo entorno en todas partes.
23. Como dev individual, quiero que Muster solo modifique las claves que gestiona y preserve todo lo demás de cada archivo, para que mis ediciones manuales y las que hacen los propios agentes no se pierdan.
24. Como dev individual, quiero que, si un agente tiene una opción que otro no entiende, Muster la aplique solo donde corresponde y me avise de lo que no pudo traducir, para no perder funciones en silencio.
25. Como dev individual, quiero que un fallo en el adaptador de un agente no impida sincronizar los demás, para que una actualización de Codex no me deje sin Claude.
26. Como dev individual, quiero ver un plan con el diff exacto antes de aplicar cualquier cambio, para saber qué va a pasar.
27. Como dev individual, quiero aprobar de forma explícita cualquier hook o servidor MCP nuevo antes de que se instale, porque son código que se ejecuta en mi máquina.
28. Como dev individual, quiero volver a la generación anterior con un comando si una sincronización rompió algo, para recuperarme en segundos.
29. Como dev individual, quiero configuración por proyecto además de la global, para que cada repo tenga sus reglas.
30. Como dev individual, quiero instalar o actualizar los CLIs de los agentes a versiones fijas y verificadas, para tener las mismas versiones en todas mis máquinas.

### Secretos
31. Como dev individual, quiero añadir, listar, actualizar y borrar secretos desde la CLI, para gestionar mis API keys sin herramientas externas.
32. Como dev individual, quiero que el valor de un secreto nunca aparezca en logs, diffs, planes, salida JSON ni historial de Git, para no filtrarlo por accidente.
33. Como dev individual, quiero que los secretos se descifren en cada máquina con su propia llave privada, que nunca sale de ella, para no depender de ningún servidor.
34. Como dev individual, quiero leer mis secretos sin conexión a internet, para trabajar en cualquier sitio.
35. Como dev individual, quiero rotar un secreto y que la nueva versión llegue a todas las máquinas autorizadas en la siguiente sincronización, para responder rápido a una filtración.
36. Como dev individual, quiero ver los metadatos de cada secreto (quién lo creó, cuándo cambió, quién puede leerlo) sin ver el valor, para auditar sin exponerlo.
37. Como dev individual, quiero que, si mi repo de configuración llegara a ser público, los secretos sigan siendo ilegibles, para que un error de visibilidad no sea un desastre.
38. Como dev que ya paga 1Password, Bitwarden, Doppler o Infisical, quiero usarlos como proveedor de secretos en lugar de la bóveda nativa, para no duplicar.
39. Como dev individual, quiero que `doctor` compruebe si mis API keys son válidas, si caducaron o si se acercan a su límite, cuando el proveedor lo permita, para no descubrirlo en mitad de una tarea.

### Equipos
40. Como administrador de equipo, quiero crear un repo de equipo con miembros, grupos y secretos compartidos, para estandarizar los agentes de todos.
41. Como administrador de equipo, quiero invitar a un miembro y que su alta quede pendiente hasta que yo la apruebe, para controlar quién accede.
42. Como miembro de equipo, quiero que mis máquinas personales queden asociadas a mi identidad de miembro, para que al revocarme se revoquen todas.
43. Como administrador de equipo, quiero agrupar secretos en grupos con su propia lista de lectores (por ejemplo "backend" o "prod"), para que cada persona solo descifre lo que necesita.
44. Como administrador de equipo, quiero distribuir a todo el equipo un perfil aprobado de agentes (instrucciones, MCP, permisos, modelos), para que todos trabajen con las mismas reglas.
45. Como miembro de equipo, quiero añadir mis capas personales encima del perfil del equipo sin romperlo, para conservar mis preferencias.
46. Como administrador de equipo, quiero ver qué miembros y máquinas están desajustados respecto al perfil aprobado, para corregirlo.
47. Como administrador de equipo, quiero revocar a un miembro que se va, con una lista de los secretos que debo rotar, para cerrar su acceso de forma completa.
48. Como administrador de equipo, quiero que los cambios en miembros, grupos, permisos y perfiles requieran mi aprobación, para que nadie se dé acceso a sí mismo.
49. Como administrador de equipo, quiero un historial de quién hizo cada cambio administrativo y cuándo, para tener trazabilidad.
50. Como administrador de equipo, quiero que varios administradores puedan aprobar cambios, para no ser un punto único de fallo.

### Interfaz web de administración
51. Como administrador, quiero abrir una interfaz web con `muster ui`, sin desplegar ningún servidor, para gestionar todo de forma visual.
52. Como administrador, quiero que la interfaz web solo escuche en localhost y exija un token de un solo uso en la URL, para que nadie más en la red pueda usarla.
53. Como administrador, quiero un panel con el estado de máquinas, miembros, desajustes y altas pendientes, para entender la situación de un vistazo.
54. Como administrador, quiero aprobar o rechazar altas de máquinas y miembros desde la interfaz web, para no tener que recordar comandos.
55. Como administrador, quiero crear, editar y rotar secretos y decidir qué grupos los leen desde la interfaz web, revelando un valor solo tras confirmarlo de forma explícita, para gestionarlos cómodamente sin exponerlos.
56. Como administrador, quiero editar perfiles de agentes y servidores MCP desde la interfaz web, con vista previa del diff antes de guardar, para cambiar la configuración con seguridad.
57. Como administrador, quiero ver el historial de cambios y volver a una generación anterior desde la interfaz web, para recuperarme sin terminal.
58. Como administrador, quiero que todo lo que hago en la interfaz web tenga un comando CLI equivalente que me pueda mostrar, para poder automatizarlo después.
59. Como administrador, quiero que la interfaz web funcione sin conexión y sin cargar recursos de CDNs externos, para usarla en entornos aislados.
60. Como administrador, quiero que la interfaz web se vea bien en modo claro y oscuro y sea usable con teclado, para trabajar cómodo.

### Agentes de IA como usuarios
61. Como agente de IA, quiero que cada comando acepte `--format json` y devuelva un objeto estable con versión de esquema, para interpretarlo sin parsear texto.
62. Como agente de IA, quiero códigos de salida estables y documentados, para saber si un fallo es de uso, de configuración, de proveedor o de seguridad.
63. Como agente de IA, quiero que, sin terminal interactiva, Muster nunca se quede esperando una respuesta y falle con un error claro si falta `--yes`, para no colgarme.
64. Como agente de IA, quiero obtener el JSON Schema de la configuración y la lista de comandos, para generar configuración válida a la primera.
65. Como agente de IA, quiero conectarme a un servidor MCP de Muster para consultar el estado, planificar cambios y proponerlos, para operar Muster como herramienta.
66. Como dev, quiero que el servidor MCP no entregue valores de secretos a los agentes salvo que yo lo permita de forma explícita para un secreto concreto, porque un agente puede filtrar lo que lee. `secret_reveal` exige que la referencia esté en `[mcp_server] reveal` **y** que su `allow` incluya al agente que llama, identificado por `clientInfo.name` del `initialize` (contiene "claude" → claude, "codex" → codex, "opencode" → opencode, "antigravity"/"agy" → antigravity; cualquier otro cliente queda denegado).
67. Como dev, quiero que los cambios propuestos por un agente pasen por el mismo plan y la misma aprobación que los míos, para que un agente no aplique cambios peligrosos solo.
68. Como agente de IA, quiero una skill o documento de instrucciones incluido que explique cómo usar Muster, para operarlo sin prueba y error.

### Diagnóstico y confianza
69. Como usuario, quiero que `muster doctor` revise binarios, versiones, permisos de archivos, estado del repo, bóveda, adaptadores y desajustes, y me sugiera cómo arreglar cada problema, para resolver incidencias solo.
70. Como usuario, quiero que `doctor` nunca repare nada destructivo sin mi permiso, para conservar el control.
71. Como usuario, quiero que Muster no envíe ninguna telemetría por defecto, para confiar en la herramienta.
72. Como usuario preocupado por la seguridad, quiero que las versiones publicadas estén firmadas, tengan checksum y un inventario de dependencias, para verificar que no fueron manipuladas.
73. Como usuario, quiero que los archivos con material sensible tengan permisos restringidos y que `doctor` avise si no los tienen, para evitar fugas locales.

## Implementation Decisions

### Producto y alcance
- Producto open source con licencia **Apache-2.0**. El mismo núcleo sirve al modo individual y al modo equipo. Un hub alojado de pago se especificará más adelante sobre el mismo modelo de datos.
- **Dependencia externa obligatoria: solo Git.** chezmoi, Bitwarden (`bws`/`bw`), Tailscale y el model hub dejan de ser obligatorios.
- **Agentes soportados de forma completa en v1: Claude Code, Codex, OpenCode y Antigravity CLI.** Pi y Hermes, como adaptadores experimentales.
- **Plataformas:** Linux, WSL2 y macOS. Windows nativo no entra.
- Lenguaje: **Rust** en todo lo que Rust permita (CLI, núcleo, bóveda, adaptadores, servidor MCP y servidor de la interfaz web). La interfaz web es HTML, CSS y JavaScript estáticos incrustados en el binario. Se distribuye como un único binario sin runtime. Muster es un repositorio nuevo escrito desde cero; Cuartel (privado) solo sirve como referencia de diseño y no se copia su historial.

### Modelo de dominio
| Concepto | Definición |
|---|---|
| **Repositorio de configuración** | Repo Git del individuo o del equipo. Contiene el manifiesto, las capas de configuración de agentes, los archivos cifrados de la bóveda y las solicitudes de alta. No contiene llaves privadas ni estado local. |
| **Manifiesto** (`muster.toml`) | Único formato canónico y versionado. Declara máquinas, miembros, grupos, perfiles, agentes, servidores MCP, referencias a secretos y transporte. Validación estricta, con espacio de extensión con prefijo `x-`. |
| **Identidad** | Par de llaves por máquina (X25519, formato age). La llave privada vive solo en el directorio de estado local de esa máquina, con permisos 0600. La pública se registra en el manifiesto. |
| **Miembro** | Persona con una o varias máquinas. En modo individual hay un único miembro implícito. |
| **Grupo de secretos** | Conjunto de secretos cifrado como una unidad para su propia lista de destinatarios (miembros, grupos o máquinas). Hay un archivo cifrado por grupo, de modo que quien no es destinatario no puede descifrarlo. |
| **Clave de recuperación** | Identidad offline generada en `init`. Es destinataria de todos los grupos. Se muestra una sola vez y no se guarda en ninguna máquina. |
| **Perfil** | Capa de configuración de agentes que se asigna a máquinas o miembros. El orden de precedencia es fijo y documentado: equipo → grupo → miembro → máquina → proyecto. |
| **Generación** | Instantánea numerada del estado aplicado en una máquina, para hacer rollback. |
| **Solicitud de alta** | Archivo con la llave pública y los metadatos de una máquina o miembro nuevo. Queda pendiente hasta que un aprobador autorizado la acepta. |

### Módulos (interfaces pequeñas y profundas)
- **Core**
  - Carga y valida el manifiesto.
  - Resuelve la identidad de la máquina, con selector explícito antes que autodetección.
  - Combina las capas y construye un **plan determinista**.
  - Muestra el diff, aplica, registra la generación y produce diagnósticos.
  - No conoce rutas de agentes ni detalles de proveedores.
- **Capa de comandos**
  - Cada operación (init, enroll, approve, revoke, secret, profile, plan, sync, rollback, doctor, status, member, group) es una función con entrada y salida estructuradas.
  - La CLI, la API de la interfaz web y el servidor MCP son adaptadores finos sobre esta misma capa. Así se garantiza que todo lo de la interfaz web y del MCP existe en la CLI.
- **Bóveda nativa**
  - Formato de archivo age, implementado con el crate `age` (rage) compilado en el binario, sin binario externo. **No se inventa criptografía.**
  - Operaciones: añadir, rotar, eliminar, listar metadatos, re-cifrar para un nuevo conjunto de destinatarios y resolver una referencia.
  - Los valores descifrados se mantienen en memoria el menor tiempo posible y se ocultan en repr, logs y trazas.
- **Proveedores de secretos**
  - Interfaz común: `describe`, `check`, `resolve`, `close`.
  - Errores tipados: no disponible, no autorizado, no encontrado, límite alcanzado, referencia inválida.
  - Proveedores incluidos: nativo (por defecto) y `env`. Bitwarden, 1Password, Doppler e Infisical como adaptadores opcionales.
  - En el manifiesto se guardan **referencias** a secretos (`native://grupo/nombre`, `env://VAR`, `op://…`, `bws://…`), nunca valores.
  - **Aislamiento por agente (denegado por defecto):** cada secreto declara qué agentes pueden recibirlo en `[secrets."<referencia>"] allow = ["claude", "codex@mi-repo"]` del `muster.toml` del repo de configuración (o con `muster secret add <ruta> --allow <agente>`). `agente` concede en todas partes; `agente@proyecto` solo al aplicar ese proyecto (`--project DIR`, cuyo nombre de carpeta es `proyecto`). Un secreto sin `allow` no llega a ningún agente. Al generar la configuración de un agente, una referencia no permitida no se escribe (ni el valor ni la referencia): se omite esa clave de `env`/`headers` y se emite el aviso `secret_not_allowed_for_agent` con el agente y la referencia, nunca el valor; el resto de la configuración se aplica. El `muster.toml` de un proyecto no puede concederse secretos. La persona, desde la CLI (`muster secret get --reveal`), siempre puede leerlos. `muster secret list` muestra la lista `allow` de cada secreto.
- **Adaptadores de agente**
  - Uno por herramienta. Cada uno declara qué archivos y alcances gestiona, qué capacidades soporta (instrucciones, MCP, subagentes, skills, hooks, permisos, modelos), plataformas y versión mínima.
  - Traducen la configuración canónica al formato nativo.
  - **Solo escriben las claves que gestionan** y preservan el resto del archivo (TOML y JSON con merge controlado).
  - Informan de lo que no pueden traducir.
  - Un fallo queda aislado a ese agente.
  - AGENTS.md es el formato canónico de instrucciones. CLAUDE.md se genera o enlaza a partir de él.
  - **Skills** (`[agents.skills] source = "skills"`, una carpeta por skill con `SKILL.md` y archivos extra): los cuatro agentes las soportan. Muster copia cada archivo tal cual; las skills que se quitan de la fuente se borran en el siguiente apply y las que el usuario añade a mano no se tocan. Una skill sin `SKILL.md` o sin `name`/`description` en el frontmatter se omite con el aviso `skill_invalid`. Rutas:

    | Agente | Usuario | Proyecto |
    |---|---|---|
    | Claude Code | `~/.claude/skills/<skill>/` | `.claude/skills/<skill>/` |
    | Codex | `~/.agents/skills/<skill>/` | `.agents/skills/<skill>/` |
    | OpenCode | `$XDG_CONFIG_HOME/opencode/skills/<skill>/` | `.opencode/skills/<skill>/` |
    | Antigravity CLI | `~/.gemini/antigravity-cli/skills/<skill>/` | `.agents/skills/<skill>/` |

    Codex y Antigravity comparten `.agents/skills` en proyecto; el archivo se escribe una sola vez.
  - **Subagentes** (`[agents.subagents] source = "subagents"`, una carpeta del repo de configuración con un archivo Markdown por subagente, `<source>/<nombre>.md`, frontmatter YAML con `name` y `description` obligatorios y `tools`/`model` opcionales; el cuerpo es el system prompt): los cuatro agentes los soportan. El frontmatter se parsea como YAML (block scalars, cadenas entrecomilladas y `tools` en lista funcionan); `name` debe cumplir `[a-z0-9][a-z0-9._-]*` (sin `/`, `\\`, `..` ni punto inicial) e igual al nombre del archivo. Muster instala el archivo tal cual donde el formato nativo es el canónico (Claude Code, Antigravity CLI) y lo traduce donde difiere (Codex lo pide como TOML con `developer_instructions`; OpenCode exige `mode: subagent` y toma el nombre del archivo); el frontmatter generado se serializa como YAML/TOML válido. Los campos sin equivalente (`tools` y cualquier clave no canónica, en Codex y OpenCode) se omiten con el aviso `subagent_field_untranslated`; un subagente sin frontmatter válido (campos ausentes o YAML malformado) se omite con `subagent_invalid`. Quitar un subagente de la fuente lo borra en el siguiente apply y los archivos que el usuario añade a mano no se tocan. Rutas:

    | Agente | Usuario | Proyecto |
    |---|---|---|
    | Claude Code | `~/.claude/agents/<name>.md` | `.claude/agents/<name>.md` |
    | Codex | `~/.codex/agents/<name>.toml` | `.codex/agents/<name>.toml` |
    | OpenCode | `$XDG_CONFIG_HOME/opencode/agents/<name>.md` | `.opencode/agents/<name>.md` |
    | Antigravity CLI | `~/.gemini/antigravity-cli/agents/<name>.md` | `.agents/agents/<name>.md` |
  - **Ciclo de vida de skills** (`muster skill`): skills propias (en el repo de configuración, bajo `[agents.skills] source`) y suscritas desde repositorios git externos (`[[agents.skills.remote]]` con `name`, `url`, `ref`, `path`, `include` opcionales). Las externas se copian (vendoring) en la carpeta de skills, así que el motor y los adaptadores no cambian y todas las máquinas las reciben por la sincronización git normal.
    - `skills.lock` (TOML, junto a `muster.toml`, versionado): por skill `name`, `origin` (`local` o id de la fuente), `commit` (externas), `content_hash` (`sha256:` sobre rutas relativas ordenadas y contenido normalizado a LF), `version` (frontmatter de `SKILL.md`) y `updated_at` (RFC 3339).
    - Estados: `local`, `up_to_date`, `update_available`, `deprecated` (frontmatter upstream `deprecated: true` o `deprecated: "<motivo>"`; gana a `update_available`), `upstream_removed`, `unknown` (sin comprobar o fuente inalcanzable; se conserva el último estado conocido). La última comprobación se guarda por máquina en `<state>/skills-status.json` (no versionado).
    - Comandos: `skill list` (sin red), `skill add <nombre>` / `skill add --url`, `skill check [nombre]`, `skill update <nombre>|--all [--force] [--dry-run]` (resumen de archivos añadidos/cambiados/eliminados), `skill rm <nombre> [--detach]`. `update` y `rm` piden confirmación (`--yes`). Nunca se aplica una actualización ni se borra una skill sin orden explícita; una skill editada a mano aparece como `modified` y `update` se niega sin `--force`.
    - En modo equipo solo los administradores añaden, actualizan o quitan skills (commits con `Muster-Actor`); los miembros pueden listar y comprobar.
    - Panel web: pestaña Skills con estado, botón para comprobar repositorios, actualizar (con resumen de archivos), quitar y, para `upstream_removed`, conservar como propia o eliminar; un punto en la pestaña avisa cuando algo requiere atención.
  - La traducción de formatos imita el modelo de rulesync, pero se implementa con código propio.
- **Motor de aplicación**
  - Reemplaza a chezmoi: estado deseado → plan → diff → apply idempotente.
  - Registra quién es dueño de cada archivo o clave gestionada.
  - Hace copia de seguridad antes de sobrescribir.
  - Crea una generación por cada apply.
  - Ofrece `rollback` a una generación concreta.
  - Incluye un importador de repos chezmoi existentes.
- **Instalador de agentes**
  - Instala y actualiza los CLIs de los agentes a versiones fijas, con checksum verificado, imitando el modelo de lockfile de mise.
  - Reutiliza el enfoque actual de instalación con verificación de hash.
- **Transporte de flota**
  - Git es el transporte principal: el usuario elige el remoto.
  - SSH se conserva para el alta asistida y para la sincronización entre máquinas existente, como transporte opcional.
  - Nunca se transportan secretos en claro.
- **Planificador**
  - Sincronización periódica con systemd en Linux/WSL, launchd en macOS y fallback manual.
- **Doctor**
  - Revisa binarios, versiones, permisos de archivos sensibles, estado del repo, bóveda, adaptadores, desajustes y validez de claves cuando el proveedor lo permite.
  - `doctor --check-keys` consulta de forma opcional la validez, caducidad y límite de gasto de `OPENROUTER_API_KEY`; `--offline` omite esta consulta. Sin la opción no se consultan APIs de claves. Alcance y auditoría de red: [docs/security/network.md](security/network.md).
  - Cada problema trae una solución sugerida.
  - Nunca hace reparaciones destructivas sin confirmación.
- **Interfaz web de administración**
  - Aplicación web incrustada en el binario y servida con `muster ui`.
  - Escucha solo en localhost, en un puerto aleatorio, con token de un solo uso.
  - Sin recursos de CDN.
  - Pantallas: panel general, máquinas, miembros, grupos y secretos, perfiles de agentes y MCP (con vista previa del diff), altas pendientes, historial y generaciones, y doctor.
  - Cada acción muestra su comando CLI equivalente.
  - La revelación de valores de secretos exige confirmación explícita y queda registrada.
  - Tema claro y oscuro, y uso completo con teclado.
- **Servidor MCP** (`muster mcp`)
  - Expone estado, plan, doctor, listado de metadatos y propuesta de cambios.
  - Las propuestas pasan por plan y aprobación.
  - No devuelve valores de secretos salvo que esté permitido de forma explícita para cada secreto.

### Flujos clave
- **Alta de máquina**
  1. La máquina nueva genera su identidad y escribe una solicitud de alta en el repo.
  2. Una máquina autorizada, un administrador o la clave de recuperación la aprueba.
  3. Se re-cifran los grupos a los que corresponde.
  4. La máquina nueva sincroniza.
- **Revocación**
  1. Se retira la llave pública de los destinatarios y se re-cifran los grupos afectados.
  2. Se lista qué secretos tuvo a su alcance y se ofrece rotarlos.
  3. Se documenta que revocar no recupera lo que ya se descargó.
- **Gobierno en equipos**
  - Los cambios en miembros, grupos, destinatarios y perfiles de equipo requieren la firma o aprobación de un administrador.
  - Las aprobaciones se registran en el historial de Git del repo de configuración.
  - **Quórum de aprobaciones:** `[team] required_approvals = N` en el manifiesto (entero ≥ 1, por defecto 1; 0 es error de configuración). Se aplica a `approve` (alta de máquina) y `member approve` (alta de miembro). El administrador que aprueba primero cuenta como la aprobación 1. Por debajo del quórum la solicitud queda pendiente y cada aprobación se guarda con el nombre del administrador en el repo de configuración (en la solicitud de alta o en el miembro pendiente), para que otro administrador la complete desde otra máquina. Un mismo administrador no cuenta dos veces (`already_approved_by_caller`, código de salida 5). Al llegar al quórum se ejecuta la aprobación normal (re-cifrado y alta). La clave de recuperación es una vía de emergencia y cuenta como aprobación completa sea cual sea N.
  - En v1 la protección de ramas o revisión del remoto Git es la barrera recomendada. El servidor de autorización llega con el hub alojado.
- **Migración desde el Muster actual**
  - `muster import bws` lee los secretos del proyecto de Bitwarden y los guarda en la bóveda nativa.
  - El importador de chezmoi trae las plantillas.
  - Todos los valores personales (IDs, hosts, IP del hub, repos propios, carpeta personal, proveedores de Hermes, origen de auto-update fijo) pasan al repo de configuración del usuario y salen del código del producto.

### Contrato de la CLI
Comandos de primer nivel:

| Comando | Para qué |
|---|---|
| `init` | Crear repo, identidad y bóveda |
| `import` | Traer configuración desde BWS o chezmoi |
| `enroll` / `approve` / `reject` / `revoke` | Altas y bajas de máquinas (`reject` descarta una solicitud de alta pendiente, sin revocar) |
| `machine` | Gestionar máquinas |
| `member` / `group` | Gestionar miembros y grupos (equipos) |
| `secret` | Gestionar secretos |
| `profile` | Gestionar perfiles de agentes |
| `agents` | Listar agentes y adaptadores |
| `plan` / `sync` | Ver y aplicar cambios |
| `rollback` | Volver a una generación anterior |
| `status` | Estado de la flota |
| `doctor` | Diagnóstico |
| `ui` | Abrir la interfaz web |
| `mcp` | Arrancar el servidor MCP |
| `schema` | Exportar el JSON Schema |

Opciones globales: `--config`, `--format text|json`, `--yes`, `--dry-run` (en todo lo que modifique algo), `--quiet` y `--verbose`.

- **Precedencia de valores:** opciones de CLI > variables `MUSTER_*` > manifiesto > valores por defecto.
- **Salida JSON:** un único objeto en stdout con `schema_version`, `command`, `ok`, `result` y `diagnostics`. Cada diagnóstico tiene `code`, `message`, `severity` y `path` opcional. El progreso va a stderr.
- **Códigos de salida:**

| Código | Significado |
|---:|---|
| 0 | OK (también un dry-run válido) |
| 1 | Error operativo |
| 2 | Uso inválido |
| 3 | Configuración inválida |
| 4 | Proveedor o dependencia no disponible |
| 5 | Fallo de seguridad o permisos |
| 130 | Interrumpido |

- **Sin TTY:** no hay prompts; las acciones que requieren confirmación fallan con código 2 si no se pasa `--yes`.
- **Estado local:** vive en el directorio XDG de estado (o su equivalente en macOS), separado del repo de configuración.

## Testing Decisions

- **Un buen test prueba comportamiento observable, no la implementación.**
  - Qué se observa: código de salida, salida JSON y estado de los archivos en un `$HOME` aislado.
  - Los tests nuevos no parchean funciones internas.
- **Costura principal (una sola): la CLI.**
  - Se invoca `muster` con un `$HOME` y un directorio XDG temporales, y repos Git locales temporales como remotos.
  - Se prueban flujos completos:
    - init → enroll → approve → sync en una segunda "máquina" simulada con otro `$HOME`;
    - revocar e intentar descifrar;
    - rotar y propagar;
    - plan, sync y rollback;
    - preservación de ediciones manuales;
    - fallo aislado de un adaptador;
    - comportamiento sin TTY;
    - códigos de salida y forma del JSON.
- **La criptografía se prueba de verdad:** se generan identidades y archivos cifrados reales en el directorio temporal, sin mocks del cifrado.
- **Dependencias externas como ejecutables falsos en un `PATH` temporal.** Aplica a los CLIs de agentes (para el instalador y `doctor`) y a los proveedores opcionales (Bitwarden, 1Password). No se parchea el código.
- **Paridad de la interfaz web y del MCP:** como son adaptadores finos sobre la capa de comandos, un conjunto de tests comprueba que cada acción de la API web y cada herramienta MCP devuelve el mismo resultado que el comando CLI equivalente. Un único test de humo en navegador verifica que la interfaz carga y muestra el panel general.
- **Tests de contrato por adaptador de agente:**
  - Snapshots de los archivos generados en cada PR.
  - Una ejecución periódica contra la última versión de cada CLI, que informa sin bloquear.
- **Seguridad:** tests que siembran secretos de prueba y verifican que no aparecen en stdout, stderr, logs, planes, diffs, historial de Git ni respuestas del MCP. Escaneo de secretos en CI.
- **Herramientas de test:**
  - Tests de integración de Cargo (`cargo test`) que ejecutan el binario `muster` compilado como subproceso.
  - Un helper compartido monta el `$HOME` temporal, los directorios XDG y los ejecutables falsos en `PATH`. Es la base de la costura.

## Out of Scope

- **Hub alojado de pago:** sync en la nube, panel multiusuario hospedado, SSO/SAML, SCIM, facturación y RBAC en servidor. Irá en una spec posterior sobre este mismo modelo.
- Windows nativo (WSL2 sí entra).
- Sincronizar la memoria de los agentes entre máquinas.
- Catálogo o marketplace público de plugins de terceros.
- Soporte completo de Pi y Hermes (quedan como experimentales).
- Cualquier dependencia del model hub personal y de Tailscale. Se mantiene solo como configuración opcional del usuario.
- La transferencia de logins OAuth de los agentes por SSH: queda como experimental y desactivada por defecto.
- Telemetría.
- Prometer control de qué modelos usa un empleado más allá de lo que cada agente permite configurar.

## Further Notes

- Se basa en el informe consolidado de 30 investigaciones (`informe/INFORME-FINAL.md`). Conclusiones que fundamentan esta spec:
  - el hueco "una persona o un equipo, muchas máquinas, muchos agentes, con secretos" está libre;
  - cc-switch (⭐143.8k) es la amenaza principal si añade sync entre máquinas;
  - Packmind es el competidor comercial más directo para equipos.
- **Antes de hacer público el repo:**
  1. Auditar todo el historial de Git en busca de secretos.
  2. Rotar cualquier credencial encontrada.
  3. Reescribir el historial una sola vez.
- **Decisiones abiertas** que no bloquean el inicio:
  - el nombre del producto en inglés (o mantener "Muster" con subtítulo).
- **Datos a verificar** al implementar los adaptadores:
  - reemplazo verificado: Antigravity CLI (`agy`) reemplazó al CLI anterior de Google para niveles de consumo el 2026-06-18 y mantiene rutas bajo `~/.gemini/`;
  - los formatos actuales de configuración de cada agente.
- **Lanzamiento:**
  - Show HN exige que el autor escriba el post y las respuestas sin IA.
  - La métrica de éxito es completar el alta en una segunda máquina y seguir usándolo, no las estrellas.
