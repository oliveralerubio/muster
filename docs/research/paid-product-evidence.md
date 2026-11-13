# Evidencia de la propuesta de pago de Muster

Fecha de corte: 13 de noviembre de 2026. Este anexo acompaña [la propuesta](paid-product.md). Distingue hechos comprobados, interpretación y experimentos pendientes.

## Método y alcance

Se encargaron 30 investigaciones con frentes distintos: 10 a `gpt-6-luna`, 10 a `claude-haiku-4-5` y 10 a `zai-coding-plan/glm-5.3-flash`. Cada investigador tenía un archivo de salida propio, instrucciones de no modificar el repositorio y prohibición de subdelegar. El primer intento de usar el identificador abreviado de GLM fue rechazado; las diez tareas de ese grupo se iniciaron después con el identificador anunciado por el proveedor. No se sustituyó el modelo solicitado.

Una revisión adicional de Sol 6.1 comprobó el plan una vez. La coordinación contrastó fuentes primarias, corrigió contradicciones y redactó la recomendación. Treinta informes no equivalen a treinta validaciones independientes: muchos consultan las mismas fuentes, comparten premisas y contienen propuestas sin demostrar.

Las 30 investigaciones entregaron un archivo. No quedaron frentes sin entrega; sí quedaron afirmaciones sin evidencia que se excluyen a continuación.

