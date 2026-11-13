# Muster Cloud: un SaaS de pago junto al núcleo abierto

Investigación del 13 de noviembre de 2026. Esta es una propuesta de negocio, no una hoja de ruta aprobada ni una oferta comercial vigente. La [evidencia y sus límites](paid-product-evidence.md) acompañan el análisis.

## La oportunidad que merece probarse

El producto buscado es un SaaS autoservicio. El cliente crea una cuenta, elige una suscripción, conecta sus dispositivos y usa el software alojado. Registro, pago, incorporación, sincronización, actualizaciones, recuperación y cancelación deben funcionar como producto. La entrega no puede depender de contratar a Oliver, de consultoría ni de una instalación hecha a mano por el fundador.

La propuesta es **Muster Cloud**: configuración de agentes, skills, herramientas MCP y secretos cifrados sincronizados entre dispositivos desde una cuenta, con administración de equipos en la web. El cliente de cada dispositivo aplica la configuración local. El servicio aloja y opera la coordinación y el almacenamiento necesarios; el usuario de pago no tiene que administrar repositorios Git, servidores ni tareas de sincronización.

Muster conserva el núcleo abierto para quienes prefieren operarlo por su cuenta. Cloud vende la experiencia administrada completa. Además de sincronizar, informa de cambios pendientes, detecta fallos y automatiza la recuperación de los casos que pueda resolver con seguridad. Todavía no hay evidencia de compradores que paguen por esta propuesta.

La promesa inicial que probaría es:

> Conecta tus máquinas una vez. Tus agentes, skills y configuración se mantienen sincronizados, y tu equipo se administra desde la web.

Es una propuesta para validar. No significa que Cloud ya exista ni que pueda demostrar que un agente obedeció las instrucciones. La comparación de negocios se refiere a un SaaS pequeño y rentable frente a un SaaS de mayor escala. Ambos entregan software por suscripción.

## Qué tenemos y qué falta

