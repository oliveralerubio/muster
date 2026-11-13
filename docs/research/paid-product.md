# Muster Cloud como producto de suscripción

Propuesta del 13 de noviembre de 2026. Sustituye la primera investigación con una segunda ronda dedicada al SaaS autoservicio. No es una oferta vigente ni autoriza construir o desplegar el servicio. La [evidencia](paid-product-evidence.md) distingue código existente, fuentes externas, propuestas y supuestos económicos.

## El producto que recomiendo probar

**Muster Cloud es un espacio alojado que mantiene la configuración de tus agentes en tus dispositivos, permite recuperar cambios y administra el equipo desde una cuenta.** El cliente conecta sus máquinas; Muster opera almacenamiento, sincronización, versiones, identidad, facturación y recuperación. Los agentes continúan ejecutándose localmente.

Empezaría por equipos de 2 a 15 personas que usan varios agentes y máquinas. Es un segmento propuesto, no un mercado medido. El comprador inicial sería quien hoy prepara el entorno de los compañeros: responsable técnico, fundador técnico o responsable de plataforma. Mantendría un plan personal sobre el mismo producto y compararía sus pagos y renovaciones por separado. Cloud debe permitir alta directa; no exige adoptar primero el OSS. Quien eligió el núcleo precisamente para evitar cuentas puede no ser el comprador adecuado.

La promesa a probar es:

> Conecta tus máquinas y tu equipo. Distribuye la configuración aprobada de tus agentes, ve qué dispositivos la aplicaron y recupera cambios desde una cuenta.

La suscripción paga por ese resultado continuo. Una incorporación puede ser ocasional; los cambios de skills, MCP, instrucciones y dispositivos mantienen el trabajo recurrente. Debemos medir que ese trabajo ocurra con frecuencia suficiente para renovar. Si solo se usa al instalar una máquina, la tesis de suscripción se debilita.

El núcleo abierto sigue funcionando gratis con Git propio. Un cliente local que aplica archivos es compatible con SaaS, como muestran servicios con clientes de escritorio. En Cloud, el usuario no crea remotos Git, configura SSH, instala servidores ni programa sincronizaciones. Una decisión sobre un conflicto o aprobar un dispositivo corresponde al cliente; instalarle el producto a mano corresponde a un fallo de autoservicio que debemos corregir.

## Qué existe y qué hay que construir

