# Un producto de pago alrededor de Muster

Investigación del 13 de noviembre de 2026. Esta es una propuesta de negocio, no una hoja de ruta aprobada ni una oferta comercial vigente. La [evidencia y sus límites](paid-product-evidence.md) acompañan el análisis.

## La oportunidad que merece probarse

Muster puede mantener su núcleo abierto y vender un servicio opcional para equipos. La hipótesis más concreta es ayudar a una persona responsable a comprobar que un cambio aprobado en la configuración de agentes llegó a las máquinas del equipo, identificar las que fallaron y completar la corrección.

Ese resultado puede tener valor recurrente cuando cambian agentes, instrucciones, herramientas MCP, skills o miembros. Sin embargo, todavía no hemos observado compradores que paguen por él. Construir un panel, añadir SSO o elegir una licencia no resuelve esa incógnita.

Mi recomendación es empezar con equipos pequeños que usan varios agentes, probar una oferta acotada y conservar la posibilidad de crecer hacia empresas. La oferta empresarial tendría que justificarse con presupuesto y requisitos repetidos de clientes reales. No conviene construir ambas a la vez.

La promesa inicial que probaría es:

> Publica un cambio aprobado para tus agentes y comprueba qué máquinas lo aplicaron, cuáles necesitan atención y qué falta para completar el despliegue.

Es una propuesta para validar. No significa que Muster ya ofrezca esa experiencia alojada ni que pueda demostrar que un agente obedeció las instrucciones.

## Qué tenemos y qué falta

