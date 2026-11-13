# Evidencia de la propuesta Muster Cloud

Fecha de corte: 13 de noviembre de 2026. Este documento acompaña la [propuesta de producto](paid-product.md) y el [modelo económico](paid-product-economics.csv). La propuesta no implementa Cloud ni valida ventas.

## Qué significa cada clase de afirmación

- Un hecho de código describe el snapshot indicado mediante un enlace permanente.
- Una fuente externa describe lo que el proveedor o autor publica. Una página comercial no prueba compras, ingresos ni resultados de Muster.
- Una propuesta es una elección para construir o experimentar. Aún no está implementada.
- Un supuesto numérico permite calcular escenarios. No es una medición ni una previsión.

El acuerdo entre investigadores no equivale a evidencia independiente. Muchos consultaron los mismos precios y el mismo código. Los estados PASS y PARTIAL de sus entregas describen su evaluación del trabajo, no el estado comercial de Muster.

## Producto existente y límites técnicos

Fuente común: [`f8ebe16faf88c1790f1a54e16bfbd5167fe152ab`](https://github.com/oliveralerubio/muster/tree/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab).

| Hecho | Fuente primaria | Límite de la conclusión |
|---|---|---|
| Configuración canónica multiagente, equipos y licencia Apache-2.0. | [README](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/README.md), [LICENSE](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/LICENSE). | No acredita adopción, titularidad exclusiva ni licencia de cada dependencia. |
| Git es dependencia del transporte actual; hay conflictos que exigen resolución manual. | [src/git.rs](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/git.rs#L1), en particular el tratamiento de rebase y errores. | Una URL Git gestionada no completa por sí sola un recorrido SaaS. |
| El panel actual escucha localmente y usa un token de entrada. | [src/web/mod.rs](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/web/mod.rs#L1). | No es backend multiusuario con aislamiento, cuentas o billing. |
| Hub, sync cloud, SSO y facturación son trabajo posterior. | [docs/SPEC.md](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/docs/SPEC.md#L387). | La exclusión anterior no prohíbe la nueva propuesta de producto. |
| El vault cifra grupos con age; hay sidecars de metadatos sin valores secretos. | [src/vault.rs](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/vault.rs#L1), [metadatos](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/vault.rs#L63). | No cifra todo el workspace. Configuración y nombres pueden revelar datos sensibles. |
| Hay claves locales y recuperación; miembros, roles y aprobaciones ya existen. | [src/identity.rs](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/identity.rs#L1), [src/team.rs](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/team.rs#L1), [init](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/commands/init.rs#L58). | Login, billing y firma autorizada son autoridades distintas. Recuperar cuenta no recupera claves. |
| Las firmas encadenan estados; el verificador puede necesitar historia intermedia. | [ADR 0005](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/docs/adr/0005-signed-fleet-state.md#L39), [cadena contigua](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/docs/adr/0005-signed-fleet-state.md#L65). | Guardar snapshots sueltos no preserva automáticamente esas garantías. |
| Generaciones y backups de archivos aplicados son locales. | [src/engine/store.rs](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/engine/store.rs#L1). | Pueden incluir secretos resueltos. No deben subirse como si fueran backups cloud inocuos. |
| Hay resultado local de sync y coordinación remota. | [src/commands/sync.rs](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/commands/sync.rs#L85), [src/commands/machine.rs](https://github.com/oliveralerubio/muster/blob/f8ebe16faf88c1790f1a54e16bfbd5167fe152ab/src/commands/machine.rs#L868). | Estado deseado firmado no prueba aplicación ni obediencia del agente. Recibos cloud son diseño nuevo. |
| v0.1.0 está publicada. | [Release oficial](https://github.com/oliveralerubio/muster/releases/tag/v0.1.0). | Prevalece sobre la frase antigua del README. No equivale a uso o ventas. |

La selección técnica se apoya especialmente en los frentes Luna 02, 04, 05, 07, 09 y 10. Sus recomendaciones sobre Git interno y cifrado integral no son idénticas. La síntesis elige una API con Git interno provisional y cifrado de secretos; exige probar confianza, concurrencia, exportación y aceptación de privacidad antes de adoptar ese diseño.

## Fuentes externas comprobadas directamente en la síntesis

Las siguientes páginas se recuperaron el 13 de noviembre de 2026. Los extractos son breves y respaldan solo la afirmación indicada. Páginas de precios pueden cambiar y contener diferencias entre pago mensual, anual y anuncios futuros.

| Fuente | Extracto o dato observado | Uso permitido en la conclusión |
|---|---|---|
| [Praxl](https://praxl.app/) | "Cloud Free", "Coming soon Pro", "$5 /month", "Join waitlist". | Competencia directa y precio anunciado. No afirmar Pro activo, ventas ni precio por asiento. |
| [Packmind pricing](https://packmind.com/pricing/) | "Open Source Free", "Enterprise Custom", "SSO / SAML / SCIM". | Solapamiento en estándares y gobernanza. La página no prueba su infraestructura ni ventas. |
| [CC Switch README](https://github.com/farion1231/cc-switch) | "Sync across devices via WebDAV" y "S3-compatible storage". | Ya tiene sincronización usando almacenamiento aportado por el usuario. No tratarla como una amenaza exclusivamente futura. |
| [Obsidian Sync](https://obsidian.md/sync) | "$4 USD Per user, per month, billed annually"; "$5" mensual; "End-to-end encryption". | Oferta real de servicio de sync de pago con cliente local. No acredita elasticidad ni disposición a pagar por Muster. |
| [Raycast pricing](https://www.raycast.com/pricing) | La página incluye Cloud Sync entre prestaciones Pro. | Otro empaquetado posible. No permite atribuir toda compra de Pro a sync. |
| [Relato de ConfigMesh](https://david.coffee/configmesh-1-1/) | El autor describe pasar de $5 a $1,99 al mes; actualización de agosto de 2026 indica el nombre Synclet. | Una experiencia publicada de precio y confianza. No permite fijar un techo universal ni inferir el resultado de Muster. |
| [Supabase pricing](https://supabase.com/pricing) | Niveles Free, Pro y Team con prestaciones y uso diferenciados. | Patrón de empaquetado de servicio administrado. No trasladar consumo de base de datos a configuración de agentes. |
| [n8n pricing](https://n8n.io/pricing/) | "20€ /mo, billed annually", "Hosted by n8n", "No credit card required". | Patrón de compra y trial autoservicio. Su unidad de ejecuciones no encaja automáticamente con Muster. |
| [Paddle pricing](https://www.paddle.com/pricing) | "5% + 50¢ per Checkout transaction"; Merchant of Record y gestión de impuestos de venta. | Tarifa base usada en escenarios. No confirma elegibilidad, contrato, impuestos sobre beneficio ni todos los costes especiales. |
| [WorkOS pricing](https://workos.com/pricing) | AuthKit gratuito hasta un millón de usuarios; SSO y Directory Sync con "1–15 $125 / ea" cada uno. | Login gratuito no implica identidad enterprise gratuita. Hay descuentos por tramo; no multiplicar $125 por cualquier volumen como tarifa universal. |
| [Cloudflare R2 pricing](https://developers.cloudflare.com/r2/pricing/) | Storage Standard "$0.015 / GB-month" y transferencia de salida gratuita. | El almacenamiento de objetos tiene una tarifa pública. Operaciones, bases de datos, Git y toda la operación SaaS tienen costes aparte. |

Las páginas recuperadas y sus hashes se conservaron como material de trabajo fuera del repositorio. Estos enlaces y extractos permiten revisar las afirmaciones duraderas sin publicar volcados de HTML ni depender de rutas temporales.

## Fuentes primarias adicionales usadas por los investigadores

- [Solicitud de sync de configuración en Claude Code](https://github.com/anthropics/claude-code/issues/22648). Evidencia de un problema expresado. Un voto o comentario no es compra.
- [Configuración oficial de Claude Code](https://code.claude.com/docs/en/settings). Describe configuración local y controles del fabricante. No determina qué funcionalidades ofrecerá en el futuro.
- [chezmoi](https://www.chezmoi.io/). Alternativa libre para configuración entre máquinas; la familiaridad del usuario y la operación real deben investigarse.
- [Stripe subscription webhooks](https://docs.stripe.com/billing/subscriptions/webhooks). Respalda verificar eventos y manejar cambios de estado. Stripe es una referencia de protocolo, no el proveedor de cobro elegido por el escenario Paddle.
- [AWS tenant isolation](https://docs.aws.amazon.com/whitepapers/latest/saas-architecture-fundamentals/tenant-isolation.html). Explica por qué autenticación no basta para aislamiento. El documento advierte que es histórico; no lo usamos para elegir una arquitectura vigente de AWS.
- [Especificación age](https://github.com/C2SP/C2SP/blob/main/age.md). Describe el formato criptográfico. No prueba el diseño de enrolamiento, recuperación o navegador de Muster Cloud.
- [n8n Sustainable Use License](https://docs.n8n.io/sustainable-use-license/), [licencia Apache-2.0](https://www.apache.org/licenses/LICENSE-2.0), [Open Source Definition](https://opensource.org/osd). Distinguen el permiso comercial del núcleo y las restricciones del modelo de n8n. No sustituyen asesoría legal específica.

No incorporamos recuentos de estrellas o mercados estimados como justificación comercial. Tampoco dependemos de enlaces de issues adicionales cuya relevancia no se verificó en la síntesis. No encontrar evidencia de compra por Muster no significa demostrar que nadie compraría.

## Cobertura de la segunda ronda

Se lanzaron 30 frentes: diez Luna, diez Haiku y diez GLM 5.3 Flash. Cuatro tareas GLM agotaron cuota y tuvieron un único reintento con el mismo modelo y proveedor. Los cuatro reintentos terminaron y se recibieron 30 archivos distintos. Un archivo recibido no certifica sus afirmaciones.

| Modelo | Frentes recibidos | Cobertura |
|---|---|---|
| Luna | 01–10 | Frontera abierto y pago; transporte; alta; cifrado; estado; equipos; suscripción; compatibilidad; operación; v1 cobrable. |
| Haiku | 11–20 | Supabase; n8n; competencia directa; clientes locales; sync personal; comprador; señales públicas; proveedores; distribución; licencia. |
| GLM | 21–30 | Personal; Team; organizaciones mayores; comparación; precios; economía; validación; ciclo de vida; conversión desde OSS; refutación del negocio. |

El frente 24 y el 28 incluyen dominios de fuentes escritos en texto, sin URL completa. El recuento automático de enlaces los registra como cero. Sus flujos se usan como propuestas, no como certificación de cada afirmación externa.

El frente 30 aporta el argumento de que atraer usuarios por ausencia de cuentas no garantiza convertirlos a Cloud. Su descripción de CC Switch sin sync contradice la fuente actual y se descarta. Su precio de $9 y regla de cinco pagos son propuestas distintas, no resultados que deban imponerse a la síntesis.

Sol 6.1 revisó el plan en una pasada. Exigió separar firmas de la web, recuperación de cuenta y claves, secretos incrustados en configuración y continuidad verificable al exportar. Las cuatro correcciones se incorporan. Esa revisión no es una auditoría de seguridad ni una validación de mercado.

## Afirmaciones corregidas o excluidas

| Afirmación de uno o más informes | Tratamiento de la síntesis |
|---|---|
| La oferta Pro de Praxl ya cobra $5 por usuario. | La fuente muestra anuncio y lista de espera; no acredita cobro ni unidad por usuario. |
| No existen competidores integrados. | Búsqueda limitada y funciones solapadas. Se elimina la exclusividad. |
| CC Switch todavía tendría que añadir sincronización. | Ya publica WebDAV y S3. Se compara contra esa capacidad actual. |
| Los individuos solo pagarían $3 o $4. | Casos y precios ajenos no establecen un techo universal. |
| Una issue y precios de competidores prueban compradores o disposición a pagar. | Demuestran fricción y ofertas adyacentes. Las compras de Muster siguen sin observarse. |
| Conversión trial a pago del 20–30%, equilibrio con 15 clientes o construcción en cuatro semanas. | No sustentados por las fuentes consultadas. No se usan. |
| La arquitectura actual es zero-knowledge o cifra todo. | age cifra grupos; configuración y metadatos pueden ser legibles. Cloud requiere un diseño explícito. |
| Basta `sync --remote` para no modificar el motor o completar Cloud. | Faltan identidad, experiencia, conflictos, aislamiento, versiones y recuperación. |
| El estado firmado demuestra que todos aplicaron la configuración. | Hace falta resultado fresco por dispositivo. Incluso ese recibo no prueba obediencia del agente. |
| Revocar borra o invalida todos los secretos entregados. | Corta acceso futuro; la rotación externa es otra operación. |
| Un fabricante nunca sincronizará herramientas competidoras. | Predicción no demostrable. Se modela como riesgo. |
| Empresa mayor siempre exige SOC 2 antes de vender o un SaaS jamás requiere trabajo humano. | Los requisitos dependen de compradores y promesas. Incidentes y mantenimiento interno tienen coste. |
| Los márgenes o puntos de equilibrio de informes individuales son resultados medidos. | Se rehace el modelo en una sola moneda con supuestos y fórmula explícitos. |

## Fórmulas y reproducción de los escenarios

Todas las cantidades monetarias del CSV son USD mensuales. `price` es ingreso por cuenta; `accounts` cuenta personas en Personal y organizaciones en Team. Los escenarios son independientes, no sumables.

```text
fee = price * payment_rate + payment_fixed_usd
refund_reserve = price * refund_reserve_rate
support = ticket_rate_per_account_month * minutes_per_ticket / 60 * support_usd_per_hour
replacement_acquisition = churn * cac
contribution = price - fee - refund_reserve - support - variable_cloud - replacement_acquisition
fixed_cost = fixed_cloud_usd + fixed_tools_usd + fixed_labor_usd
breakeven = ceil(fixed_cost / contribution)
mrr = price * accounts
result = accounts * contribution - fixed_cost
```

`churn` es baja mensual de cuentas. `cac` es coste completo de adquirir una cuenta, incluidos tiempo o gasto asignados a esa adquisición. La fórmula solo cubre reemplazar bajas en una base estable; el crecimiento inicial exige efectivo adicional. `support` se calcula como coste variable y no vuelve a cargarse a personal fijo. `fixed_labor_usd` incluye mantenimiento y trabajo interno asignados, no otro sueldo que duplicar.

Para verificar la aritmética desde la raíz del repositorio:

```bash
python - <<'PY'
import csv
import math
from pathlib import Path

path = Path('docs/research/paid-product-economics.csv')
for row in csv.DictReader(path.open()):
    x = {k: float(v) for k, v in row.items() if k not in ('scenario', 'assumptions')}
    fee = x['price'] * x['payment_rate'] + x['payment_fixed_usd']
    support = x['ticket_rate_per_account_month'] * x['minutes_per_ticket'] / 60 * x['support_usd_per_hour']
    cm = x['price'] - fee - x['price'] * x['refund_reserve_rate'] - support - x['variable_cloud'] - x['churn'] * x['cac']
    fixed = x['fixed_cloud_usd'] + x['fixed_tools_usd'] + x['fixed_labor_usd']
    assert math.isclose(fee, x['fee'], abs_tol=0.0001)
    assert math.isclose(support, x['support'], abs_tol=0.0001)
    assert math.isclose(cm, x['contribution'], abs_tol=0.0001)
    assert fixed == x['fixed_cost']
    assert math.ceil(fixed / cm) == x['breakeven']
    assert x['price'] * x['accounts'] == x['mrr']
    assert math.isclose(x['accounts'] * cm - fixed, x['result'], abs_tol=0.01)
    print(row['scenario'], 'OK')
PY
```

El ingreso excluye impuestos de venta. La reserva del 1% es un supuesto de pérdidas, no una estadística sobre devoluciones. El resultado no es beneficio neto: excluye impuestos sobre beneficio, construcción y costes empresariales sin cotizar. El precio de una instancia o un GB no permite deducir capacidad ni disponibilidad sin medir cargas, réplicas, backups y recuperación.

## Qué no se hizo

No se construyó ni desplegó Cloud, se contactó a compradores, se cobraron suscripciones ni se observaron renovaciones. Tampoco se ejecutó el prototipo del transporte, la matriz real de compatibilidad ni una auditoría de seguridad. El entregable es una propuesta accionable y verificable para esas decisiones. Una implementación futura necesita especificación, aceptación y presupuesto propios.