La base examinada es [`f8ebe16`](https://github.com/oliveralerubio/muster/tree/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab). Existe una [release v0.1.0](https://github.com/oliveralerubio/muster/releases/tag/v0.1.0), aunque el README de ese snapshot conserva una frase anterior que lo contradice.

| Capacidad | Producto abierto actual | Trabajo nuevo de Cloud |
|---|---|---|
| Configuración | Formato canónico, adaptadores y aplicación local con perfiles. | Selección e importación guiadas, revisiones compartidas y propuestas desde la web. |
| Sincronización | Git del usuario, pull y push. Un conflicto puede exigir resolución manual. | Transporte operado por Muster, reintentos, concurrencia y conflictos comprensibles dentro del producto. |
| Secretos | Bóvedas age por grupos, claves por máquina y recuperación. | Enlace seguro de cuenta y dispositivo, recuperación explicada y autorización cloud sin sustituir las firmas. |
| Equipos | Miembros, grupos, quórum, aprobaciones y revocación gratuitos. | Invitaciones y gestión web conectadas con las autorizaciones criptográficas existentes. |
| Versiones | Historial Git y generaciones locales restaurables. | Historial alojado, restauración distribuida, exportación y backups probados. |
| Estado | Estado deseado firmado y resultados operativos locales. | Recibos por dispositivo con revisión, resultado y antigüedad. |
| Servicio comercial | Interfaz web local, CLI y MCP. | Cuentas, aislamiento, planes, cobro, baja, observabilidad y operación multiusuario. |

La [especificación](../SPEC.md#out-of-scope) deja el hub para trabajo futuro. Esa decisión anterior describe el alcance implementado; no impide diseñar ahora el SaaS pedido. Tener un motor local útil no convierte el panel local en un servicio multiusuario.

## Tres formas de negocio y una entrada recomendada

| Forma | Trabajo pagado | Ventaja | Riesgo principal | Decisión propuesta |
|---|---|---|---|---|
| Cloud personal | Mantener el entorno entre máquinas y recuperarlo sin operar transporte. | Alta y compra individuales; recorrido más corto. | Sustitutos gratuitos, ingreso bajo y posible uso episódico. | Ofrecerlo sobre la misma base, con una cohorte propia. |
| Cloud para equipos pequeños | Incorporar personas y máquinas, distribuir cambios aprobados y ver resultados. | Un comprador coordina varios usuarios; hay más ocasiones de uso compartido. | El equipo puede considerar suficiente Git privado. | Centro del lanzamiento y de la primera prueba comercial. |
| Cloud para organizaciones mayores | El mismo servicio con identidad corporativa, políticas, auditoría y garantías operativas. | Más ingresos posibles por cuenta. | Mayor coste de seguridad, identidad, contratación e incidentes. | Expansión condicionada a clientes recurrentes; no alcance inicial. |

Un SaaS pequeño y una empresa mayor pueden vender exactamente el mismo software. El pequeño mantiene una región, pocos planes, una matriz de compatibilidad acotada y compra autónoma. La empresa mayor amplía capacidad, controles y equipo interno. Ninguna ruta requiere vender horas, instalaciones o proyectos personalizados.

La recomendación de equipos es una apuesta razonada sobre coordinación y frecuencia, no una conclusión obtenida por mayoría de agentes. Si la cohorte personal activa y renueva mejor con menos coste, el lanzamiento debe adaptarse a ese resultado.

## Por qué pagar si existen alternativas

| Alternativa comprobada | Lo que ya resuelve | Diferencia que Muster debe demostrar |
|---|---|---|
| Muster OSS, Git privado y chezmoi | Transporte barato, historial y control sobre los datos. | El recorrido completo evita configurar transporte, aprobaciones dispersas, diagnóstico y recuperación manual. |
| [CC Switch](https://github.com/farion1231/cc-switch) | Proveedores, MCP, skills y prompts; sync entre dispositivos con WebDAV o S3 del usuario. | Un servicio alojado listo para usar y coordinación de personas, permisos y dispositivos. No basta afirmar que CC Switch carece de sync. |
| [Praxl](https://praxl.app/) | Gestión multiagente de skills, sincronización, versiones y oferta cloud gratuita. Pro anuncia $5/mes. | Alcance adicional de entorno, MCP, secretos y dispositivos en un recorrido mejor. Pro aparece como "Coming soon"; su precio no demuestra ventas. |
| [Packmind](https://packmind.com/pricing/) | Centralización y distribución de estándares, reglas y prompts; gobernanza empresarial. | Administración del entorno por dispositivo, además del contenido de repositorios. Hay solapamiento real. |
| Doppler, Infisical y gestores de secretos | Distribución especializada de secretos y controles organizacionales. | Configuración del entorno de agentes sin exigir sustituir la fuente de secretos del cliente. |
| Sincronización nativa de cada agente | Puede resolver parte del trabajo dentro de un proveedor. | Una experiencia útil entre herramientas y perfiles distintos. Los fabricantes también pueden ampliar su alcance. |

No encontramos base para afirmar que Muster sea único. Tampoco para concluir que nadie paga por sincronización personal. [Obsidian Sync](https://obsidian.md/sync) comercializa sync e historial desde $4 por usuario al mes con pago anual. El relato de ConfigMesh sobre rechazo a $5 es un caso de un fundador, no un techo universal de precio.

De Supabase Cloud y n8n Cloud copiaría la separación entre software utilizable por cuenta propia y una operación alojada que el comprador recibe completa. No copiaría su unidad de consumo: Muster no ejecuta consultas, workflows ni tokens de agentes en el servidor. n8n usa además una licencia con restricciones comerciales; no equivale al núcleo Apache-2.0 de Muster.

## El recorrido completo del cliente

Las pantallas y comandos descritos aquí son propuestas, no funciones ya disponibles.

1. El usuario crea cuenta y espacio desde la web. Ve precio, alcance, retención y prueba de 14 días antes de conectar dispositivos. La prueba no requiere tarjeta; sin compra explícita no hay cobro.
2. Descarga el cliente firmado para un sistema admitido. Un enlace en el navegador conecta el dispositivo con un código temporal. El instalador prepara las dependencias necesarias; el usuario no configura Git.
3. El cliente muestra qué archivos y campos importará, su destino y su diff. Permite excluir contenido y mover credenciales detectadas a la bóveda. Detectar patrones de secretos no garantiza encontrarlos todos. Nada se sube sin confirmar.
4. La primera máquina establece las claves y la confianza del workspace. El usuario guarda y comprueba el material de recuperación antes de confiar secretos al servicio.
5. Para la segunda máquina, el usuario inicia sesión y solicita enlace. Un dispositivo o administrador ya autorizado aprueba su identidad y acceso criptográfico. Recuperar email o pagar no concede descifrado automáticamente.
6. El cliente sincroniza en segundo plano y aplica el perfil correspondiente a cada máquina. La web distingue revisión deseada, recibida, aplicada, bloqueada y dispositivo sin contacto. Un dispositivo desconectado no figura como actualizado.
7. Si dos máquinas editan el mismo contenido, el producto conserva ambas propuestas y muestra el conflicto. Combina únicamente cambios seguros y disjuntos. El cliente decide los conflictos reales; no abre una terminal Git ni espera a Oliver.
8. En Team, el administrador invita miembros, asigna grupos, aprueba dispositivos y retira acceso. Los cambios que necesitan firma o quórum esperan esas aprobaciones dentro del producto.
9. La web muestra historial y propone restaurar una revisión. Un cliente autorizado valida, firma cuando corresponde y aplica el cambio como una revisión nueva. Restaurar no rebobina épocas de seguridad ni revive destinatarios revocados.
10. Al terminar la prueba, el usuario compra en un checkout alojado. El servicio activa el plan mediante eventos verificados y reconciliación con el proveedor de pagos. Repetir o retrasar un evento no duplica el cobro ni rebaja un plan vigente.
11. El portal permite cambiar plan y cancelar renovaciones. Propongo una ventana de exportación de 30 días tras terminar el acceso pagado, con fechas de borrado anunciadas. La exportación incluye historial y ciphertext verificables por una instalación OSS limpia.
12. El cliente conserva la última configuración aplicada y puede continuar con el núcleo abierto. Cancelar Cloud no inutiliza la CLI, la bóveda local ni el modo equipo gratuito.

La recuperación tiene tres objetos distintos: cuenta, historial y claves. Backups del proveedor recuperan datos alojados. Para descifrar secretos tras perder todos los dispositivos se necesita recuperación del cliente o administradores autorizados. Si también se pierde ese material, el proveedor no puede prometer recuperar los secretos.

## La frontera entre abierto y pago

Mantendría Apache-2.0 y todas las capacidades actuales del núcleo: CLI, adaptadores, formato, perfiles, importación, motor local, secretos, equipos, firmas, quórum, historial local y sync con Git propio. No retiraría funciones para crear la suscripción.

Cloud vende API y transporte operados por Muster, almacenamiento e historial remotos, consola de cuenta y equipo, estado de dispositivos, backups, recuperación operativa y ciclo de suscripción. Un futuro nivel organizacional puede añadir SSO, SCIM y retención avanzada. Su implementación y licencia requieren una decisión explícita; el backend puede tener licencia propia separada del núcleo.

Apache-2.0 permite comercializar servicios sin obligar al cliente a contratarlos. También permite competidores sobre el código publicado. La defensa comercial propuesta es una experiencia fiable y una marca de confianza. Cambiar la licencia no revoca derechos ya concedidos. No hemos auditado titularidad, marca ni todas las dependencias.

## Arquitectura inicial sin reescribir el producto entero

Recomiendo probar una API Cloud con Git administrado internamente por workspace. La base de datos mantiene cuentas, membresías, planes y dispositivos. Un servicio de sync conserva revisiones y conflictos. Clientes locales verifican confianza, cifran secretos, renderizan perfiles y aplican archivos. Colas y copias fuera del almacenamiento primario completan la operación.

Git es una decisión interna provisional. Conserva historia y facilita exportación; el verificador actual recorre estados intermedios en ese historial. Sustituirlo por snapshots sueltos exige preservar o rediseñar esa prueba, no solo cambiar una URL. La API debe controlar revisión base, idempotencia, aislamiento y límites. No debe aceptar órdenes Git arbitrarias del cliente.

La primera versión puede conservar una copia Git local preparada automáticamente por el instalador. Quitar el binario del cliente no justifica romper la verificación existente. Si un transporte distinto demuestra las mismas propiedades con menor coste, puede sustituirse detrás de la API. Git del cliente queda disponible como salida o integración opcional.

Antes de escoger definitivamente el transporte, un prototipo debe demostrar dos clientes concurrentes, uno offline, conflicto recuperable, cero pérdida silenciosa, cadena de firmas intacta y exportación verificable y utilizable desde OSS. Son pruebas por ejecutar; esta investigación no las ejecutó.

### Privacidad y autorización de la primera versión

Propongo secretos cifrados en el cliente y configuración seleccionada visible al proveedor, protegida en tránsito y reposo. Permite historial y diffs web sin desarrollar inicialmente un segundo cliente criptográfico completo en navegador. Es una decisión de producto por validar, no una garantía de seguridad existente.

La separación requiere cuidado: instrucciones, rutas, comandos MCP y campos de configuración también pueden contener tokens o información privada. La importación explica qué se aloja y qué conserva el historial. No se suben generaciones locales completas: pueden contener secretos ya resueltos. El cifrado age actual protege grupos de secretos, no todo el workspace; sus metadatos actuales también son visibles.

El cifrado integral en cliente es una alternativa válida si los compradores lo exigen. Añade recuperación de claves, edición y comparación en cliente y un diseño nuevo de fusión. No lo anunciaría como implementado ni usaría "zero-knowledge" para describir la versión propuesta.

Cuenta, facturación, autorización del servidor y autorización criptográfica son controles distintos. La web puede proponer cambios y solicitar aprobación; el backend no recibe por ello claves administrativas. Un recibo firmado acredita lo que declara el dispositivo, no que el agente obedeció instrucciones ni que el equipo esté libre de compromiso.

Revocar un miembro corta acceso futuro a Cloud. No borra copias ni invalida credenciales externas ya entregadas. Rotar esas credenciales requiere integración con su emisor y autorización suficiente. En ausencia de integración, el producto debe mostrar la acción pendiente; no prometer una revocación universal instantánea.

## Una primera versión cobrable y sus límites

La primera entrega debe completar el recorrido anterior en Linux y macOS, con una matriz pública y acotada de versiones de agentes. El alcance incluye un workspace, sync, historial, restauración, exportación, billing y permisos básicos de equipo. Las actualizaciones de compatibilidad pasan por pruebas con CLIs reales, despliegue gradual y recuperación. Los mocks actuales no demuestran esa compatibilidad.

No incluiría ejecución cloud de agentes, marketplace, Windows nativo, regiones múltiples, certificaciones prometidas, políticas arbitrarias ni conectores personalizados en esa primera versión. La fecha depende del prototipo, del protocolo de confianza y de la revisión de seguridad. Esta investigación no establece un plazo de construcción.

El servicio debe automatizar los casos normales:

| Situación | Respuesta del producto |
|---|---|
| Alta incompleta o enlace abandonado | Reanudar de forma idempotente; expirar códigos y solicitudes sin dejar recursos huérfanos. |
| Red caída o dispositivo offline | Mantener la configuración local; cola y reintentos con espera creciente; mostrar antigüedad. |
| Conflicto o cambio local | Conservar propuestas y respaldos; pedir una decisión comprensible sin sobrescribir silenciosamente. |
| Pago fallido | Reintentos y avisos; gracia explícita; suspender sync futuro conservando exportación. |
| Cliente incompatible | Bloquear el cambio afectado, explicar la actualización y preservar la última configuración válida. |
| Backup o restauración fallidos | Ensayos automáticos aislados; detener publicación de datos inválidos y alertar al operador. |
| Cancelación | Detener renovación, exportar, notificar plazos y borrar según política, incluido vencimiento de backups. |

Los incidentes sistémicos siguen requiriendo responsables internos. Comprar infraestructura administrada reduce esa carga; no elimina mantenimiento de adaptadores, seguridad y soporte ordinario. Una corrección repetida a mano es un fallo del producto a automatizar. No se vende ese trabajo al cliente como un servicio adicional.

## Precios experimentales y cuotas

Probaría estos precios mensuales en USD, antes de impuestos. Son hipótesis elegidas para el experimento, no disposición a pagar observada.

| Plan | Hipótesis | Alcance propuesto |
|---|---|---|
| OSS | Gratis | Capacidades actuales y transporte propio. |
| Cloud Personal | $8 por persona al mes | Un workspace personal, hasta 5 dispositivos activos, historial de 30 días. |
| Cloud Team | $12 por miembro al mes, mínimo $36 por workspace | Equipo, grupos e invitaciones web; hasta 5 dispositivos por miembro e historial de 90 días. |
| Organización | Sin precio fijado | El mismo Cloud con identidad y controles adicionales, tras validar su coste y compra. |

No cobraría por agente, token o cada sincronización. Un humano puede usar varias herramientas. Los dispositivos adicionales necesitan una política visible tras medir su uso; en el experimento no habría sobrecargos automáticos. La cuota inicial propuesta es 1 GB de configuración e historial por workspace y 10 MB por archivo. Binarios y cachés no se sincronizan. Exceder cuota avisa y pausa nuevas cargas, sin borrar ni impedir exportar.

Las facturas Team serían $36 con una persona, $60 con cinco, $240 con veinte y $1.200 con cien. Para una persona sin equipo existe Personal. El mínimo de Team debe probarse: puede cubrir coste por organización o impedir adopción. No desplegaría un Cloud gratuito permanente antes de conocer abuso y coste de las cuentas inactivas; sí una prueba y OSS gratuito.

SSO y SCIM no son gratuitos por usar un proveedor de login gratuito. WorkOS publica productos separados: SSO y Directory Sync parten de $125 por conexión al mes cada uno en su primer tramo. Esos costes pueden exceder el precio Team. No ofrecería ambos por $12 al mes sin modelar conexión, volumen y coste de operación.

## Dos tamaños de SaaS con cuentas completas

El [CSV económico](paid-product-economics.csv) contiene escenarios ilustrativos en USD. Todos los inputs de demanda y costes internos son supuestos. Solo la tarifa base de Paddle, 5% más $0,50 por transacción, se usa como precio público comprobado. La elegibilidad comercial, país de la entidad y contrato del proveedor deben confirmarse antes de adoptarlo.

El modelo calcula por cuenta: ingreso menos cobro, reserva del 1% para devoluciones o pérdidas, soporte variable, coste cloud variable y adquisición para reemplazar bajas. Añade infraestructura fija, herramientas y trabajo interno. La adquisición de crecimiento se paga aparte. Los costes de personal son asignaciones sin añadir un segundo sueldo por las mismas horas.

| Escenario hipotético | Clientes | Ingreso mensual | Coste fijo mensual | Contribución por cliente después de variables y reposición de bajas | Resultado mensual antes de impuestos y construcción |
|---|---:|---:|---:|---:|---:|
| Solo Personal, $8 | 600 personas | $4.800 | $3.210 | $5,42 | $42 |
| SaaS pequeño, Team medio de 5 personas | 100 equipos | $6.000 | $3.360 | $49,50 | $1.590 |
| Empresa mayor, Team medio de 20 personas | 300 equipos | $72.000 | $34.000 | $199,10 | $25.730 |

El equilibrio de esos supuestos sería 593 cuentas personales, 68 equipos pequeños o 171 equipos mayores. Son cálculos condicionados, no objetivos respaldados por mercado. El último escenario no incluye SSO o SCIM contratado, SLA, nuevas regiones ni certificaciones; al añadirlos cambian precio y costes.

En los dos escenarios pequeños, $2.960 de trabajo fijo se reparten entre 24 horas mensuales de compatibilidad a $30, ocho de incidentes y $2.000 de desarrollo y administración. El soporte variable se añade por ticket. Para 100 equipos, el supuesto de 0,4 tickets por cuenta de 15 minutos implica diez horas mensuales. La capacidad y las horas deben medirse; no demuestra que una persona pueda sostener cualquier volumen.

La empresa mayor reserva $30.000 mensuales para personal, mantenimiento y cumplimiento interno, además de $4.000 de infraestructura y herramientas. No representa un equipo contratado ni una estimación de plantilla suficiente. El soporte variable y CAC se contabilizan aparte; sus horas o facturas no deben volver a incluirse en la asignación fija.

Sensibilidades del escenario pequeño Team:

- Subir el soporte de 0,4 a 1,2 tickets por cuenta al mes reduce el resultado de $1.590 a $990.
- Subir la baja mensual supuesta del 2% al 5%, con CAC de $120, lo reduce a $1.230.
- Bajar el ingreso por cuenta de $60 a $36 manteniendo los demás supuestos lo reduce a una pérdida de $666.
- Añadir diez horas fijas de mantenimiento a $30 reduce el resultado en $300.

Presupuestaría la construcción por separado. A modo de límite de inversión, 600 o 1.200 horas valoradas a $30 representan $18.000 o $36.000. No son estimaciones de duración ni coste real. Recuperarlas en 24 meses requeriría $750 o $1.500 mensuales adicionales. Seguridad independiente, gastos legales y constitución necesitan cotización antes de comprometer ese presupuesto. Ningún escenario incorpora su coste desconocido como cero certificado.

Para ilustrar adquisición inicial, llegar desde cero a 100 equipos con CAC supuesto de $120 requiere $12.000 antes de bajas. No se deduce de tener GitHub o documentación. En régimen, 100 equipos con baja del 2% requieren dos nuevos equipos al mes solo para mantener tamaño. Una lista de espera no demuestra ese embudo.

## Validar el SaaS que se va a vender

La investigación inicial puede incluir entrevistas sobre incidentes recientes y alternativas. Las entrevistas no entregan el producto. Una landing con precio prueba interés; un checkout de prueba comprueba una integración. Ninguno acredita ingresos ni renovación.

Primero construiría una prueba técnica con datos sintéticos y el recorrido de dos dispositivos. Debe incluir aislamiento entre cuentas, pérdida de red, eventos de pago duplicados o fuera de orden, conflicto, actualización incompatible, restauración y salida a OSS. La recuperación de secretos debe probarse en una máquina limpia. Fallar en aislamiento, pérdida silenciosa o autorización bloquea la beta con datos reales.

Después abriría una beta pequeña del software completo con dos cohortes: diez individuos y diez equipos cualificados. Son tamaños propuestos para aprender, no muestras representativas. Cada participante debe activar sin ayuda del proveedor. Los asientos facturados son miembros humanos que aceptaron la invitación, no invitaciones pendientes ni agentes. Team requiere dos miembros y dos dispositivos con una revisión compartida aplicada; Personal requiere dos dispositivos. Una simulación sobre Git propio solo valida OSS.

Fijaría antes del lanzamiento estas reglas de decisión provisionales:

- Al menos seis de diez cuentas de cada cohorte completan activación sin asistencia de configuración.
- Al menos tres compran con dinero real y al menos dos de esas tres renuevan un segundo ciclo. Se informa la fracción exacta; no se extrapola una tasa de mercado.
- Se registran cambios sincronizados, restauraciones, cancelaciones y motivo de abandono. No basta mantener un dispositivo enviando señales.
- Se contabilizan todos los tickets, minutos, intervenciones, incidentes y costes cloud. Una reparación repetida exige automatización antes de ampliar la cohorte.
- Para crecer, la contribución observada debe ser positiva y el trabajo recurrente debe caber en un presupuesto interno fijado. Como primer límite, propondría 48 horas mensuales de operación, compatibilidad y soporte para 100 equipos, separado del desarrollo. Excederlo exige corregir producto, precio o capacidad.

Mediría el embudo en el servicio alojado con datos operativos mínimos y política de privacidad. El núcleo OSS conserva su funcionamiento sin telemetría obligatoria. Contar eventos de Cloud no autoriza recopilar prompts, código ni secretos.

Si paga y renueva la cohorte personal, merece prioridad aunque el razonamiento inicial favorezca equipos. Si paga Team con incorporación autónoma, se expande ese plan. Si solo compran después de una instalación personalizada, no se validó el SaaS pedido. Si ninguna cohorte renueva por un trabajo recurrente, se cambia la propuesta antes de construir controles empresariales.

La siguiente inversión concreta es ese recorrido completo y verificable. La investigación respalda una forma coherente de producto; su éxito comercial se decidirá con uso autónomo, cobros y renovaciones.