La base examinada es el commit [`f8ebe16`](https://github.com/oliveralerubio/muster/tree/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab). Ya incluye configuración para varios agentes, sincronización sobre Git, cifrado de secretos, miembros, grupos, aprobaciones, quórum, perfiles, firmas de estado y una interfaz web local. El [modo equipo](../guides/team-mode.md) ya forma parte del producto gratuito.

La [especificación actual](../SPEC.md#out-of-scope) deja expresamente para otro momento el servicio alojado, la interfaz multiusuario, SSO/SAML, SCIM, facturación y autorización en servidor. Un producto de pago aprovecharía el núcleo, pero exige construir y operar capacidades nuevas.

Hay una [release pública v0.1.0](https://github.com/oliveralerubio/muster/releases/tag/v0.1.0), publicada el 13 de noviembre de 2026, con binarios Linux y macOS. Algunas revisiones encontraron un README anterior que decía que no había releases. Para ese hecho prevalece el registro actual de GitHub. La existencia de una release o de pruebas automatizadas no demuestra adopción ni preparación empresarial.

En los issues y PRs públicos examinados no encontramos una señal suficiente de compradores independientes. Eso describe el alcance de la investigación; no demuestra que nadie lo use ni que el problema carezca de mercado.

Sí encontramos relatos externos del problema. Una [solicitud de sincronización entre dispositivos en Claude Code](https://github.com/anthropics/claude-code/issues/22648) describe configuraciones mantenidas por separado. Otra [solicitud sobre secretos de MCP](https://github.com/anthropics/claude-code/issues/28942) relata la dificultad de compartir configuración y preparar a nuevos desarrolladores. Son evidencia de fricción, no de disposición a pagar por Muster. Piden mejoras al fabricante, que también podría resolver parte del problema.

## Qué copiar de Supabase y n8n

[Supabase](https://supabase.com/docs/guides/self-hosting) permite operar su software y vende la comodidad y las prestaciones de su plataforma administrada. Su [monorepo tiene licencia Apache-2.0](https://github.com/supabase/supabase/blob/master/LICENSE). Esto no establece la licencia de cada componente del conjunto. El aprendizaje para Muster es que un cliente puede preferir pagar por una operación resuelta aunque tenga acceso al código.

La diferencia económica importa. Supabase aloja bases de datos y otros servicios con consumo continuo. Muster ya sincroniza usando Git del cliente. Alojar los mismos archivos aporta menos valor adicional. Necesitamos demostrar qué trabajo recurrente elimina el servicio y por qué GitHub o GitLab no bastan.

[n8n combina una edición comunitaria y ofertas comerciales](https://docs.n8n.io/sustainable-use-license/), pero su Sustainable Use License tiene restricciones comerciales. No es equivalente a Apache-2.0 ni a una licencia abierta según la [definición de OSI](https://opensource.org/osd). Su [licencia](https://github.com/n8n-io/n8n/blob/master/LICENSE.md) separa además los componentes empresariales. Podemos aprender de cómo empaqueta conveniencia y funciones para organizaciones sin copiar su licencia.

Muster no necesita cambiar de licencia para cobrar. Apache-2.0 permite uso comercial, distribución y servicios de pago. El cliente puede comprar alojamiento, operación, soporte o compromisos contractuales adicionales. No necesita comprar un permiso que la licencia actual ya concede.

Conservaría Apache-2.0 para el núcleo existente. Si más adelante aparecen módulos empresariales nuevos, su licencia y separación requieren una decisión expresa. Los derechos ya concedidos sobre las versiones publicadas permanecen. Esta investigación no acredita titularidad exclusiva, disponibilidad de marca ni viabilidad jurídica de un cambio de licencia.

## El mercado ya tiene alternativas

No sostendría la afirmación de que el espacio está vacío:

| Alternativa | Lo que ya ofrece | Pregunta que Muster debe responder |
|---|---|---|
| [Packmind](https://packmind.com/pricing/) | Distribución de instrucciones entre agentes y repositorios; oferta empresarial de gobernanza, identidad y auditoría. | ¿La gestión de máquinas y su configuración completa resuelve un trabajo que su enfoque sobre repositorios deja pendiente? |
| [CC Switch](https://github.com/farion1231/cc-switch) | Gestión de proveedores, MCP, skills y prompts; sincronización entre dispositivos mediante WebDAV o almacenamiento compatible con S3. | ¿Un equipo necesita coordinación de personas y máquinas que esta sincronización no le da? |
| [chezmoi](https://www.chezmoi.io/) y Git con scripts | Gestión flexible de configuración usando herramientas conocidas. | ¿El ahorro de trabajo de Muster compensa adoptar otra herramienta? |
| [Infisical](https://infisical.com/), [Doppler](https://www.doppler.com/) y [Bitwarden Secrets Manager](https://bitwarden.com/products/secrets-manager/) | Productos especializados en secretos y acceso de organizaciones. | ¿Muster aporta distribución y configuración de agentes sin obligar a sustituir el gestor existente? |

El enfoque que probaría es el entorno de trabajo completo de un equipo que usa varios agentes y máquinas. Su diferenciación tendría que demostrarse en ese flujo completo. La suma de funcionalidades no demuestra exclusividad ni disposición a pagar.

La convergencia de formatos y los controles propios de los fabricantes pueden reducir el valor de traducir archivos. Necesitamos que el valor pagado sobreviva a esa simplificación. Competir por contenido de skills o por un marketplace añadiría otro negocio antes de probar este.

## Qué permanece gratuito y qué se vendería

La frontera propuesta conserva las capacidades actuales:

| Núcleo abierto y gratuito | Servicio de pago propuesto |
|---|---|
| CLI, interfaz local y MCP. | Interfaz alojada para administrar una organización. |
| Configuración, adaptadores, skills y sincronización con Git propio. | Seguimiento central de versiones aplicadas, errores y máquinas pendientes. |
| Miembros, grupos, perfiles, quórum y aprobaciones actuales. | Coordinación de incorporaciones y cambios sin perseguir a cada persona. |
| Cifrado, validación de firmas, historial y diagnóstico local. | Alertas, seguimiento y retención de eventos del servicio, con exportación. |
| Uso local con la configuración disponible cuando no hay conexión. | Más adelante, identidad corporativa, integración con directorios y compromisos de soporte contratados. |

Las correcciones de seguridad y compatibilidad del núcleo seguirían siendo públicas. La propuesta de pago tendría que aportar una operación mejor resuelta. Si solo traslada al navegador comandos que ya funcionan bien, el negocio puede no existir.

El diseño alojado es una hipótesis subordinada a ese resultado. Una primera variante podría recibir únicamente informes firmados de estado iniciados por los dispositivos. No necesitaría recibir secretos ni transcripciones. El acceso a repositorios, si resulta necesario, tendría alcance mínimo y autorización explícita.

El estado firmado compartido describe el estado autorizado de la flota. Por sí solo no acredita que cada máquina lo aplicó. Esa confirmación, su fecha y sus límites requieren un protocolo nuevo de reporte por máquina que habría que diseñar y probar. No basta con subir `fleet.state.json` al servicio.

## Dos negocios distintos

| Decisión | Negocio pequeño y rentable | Empresa con equipo y ventas a organizaciones |
|---|---|---|
| Cliente inicial supuesto | Responsable técnico de una agencia o equipo de aproximadamente 5 a 30 personas que usa varios agentes. | Responsable de plataforma o ingeniería con patrocinio de seguridad e identidad corporativa. El tamaño por sí solo no define al comprador. |
| Trabajo por el que paga | Completar incorporaciones y cambios de configuración con menos seguimiento manual. | Coordinar el ciclo de vida de agentes y dispositivos entre equipos, con integración corporativa y evidencia para sus controles. |
| Producto | Una experiencia acotada, contratación directa y pocas integraciones. | Identidad, permisos del servicio, integración de directorios, retención, exportación y opciones de despliegue según demanda. |
| Venta | Prueba corta y pago con tarjeta; instalación y resolución guiadas. | Piloto con presupuesto, revisión de seguridad, negociación y compra corporativa. |
| Soporte | Asíncrono, alcance publicado y versiones compatibles delimitadas. | Obligaciones pactadas, responsables de incidentes y personal disponible para cumplirlas. |
| Coste que puede limitarlo | Adquisición, soporte por cuenta y cambios de los fabricantes de agentes. | Personal, ciclo de ventas, integraciones de identidad, seguridad y obligaciones contractuales. |
| Motivo para descartarlo | Git y las alternativas gratuitas ya bastan o cada cliente exige trabajo a medida. | No aparece presupuesto suficiente para cubrir la venta y la operación adicionales. |

Los tamaños de equipo son criterios de selección propuestos para el experimento, no segmentos de mercado validados. La ruta pequeña permite aprender con menos compromisos. También puede tener un techo comercial bajo. La empresarial puede justificar contratos mayores, pero constituye un negocio de operación y venta continuas.

Automatizar reduce trabajo; no elimina la responsabilidad por incidentes, proveedores que cambian o clientes que necesitan ayuda. Una oferta que depende de intervención recurrente de Oliver incumple el objetivo de poca operación. Ese trabajo tendría que eliminarse mediante producto, cubrirse con personal financiado por el precio o quedar fuera de alcance.

Hay otras formas de cobrar, pero cada una prueba un negocio distinto:

| Alternativa | Cuándo tendría sentido | Motivo para no empezar por ella |
|---|---|---|
| Migración o consultoría con precio fijo | Un comprador necesita ayuda inmediata y acepta un alcance cerrado. | Cobra trabajo puntual; puede aumentar la dependencia de Oliver. |
| Edición empresarial nueva para despliegue privado | Compradores con presupuesto rechazan una nube compartida. | Añade versiones, instalaciones y soporte; necesita prestaciones nuevas y condiciones claras. |
| Suscripción personal Pro | Una persona paga por un resultado recurrente que Community no resuelve. | La sincronización individual tiene alternativas gratuitas y presión de los fabricantes. |
| Marketplace de skills | Hay compradores, autores y demanda verificable de distribución comercial. | Exige moderación, revisión, pagos y atender a dos grupos de usuarios. |
| Proxy de modelos o secretos | El comprador necesita control en la ejecución y acepta esa intermediación. | Cambia la arquitectura, los datos tratados y la responsabilidad del negocio. |

El despliegue privado podría ser una modalidad empresarial futura. No es necesario construirlo junto con el primer servicio. La licencia Apache actual permite usos comerciales; una oferta nueva tendría que explicar el valor adicional que se compra.

## El primer producto cobrable

Probaría una sola secuencia para el responsable del equipo:

1. Selecciona una configuración de equipo y las máquinas que deben recibirla.
2. Aprueba un cambio usando las reglas y firmas existentes.
3. Ve qué máquinas informan que lo aplicaron y cuáles están atrasadas, desconectadas o con errores.
4. Recibe una explicación y una acción de recuperación para los fallos que el producto sabe resolver.
5. Comprueba cuándo se completó el cambio, conservando los pendientes visibles.

La incorporación de una persona sirve para demostrar el primer valor. Las actualizaciones posteriores permiten comprobar si ese valor se repite. Un servicio de migración con pago único puede financiar aprendizaje, pero no demuestra una suscripción sostenible.

El piloto puede empezar con la CLI publicada, una vista privada de resultados y operación asistida de alcance explícito. Hay que registrar toda intervención humana. Simular un servicio automático mientras alguien resuelve todo a mano daría una señal falsa de rentabilidad.

Dejaría fuera inicialmente un proxy de modelos, reventa de tokens, marketplace, custodia central de secretos y desarrollos particulares para cada cliente. Cada uno añade dependencias y obligaciones que no necesitamos para comprobar la hipótesis principal.

## Precios para probar

Estos importes en USD son ofertas experimentales. No son precios vigentes ni una estimación de lo que el mercado acepta. Los precios de competidores sirven para estudiar su empaquetado, no para demostrar el precio de Muster.

Una propuesta coherente para entrevistar compradores y ofrecer pilotos sería:

| Oferta hipotética | Precio de prueba | Qué se compra |
|---|---|---|
| Community | Gratis. | Núcleo actual, incluido modo equipo sobre Git propio. |
| Team administrado | USD 99 por organización al mes, hasta 10 personas; USD 10 por persona adicional. | El flujo alojado de seguimiento y resolución descrito antes. |
| Enterprise | Piloto con alcance y precio fijos; como escenario financiero, contrato de USD 18.000 al año. | Requisitos adicionales y compromisos expresamente contratados. No implica que hoy estén disponibles. |

Con esa fórmula, 5 personas costarían USD 99 al mes, 20 costarían USD 199 y 100 costarían USD 999 antes de contratar prestaciones empresariales adicionales. Una persona puede seguir usando Community gratis. Las máquinas y los agentes automáticos no se convierten en asientos humanos facturables.

Probaría también USD 199 por equipo para saber si el valor soporta un precio que cubra la operación. No publicaría simultáneamente muchas tarifas. Mediría aceptación de una oferta concreta, pago, renovación y motivo de rechazo. Una carta de intención aporta menos evidencia que un piloto pagado que renueva.

## Qué tendría que pasar para ser rentable

Los siguientes cálculos son escenarios ilustrativos. No son costes medidos, márgenes observados ni previsiones de ingresos. El [CSV](paid-product-economics.csv) permite comprobar las cuentas.

Para el negocio pequeño supongo USD 8 mensuales de infraestructura por organización; comisión de pago de 3,5 % más USD 0,30; trabajo humano valorado en USD 40 por hora; y USD 16,67 mensuales de reserva por cuenta para recuperar aproximadamente USD 200 de adquisición e incorporación durante 12 meses. Esa duración es un supuesto contable, no retención observada.

Los costes fijos supuestos son USD 500 al mes: USD 100 comunes y 10 horas de mantenimiento a USD 40. El objetivo del cálculo es dejar USD 3.000 mensuales de excedente después de esos costes y de imputar el trabajo, antes de impuestos.

| Precio mensual por organización | Soporte mensual por organización | Contribución mensual aproximada | Organizaciones para cubrir costes fijos y objetivo |
|---|---|---|---|
| USD 99 | 15 minutos | USD 60,57 | 58 |
| USD 99 | 1 hora | USD 30,57 | 115 |
| USD 199 | 15 minutos | USD 157,07 | 23 |
| USD 99 | 3 horas | Negativa. | El volumen no arregla esta oferta. |

La contribución resta infraestructura, pago, soporte y reserva de adquisición e incorporación. El número de organizaciones se redondea hacia arriba. El modelo no incluye impuestos, devoluciones, pérdidas extraordinarias ni una garantía de continuidad de ingresos. Cada renovación fallida cambia la recuperación de la adquisición y puede exigir más caja. Tampoco establece cuánto esfuerzo o dinero permite conseguir esas organizaciones.

Para la ruta empresarial, un ejemplo deliberadamente distinto supone USD 35.000 de costes fijos mensuales de personal, venta y operación; USD 1.500 de ingreso mensual por organización; USD 50 de infraestructura; USD 250 de integración de identidad; cuatro horas de soporte a USD 40; y la misma comisión de pago. Deja USD 987,20 de contribución por organización y exige 39 organizaciones para cubrir los costes fijos y el mismo objetivo de USD 3.000. Todos esos importes son supuestos y el presupuesto fijo no está dimensionado para un equipo contratado real.

La integración de identidad merece presupuesto separado: [WorkOS publica cobro por conexión](https://workos.com/pricing), no solo por usuario. No basta con calcular el precio de un servidor. El escenario empresarial tampoco incluye una reserva adicional por adquisición e incorporación por cuenta; ambas tendrían que caber en el presupuesto fijo de ventas y operación o añadirse al modelo. No usaría estas cuentas para contratar ni comprometer un SLA.

El dato decisivo del piloto será cuánto cuesta adquirir, incorporar y atender a una cuenta que renueva. Si el trabajo por cuenta no baja después de automatizar las causas repetidas, habría que subir precio, reducir alcance o abandonar la oferta.

Las cuentas tampoco incluyen el coste inicial de construir el producto nuevo ni el tiempo sin clientes. Ese gasto necesita un presupuesto separado. Una cuenta mensual que cierra con clientes supuestos no demuestra que podamos conseguirlos o financiar la llegada hasta ellos.

## Límites de seguridad que afectan a la venta

Las [garantías actuales](../security/verification.md) deben acompañar cualquier propuesta comercial:

- Retirar acceso al servicio mediante SSO o SCIM no retira por sí mismo permisos en Git, acceso al sistema operativo ni credenciales de terceros.
- Revocar una máquina y volver a cifrar la bóveda protege generaciones futuras. No borra secretos ya copiados. Esos secretos necesitan rotación en el proveedor que los emite.
- Un dispositivo desconectado no recibe una orden inmediata. Su último estado conocido debe aparecer con fecha y alcance, sin presentarlo como confirmación actual.
- Una firma de estado o una confirmación de configuración aplicada no demuestra todas las acciones del agente ni quién humano las ejecutó.
- Los controles locales de entrega de secretos reducen exposición accidental; no aíslan procesos que corren bajo el mismo usuario.

Un servicio alojado añadiría identidades de organización y dispositivo, aislamiento entre organizaciones, recuperación de cuentas, retención y borrado. Las claves privadas, la recuperación y los secretos en claro permanecerían fuera de ese servicio según el diseño propuesto. Los metadatos también pueden revelar información sensible y requieren minimización.

El servicio tampoco debería poder aprobar cambios de flota únicamente porque alguien inició sesión en la web. La autoridad de la sesión alojada y las firmas administrativas de la flota son distintas. Cancelar la suscripción no debería inutilizar el núcleo local ni el repositorio del cliente.

No hay base aquí para afirmar certificación SOC 2, ISO 27001, cumplimiento automático, revocación universal o auditoría completa. Un producto local puede seguir tratando datos mediante soporte, facturación e integraciones. Los requisitos de privacidad y compra se evalúan por los flujos reales, no por la ausencia de un servidor en el núcleo actual.

## Cómo elegir durante los próximos 90 días

Este es un experimento propuesto. Los umbrales se eligen para orientar una decisión con recursos limitados; no tienen validez estadística ni garantizan mercado. Esta investigación no ha contactado ni cobrado a nadie.

Durante los primeros 30 días buscaría 15 conversaciones con responsables de equipos que ya usan varios agentes. La pregunta inicial sería sobre el último cambio o incorporación que salió mal: cómo lo detectaron, quién lo resolvió, cuánto trabajo exigió y qué alternativa usan. Después pediría observar el flujo con consentimiento y sin recoger secretos. Es más informativo que preguntar si les gusta un futuro panel.

Avanzaría a una prueba si cinco equipos independientes describen un problema recurrente concreto y tres responsables aceptan probar el mismo resultado. Si cada uno pide algo distinto, todavía no hay una oferta coherente. Probaría al menos una alternativa gratuita con ellos para comprobar qué valor incremental queda.

Entre los días 31 y 60 ofrecería a esos equipos un piloto pagado, con alcance, límites y precio escritos. El objetivo sería que tres organizaciones paguen por el mismo trabajo, con al menos dos fuera del círculo cercano de Oliver. Registraría pagos, abandonos, tiempo hasta el primer cambio completo, fallos e intervención humana. No presentaría un clic de compra o una promesa como ingreso.

Entre los días 61 y 90 buscaría renovación y un segundo cambio real del equipo. Para seguir con la ruta pequeña, propondría exigir al menos dos renovaciones al precio ofrecido, instalación cada vez más autónoma y soporte estabilizado cerca de 15 minutos por organización al mes. Una incidencia de seguridad o una recuperación no probada puede detener el piloto aunque haya ventas.

La vía empresarial necesitaría, además, responsables con presupuesto, un proceso de compra identificable y requisitos comunes que el producto pequeño no satisfaga. Dos pilotos pagados con patrocinador y los mismos bloqueos de identidad o auditoría justificarían diseñar esa oferta con más detalle. Pedir SSO sin presupuesto no basta.

Si los equipos resuelven el problema con Git, Packmind o CC Switch y no pagan por la diferencia, conservaría Muster como herramienta abierta y descartaría ese servicio. Si pagan pero requieren atención constante, la propuesta tampoco cumple el objetivo de poca operación. La decisión de construir nube llega después de comprobar el trabajo por el que se paga.