La base examinada es el commit [`f8ebe16`](https://github.com/oliveralerubio/muster/tree/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab). Ya incluye configuración para varios agentes, sincronización sobre Git, cifrado de secretos, miembros, grupos, aprobaciones, quórum, perfiles, firmas de estado y una interfaz web local. El [modo equipo](../guides/team-mode.md) ya forma parte del producto gratuito.

La [especificación actual](../SPEC.md#out-of-scope) deja expresamente para otro momento el servicio alojado, la interfaz multiusuario, SSO/SAML, SCIM, facturación y autorización en servidor. Un producto de pago aprovecharía el núcleo, pero exige construir y operar capacidades nuevas.

Hay una [release pública v0.1.0](https://github.com/oliveralerubio/muster/releases/tag/v0.1.0), publicada el 13 de noviembre de 2026, con binarios Linux y macOS. Algunas revisiones encontraron un README anterior que decía que no había releases. Para ese hecho prevalece el registro actual de GitHub. La existencia de una release o de pruebas automatizadas no demuestra adopción ni preparación empresarial.

En los issues y PRs públicos examinados no encontramos una señal suficiente de compradores independientes. Eso describe el alcance de la investigación; no demuestra que nadie lo use ni que el problema carezca de mercado.

Sí encontramos relatos externos del problema. Una [solicitud de sincronización entre dispositivos en Claude Code](https://github.com/anthropics/claude-code/issues/22648) describe configuraciones mantenidas por separado. Otra [solicitud sobre secretos de MCP](https://github.com/anthropics/claude-code/issues/28942) relata la dificultad de compartir configuración y preparar a nuevos desarrolladores. Son evidencia de fricción, no de disposición a pagar por Muster. Piden mejoras al fabricante, que también podría resolver parte del problema.

## Qué copiar de Supabase y n8n

[Supabase](https://supabase.com/docs/guides/self-hosting) permite operar su software y vende la comodidad y las prestaciones de su plataforma administrada. Su [monorepo tiene licencia Apache-2.0](https://github.com/supabase/supabase/blob/master/LICENSE). Esto no establece la licencia de cada componente del conjunto. El aprendizaje para Muster es que un cliente puede preferir pagar por una operación resuelta aunque tenga acceso al código.

La diferencia económica importa. Supabase aloja bases de datos y otros servicios con consumo continuo. Muster ya sincroniza usando Git del cliente. Cloud debe eliminar esa operación para el usuario que se suscribe: alta desde una cuenta, almacenamiento administrado, sincronización automática, versiones y recuperación. El valor incremental debe probarse frente a GitHub, GitLab y otros productos que ya sincronizan.

[n8n combina una edición comunitaria y ofertas comerciales](https://docs.n8n.io/sustainable-use-license/), pero su Sustainable Use License tiene restricciones comerciales. No es equivalente a Apache-2.0 ni a una licencia abierta según la [definición de OSI](https://opensource.org/osd). Su [licencia](https://github.com/n8n-io/n8n/blob/master/LICENSE.md) separa además los componentes empresariales. Podemos aprender de cómo empaqueta conveniencia y funciones para organizaciones sin copiar su licencia.

Muster no necesita cambiar de licencia para cobrar. Apache-2.0 permite uso comercial y servicios de pago. En esta propuesta el cliente paga una suscripción a Cloud y sus prestaciones administradas. La licencia del núcleo no obliga a contratar ese servicio.

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
| Configuración, adaptadores, skills y sincronización con Git propio. | Sincronización automática y almacenamiento operados por Muster, sin administrar Git. |
| Miembros, grupos, perfiles, quórum y aprobaciones actuales. | Invitaciones, conexión de dispositivos y administración del equipo desde la web. |
| Cifrado, validación de firmas, historial y diagnóstico local. | Versiones, recuperación, alertas y seguimiento central de máquinas. |
| Uso local con la configuración disponible cuando no hay conexión. | Suscripción, facturación, exportación y cancelación autoservicio; después, identidad corporativa e integración de directorios. |

Las correcciones de seguridad y compatibilidad del núcleo seguirían siendo públicas. La propuesta de pago tendría que aportar una operación mejor resuelta. Si solo traslada al navegador comandos que ya funcionan bien, el negocio puede no existir.

Cloud requiere una aplicación web, un servicio de cuentas y suscripciones, y un transporte administrado para la configuración y las versiones. El cliente cifra los secretos antes de enviarlos. El diseño propuesto almacena ese material cifrado y metadatos mínimos, conservando las claves privadas y la recuperación en los dispositivos autorizados. No necesita recopilar transcripciones de los agentes.

El núcleo actual usa Git. Es necesario diseñar un transporte nuevo o encapsular un backend administrado para que el cliente de pago no tenga que operar Git. Mantener el Git del cliente puede ser una integración opcional. Un panel que únicamente recibe estado no cumple por sí solo la propuesta de Cloud.

El estado firmado compartido describe el estado autorizado de la flota. Por sí solo no acredita que cada máquina lo aplicó. Esa confirmación, su fecha y sus límites requieren un protocolo nuevo de reporte por máquina que habría que diseñar y probar. No basta con subir `fleet.state.json` al servicio.

## Dos tamaños de SaaS

| Decisión | SaaS pequeño y rentable | SaaS de mayor escala |
|---|---|---|
| Cliente inicial supuesto | Responsable técnico de una agencia o equipo de aproximadamente 5 a 30 personas que usa varios agentes. | Responsable de plataforma o ingeniería con patrocinio de seguridad e identidad corporativa. El tamaño por sí solo no define al comprador. |
| Trabajo por el que paga | Sincronizar y recuperar su entorno de agentes sin operar la infraestructura. | Lo anterior entre equipos y departamentos, con integración corporativa y controles adicionales. |
| Producto | Cloud autoservicio, pocas integraciones y una matriz de versiones acotada. | El mismo Cloud con identidad, directorios, permisos y retención ampliados. |
| Compra | Registro, prueba y tarjeta en la web; activación automática. | Plan empresarial con aprovisionamiento automático; puede añadir proceso de compra corporativa. |
| Operación | Altas, cobro, sincronización, actualizaciones y recuperación automatizados. | La misma automatización con personal para disponibilidad, incidentes e integraciones comunes del producto. |
| Coste que puede limitarlo | Adquisición, soporte por cuenta y cambios de los fabricantes de agentes. | Personal, ciclo de ventas, integraciones de identidad, seguridad y obligaciones contractuales. |
| Motivo para descartarlo | Git y las alternativas gratuitas ya bastan o cada cliente exige trabajo a medida. | No aparece presupuesto suficiente para cubrir la venta y la operación adicionales. |

Los tamaños de equipo son criterios de selección propuestos para el experimento, no segmentos de mercado validados. La ruta pequeña permite aprender con menos compromisos. La empresarial puede justificar suscripciones mayores, pero exige más producto y capacidad operativa. No cambia la entrega a trabajo por encargo.

Automatizar reduce trabajo; no elimina la responsabilidad por incidentes, proveedores que cambian o clientes que necesitan ayuda. Una función que necesita intervención recurrente de Oliver no está terminada como SaaS. Hay que automatizar las causas repetidas y financiar la operación normal con la suscripción.

La oferta inicial se limita a lo que fortalece ese SaaS:

| Opción | Decisión inicial | Motivo |
|---|---|---|
| Cloud para personas | Puede probarse con el mismo registro y sincronización automáticos. | El problema entre máquinas tiene relatos públicos, aunque no prueba de pago. |
| Cloud para equipos | Primera oferta propuesta. | Permite probar administración compartida además de sincronización. |
| Cloud empresarial | Ampliación condicionada a demanda pagada. | Necesita identidad, controles y operación adicionales. |
| Marketplace o proxy de modelos | Fuera del primer producto. | Añade otro negocio y más obligaciones antes de validar Cloud. |

La consultoría, las migraciones realizadas por Oliver y los proyectos particulares para clientes quedan fuera del modelo. Las mejoras de producto pueden incluir importación automática y asistentes de recuperación para todos los suscriptores.

## El primer producto cobrable

La primera versión cobrable debe completar esta secuencia de principio a fin:

1. La persona crea una cuenta y un espacio de trabajo desde la web.
2. Elige un plan o una prueba, con activación automática y cobro recurrente.
3. Instala el cliente en un dispositivo y lo vincula mediante un flujo de autenticación.
4. El cliente detecta la configuración existente y presenta una importación revisable.
5. Vincula una segunda máquina; el servicio sincroniza y el cliente aplica lo aprobado.
6. Desde la web invita a otros miembros, distribuye perfiles y ve qué cambios se aplicaron.
7. Puede consultar versiones, recuperar un estado anterior y resolver fallos habituales con acciones del producto.
8. Administra pagos, exporta sus datos y cancela desde la cuenta.

La segunda máquina demuestra el primer valor. Las actualizaciones posteriores y la incorporación de otros miembros permiten comprobar si se repite. Las aprobaciones humanas de permisos o cambios sensibles siguen siendo decisiones del usuario; no son trabajo operativo que deba hacer Oliver.

La beta pagada debe ejecutar ese recorrido como software. Una instalación que el fundador completa a mano no cuenta como activación autoservicio. Una tarea manual excepcional se registra como fallo de producto o incidencia, y no como prestación que se vende al cliente. No se simula automatización detrás de un panel.

Dejaría fuera inicialmente un proxy de modelos, reventa de tokens, marketplace, custodia central de secretos en claro y desarrollos particulares para cada cliente. Cada uno añade dependencias y obligaciones que no necesitamos para comprobar la hipótesis principal.

## Precios para probar

Estos importes en USD son ofertas experimentales. No son precios vigentes ni una estimación de lo que el mercado acepta. Los precios de competidores sirven para estudiar su empaquetado, no para demostrar el precio de Muster.

Una propuesta coherente para entrevistar compradores y ofrecer una beta de autoservicio sería:

| Oferta hipotética | Precio de prueba | Qué se compra |
|---|---|---|
| Community | Gratis. | Núcleo actual, incluido modo equipo sobre Git propio. |
| Cloud Team | USD 99 por organización al mes, hasta 10 personas; USD 10 por persona adicional. | Cuenta, sincronización administrada y flujo de equipos descrito antes. |
| Cloud Enterprise | Como escenario financiero, suscripción de USD 18.000 al año. | El mismo SaaS con prestaciones corporativas definidas. No implica que hoy estén disponibles. |

Con esa fórmula, 5 personas costarían USD 99 al mes, 20 costarían USD 199 y 100 costarían USD 999 antes de contratar prestaciones empresariales adicionales. Una persona puede seguir usando Community gratis. Las máquinas y los agentes automáticos no se convierten en asientos humanos facturables.

Probaría también USD 199 por equipo para saber si el valor soporta un precio que cubra la operación. No publicaría simultáneamente muchas tarifas. Mediría aceptación de una oferta concreta, pago, renovación y motivo de rechazo. Una carta de intención aporta menos evidencia que un piloto pagado que renueva.

## Qué tendría que pasar para ser rentable

Los siguientes cálculos son escenarios ilustrativos. No son costes medidos, márgenes observados ni previsiones de ingresos. El [CSV](paid-product-economics.csv) permite comprobar las cuentas. El soporte aparece como coste interno de operar el SaaS; no son horas vendidas ni un trabajo que el cliente encarga a Oliver. El objetivo es reducirlo con diagnóstico y recuperación automáticos.

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

Entre los días 31 y 60 probaría un prototipo del recorrido autoservicio y la oferta de suscripción con esos equipos. Antes de que exista producto cobrable, se puede medir interés con una lista de acceso y precio explícito, sin simular ventas. La beta pagada comienza cuando el recorrido central ya funciona como software. El objetivo sería que tres organizaciones se suscriban por sí mismas, con al menos dos fuera del círculo cercano de Oliver. Registraría pagos, abandonos, tiempo hasta la segunda máquina sincronizada y fallos que requieren intervención.

Entre los días 61 y 90 buscaría renovación y un segundo cambio real del equipo, siempre que la beta ya esté disponible. Estos plazos organizan el experimento; no estiman el trabajo de construir Cloud. Para seguir con la ruta pequeña, propondría exigir al menos dos renovaciones al precio ofrecido, altas sin intervención del fundador y soporte estabilizado cerca de 15 minutos por organización al mes. Una incidencia de seguridad o una recuperación no probada puede detener la beta aunque haya ventas.

La vía empresarial necesitaría, además, responsables con presupuesto y requisitos comunes que el plan pequeño no satisfaga. Dos clientes que ya pagan por Cloud y comprometen presupuesto para las mismas funciones de identidad o auditoría justificarían diseñar esa ampliación. Esas funciones pasan al producto compartido. Pedir SSO sin presupuesto no basta.

Si los equipos resuelven el problema con Git, Packmind o CC Switch y no pagan por la diferencia, conservaría Muster como herramienta abierta y descartaría ese servicio. Si pagan pero requieren atención constante, la propuesta tampoco cumple el objetivo de poca operación. La decisión de construir nube llega después de comprobar el trabajo por el que se paga.