El examen del producto se refiere principalmente a [`f8ebe16faf88c1790f1a54e16bfbd5167fe152ab`](https://github.com/oliveralerubio/muster/tree/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab). El documento se redactó en una rama separada sobre un `origin/main` posterior. No se ejecutó una auditoría de seguridad, un ensayo de carga ni una evaluación de compatibilidad con todos los agentes reales. No se entrevistó a clientes, no se realizó contacto comercial y no se cobró a nadie.

Las notas originales son material de trabajo, no fuentes de autoridad. Este anexo recoge la cobertura y las decisiones de uso. Las afirmaciones comerciales dependen de las fuentes citadas y de los límites expresos; los precios propuestos y las cuentas son hipótesis.

## Hechos que sostienen la propuesta

| Hecho | Evidencia primaria | Límite |
|---|---|---|
| El núcleo permite uso comercial bajo Apache-2.0. | [LICENSE del snapshot](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/LICENSE), secciones 2, 4 y 9; [FAQ de Apache](https://www.apache.org/foundation/license-faq.html). | No acredita titularidad exclusiva ni disponibilidad de una marca. |
| Las funciones de equipo ya forman parte del núcleo. | [README, modo equipo](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/README.md#team-mode); [comandos de miembros](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/commands/member.rs#L32-L100); [grupos](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/commands/group.rs#L30-L95). | La existencia de código no demuestra que una empresa lo haya adoptado. |
| Git transporta configuración y el equipo aplica localmente. | [Sync](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/commands/sync.rs#L131-L179); [transporte Git](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/git.rs#L54-L147). | Un servicio adicional tiene que demostrar valor sobre ese flujo. |
| La interfaz actual es local. | [Servidor web y escucha loopback](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/web/mod.rs#L226-L259). | No es autenticación de una organización ni aislamiento entre clientes de un servicio. |
| El servicio alojado sigue fuera del alcance de la especificación actual. | [SPEC, alcance excluido](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/docs/SPEC.md#L387-L397). | Es intención de producto, no capacidad disponible ni demanda. |
| Hay una release pública. | [v0.1.0](https://github.com/oliveralerubio/muster/releases/tag/v0.1.0); [API de releases](https://api.github.com/repos/oliveralerubio/muster/releases/tags/v0.1.0). Fecha publicada: `2026-11-13T01:57:06Z`. | El README del snapshot tenía información anterior. No se instaló cada artefacto. |
| Volver a cifrar no invalida credenciales ya copiadas. | [Guía de baja y rotación](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/docs/guides/team-mode.md#L205-L260). | Se necesita rotación en el proveedor; no se ha probado una revocación remota universal. |
| Las garantías de firmas y secretos tienen alcance definido. | [ADR de estado firmado](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/docs/adr/0005-signed-fleet-state.md); [verificación](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/docs/security/verification.md); [límites de entrega a agentes](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/docs/agents/muster-for-agents.md#L200-L241). | No son certificación, aislamiento entre procesos del mismo usuario ni auditoría de toda ejecución. |

## Fuentes externas que cambian la decisión

Se consultaron las siguientes fuentes oficiales. Los sitios comerciales cambian; las capacidades se describen al corte de la investigación. No se compararon todos sus productos mediante instalaciones reales.

- [Supabase, self-hosting](https://supabase.com/docs/guides/self-hosting), [precios](https://supabase.com/pricing) y [licencia del monorepo](https://github.com/supabase/supabase/blob/master/LICENSE). Respaldan la distinción entre software disponible y plataforma administrada. No prueban que el mismo ahorro operativo exista en Muster.
- [n8n, licencia](https://github.com/n8n-io/n8n/blob/master/LICENSE.md) y [FAQ de licencia](https://docs.n8n.io/sustainable-use-license/). El uso comercial interno permitido no equivale a permiso irrestricto de reventa. La Sustainable Use License no debe describirse como Apache-2.0.
- [OSI, definición de código abierto](https://opensource.org/osd). Permite distinguir una licencia abierta de una licencia con restricciones sobre campos de uso.
- [Packmind, precios](https://packmind.com/pricing/) y [README oficial](https://github.com/PackmindHub/packmind). Su oferta gratuita ya distribuye instrucciones y su oferta empresarial anuncia gobierno e identidad. No se verificó una tarifa pública para Enterprise ni el alcance de cada promesa mediante una prueba del producto.
- [CC Switch, README oficial](https://github.com/farion1231/cc-switch#readme). La sección de capacidades describe sincronización WebDAV y almacenamiento compatible con S3. Se excluyen como evidencia las afirmaciones de patrocinadores insertadas en ese README.
- [PostHog, precios](https://posthog.com/pricing). Publica unidades de consumo por producto. Se descartó caracterizar su negocio como exclusivamente cobro por asientos.
- [GNU, Apache-2.0 y GPLv3](https://www.gnu.org/licenses/license-list.html#apache2). Contradice una afirmación demasiado amplia de incompatibilidad jurídica aparecida en los apuntes. La compatibilidad de una integración concreta requiere analizar sus componentes y obligaciones.
- [WorkOS, precios](https://workos.com/pricing). El cobro por conexiones de identidad justifica presupuestar ese coste por separado. La reserva de USD 250 del escenario no es una cotización para Muster ni una arquitectura elegida.

La señal pública de Muster se examinó mediante `gh issue list --state all`, `gh pr list --state all`, `gh repo view` y la API de releases. La actividad consultada estaba dominada por el propietario. No existe en esta investigación evidencia suficiente de uso independiente, comprador con presupuesto o renovación. Tampoco existe una estimación defendible del mercado total.

Como evidencia del problema, se contrastaron directamente las solicitudes [Claude Code #22648](https://github.com/anthropics/claude-code/issues/22648), sobre sincronización entre dispositivos, y [#28942](https://github.com/anthropics/claude-code/issues/28942), sobre secretos por desarrollador en configuración MCP compartida. Son relatos de usuarios y solicitudes a un fabricante. No se interpreta la ausencia de pagos en esos hilos como disposición a pagar igual a cero; sencillamente no aportan evidencia de pago por Muster. Tampoco validan el seguimiento de flota propuesto ni un incidente concreto de baja de empleados.

## Correcciones aplicadas antes de recomendar

| Afirmación encontrada en los apuntes | Tratamiento |
|---|---|
| No existe release. | Corregida con el registro de GitHub publicado el mismo día. |
| CC Switch sería una amenaza si añadiera sincronización. | Descartada como premisa actual: su README ya anuncia sincronización entre dispositivos. |
| No hay competidores o Muster es el único producto integrado. | No demostrado. Se incorporaron Packmind, CC Switch y alternativas combinables. |
| El margen sería superior al 90 %, la infraestructura casi gratis o la operación nula. | No demostrado. Se sustituyó por supuestos explícitos que imputan trabajo humano y adquisición. |
| Hay ingresos futuros, inversión necesaria o plazos de construcción determinados. | No se adoptaron como previsiones. Los escenarios no tienen demanda ni costes medidos. |
| Un cambio a Sustainable Use, BSL o módulos cerrados es necesario para cobrar. | Descartado. Apache-2.0 ya permite servicios comerciales. |
| Oliver es el titular exclusivo y la marca está disponible. | No acreditado. Un listado de autores de Git no resuelve ninguna de las dos cuestiones. |
| n8n prohíbe todo uso comercial. | Corregida: la licencia permite determinados usos internos y limita otras formas de uso y distribución. |
| El núcleo local satisface automáticamente privacidad, residencia o requisitos de contratación. | Descartada. Hay que examinar soporte, facturación, diagnóstico, integraciones y contratos reales. |
| SSO, SCIM o revocación de flota invalidan todo acceso y secretos anteriores. | Descartada. Servicio, Git, sistema operativo y proveedor de credenciales tienen autoridades diferentes. |
| Recibir el estado firmado de flota demuestra que cada dispositivo aplicó el cambio. | Descartada. Hace falta un reporte por dispositivo con frescura y alcance definidos; el protocolo alojado no existe hoy. |
| Pedir SSO, firmar una intención o pulsar un botón demuestra demanda pagada. | No. Se exige distinguir interés, pago, uso repetido y renovación. |
| Las recomendaciones de 30 agentes demuestran mercado. | No. Constituyen cobertura de investigación y propuestas de experimentos. |

## Cobertura de las investigaciones

La valoración de uso corresponde a la síntesis, no al `PASS` que cada agente asignó a su propia tarea. "Parcial" significa que se aprovecharon preguntas, referencias o límites, pero se descartaron conclusiones no demostradas. Ninguna fila valida demanda.

| N.º | Modelo | Frente | Uso y limitación |
|---|---|---|---|
| 01 | Luna | Producto y valor existente | Inventario con referencias; se corrigió el estado de releases. |
| 02 | Luna | Frontera del servicio alojado | Base para separar Git, dispositivos y servicio; arquitectura todavía hipotética. |
| 03 | Luna | Garantías de seguridad | Límites incorporados; revisión estática, sin auditoría externa. |
| 04 | Luna | Incorporación y primer valor | Fricción y métrica de activación propuestas; no observación de clientes. |
| 05 | Luna | Equipo gratuito | Base para preservar funciones existentes y distinguir autoridad cliente/servidor. |
| 06 | Luna | Adaptadores y compatibilidad | Riesgo de mantenimiento y diferencia entre simulaciones y agentes reales. |
| 07 | Luna | Preparación comercial | Distingue release, tests y preparación empresarial. |
| 08 | Luna | Identidad y facturación | Separación propuesta; sin implementación ni garantía de aislamiento. |
| 09 | Luna | Señales públicas | Ausencia de evidencia suficiente en la muestra; no censo de usuarios. |
| 10 | Luna | Operación y soporte | Costes humanos y automatización; afirmaciones antiguas de releases corregidas. |
| 11 | Haiku | Supabase | Parcial: licencia y separación de servicio; no se adoptaron todos los detalles de tarifas ni garantías. |
| 12 | Haiku | n8n | Parcial: distinción de licencia; excluidas fuentes secundarias de embedding y promesas de operación casi nula. |
| 13 | Haiku | Tailscale | Parcial: analogía de coordinación; no se equiparó conectividad de red con control local de agentes. |
| 14 | Haiku | Gestores de secretos | Solo orientación para estudiar complementariedad; exclusividad, márgenes e ingresos proyectados descartados. |
| 15 | Haiku | Alternativas de configuración | Parcial: referencias de alternativas; descartados valor monetario imputado y ausencia de competidores. |
| 16 | Haiku | Fabricantes y estándares | Parcial: riesgo de convergencia; no se adoptó la inviabilidad categórica del servicio ni un permiso comercial innecesario. |
| 17 | Haiku | Otros negocios abiertos | Parcial: preguntas sobre empaquetado; datos de precios, licencias y garantías requerían corrección o exclusión. |
| 18 | Haiku | Licencias y marca | Se conservó la posibilidad de cobrar bajo Apache; titularidad, marca y conclusiones legales no acreditadas excluidas. |
| 19 | Haiku | Distribución | Canales como experimentos; tiempos, coste cero y predicciones de respuesta no adoptados. |
| 20 | Haiku | Comprador y presupuesto | Personas y desencadenantes como hipótesis; precios de otros productos no equivalen a presupuesto de Muster. |
| 21 | GLM Flash | Demanda observable | Issues externos aportan relatos de fricción; no prueban pago ni demanda de la oferta de flota. |
| 22 | GLM Flash | Precio y unidad de cobro | Oferta experimental por organización/persona; se rechazó que el alojamiento sea la única forma legal de cobrar. |
| 23 | GLM Flash | Economía | Preguntas sobre identidad y coste fundador; sustituidos márgenes optimistas por escenarios explícitos. |
| 24 | GLM Flash | Validación | Secuencia de pruebas adaptada; corregidas premisas de releases, competidores y ausencia de operación. |
| 25 | GLM Flash | Primer producto cobrable | Seguimiento sin custodia como alternativa; corregida la equivalencia entre firma de estado y aplicación por máquina. |
| 26 | GLM Flash | Compra empresarial | Mapa de preguntas útil; descartadas exenciones automáticas de privacidad y cumplimiento. |
| 27 | GLM Flash | Comunidad y frontera comercial | Operación adicional, exportación y continuidad local como propuesta; no compromiso contractual nuevo. |
| 28 | GLM Flash | Otros modelos de negocio | Comparación de soporte, despliegue privado, Pro, marketplace y proxy; consultoría no equivale a poca operación. |
| 29 | GLM Flash | Caso adversario | Alternativas gratuitas y metadatos sensibles; la conclusión de que nadie pagará tampoco está demostrada. |
| 30 | GLM Flash | Dos rutas estratégicas | Diferencia entre venta directa y empresarial; descartados umbrales de lista de espera como prueba suficiente de pago. |

## Cómo se comprobaron los números

El [CSV](paid-product-economics.csv) contiene escenarios hipotéticos en USD por mes, sin redondear la contribución. Las fórmulas son:

```text
comisión = precio × 0,035 + 0,30
soporte = horas de soporte × 40
contribución = precio − infraestructura − identidad − comisión − soporte − reserva
organizaciones = techo((costes fijos + objetivo de excedente) / contribución)
```

Una contribución negativa se marca como `not_viable`. El escenario pequeño reserva USD 16,67 por cuenta al mes para adquisición e incorporación. El empresarial los deja dentro del presupuesto fijo supuesto de venta y operación; no están medidos ni garantizados. La tabla de la propuesta redondea moneda a dos decimales y explica exclusiones. No se midieron rendimiento, conversión, retención, adquisición ni soporte de clientes.

## Evidencia que todavía falta

Faltan clientes observados, un problema recurrente compartido, comparación del flujo contra alternativas, pagos y renovaciones. También faltan costes reales de incorporación, soporte, adquisición y recuperación; una prueba del flujo alojado y sus límites de seguridad; y revisión de contratación para los compradores concretos.

La siguiente investigación con más valor ocurre con equipos reales. Más opiniones de agentes sobre la misma documentación no sustituyen esa evidencia.
