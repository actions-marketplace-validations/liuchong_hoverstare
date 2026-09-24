<p align="center">
  <img src=".github/assets/logo.svg" width="128" alt="hoverstare logo" />
  <h1 align="center">HoverStare</h1>
  <p align="center">
    <b>Revisión de código con IA que realmente lee tu repositorio.</b>
  </p>
  <p align="center">
    <i>El nombre viene del gag de la película de Stephen Chow «凌空瞪»: un ojo incorpóreo flotando en el aire, mirándote fijamente.</i>
  </p>
  <p align="center">
    <a href="https://github.com/liuchong/hoverstare/actions/workflows/ci.yml"><img src="https://github.com/liuchong/hoverstare/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="https://github.com/liuchong/hoverstare/releases"><img src="https://img.shields.io/github/v/release/liuchong/hoverstare" alt="release" /></a>
    <a href="https://crates.io/crates/hoverstare"><img src="https://img.shields.io/crates/v/hoverstare" alt="crates.io" /></a>
    <a href="https://license.pub/1pl/"><img src="https://img.shields.io/badge/license-1PL-green" alt="license 1PL" /></a>
  </p>
  <p align="center">
    <a href="../../README.md">English</a> ·
    <a href="README.zh-CN.md">简体中文</a> ·
    <a href="README.ru.md">Русский</a> ·
    <a href="README.fr.md">Français</a> ·
    <a href="README.de.md">Deutsch</a> ·
    <b>Español</b>
  </p>
</p>

<br/>

HoverStare es un bot de revisión de código con IA para pull requests de GitHub,
escrito en Rust y distribuido como un único binario estático que se ejecuta
como GitHub Action. En lugar de lanzar el diff a un modelo de una sola vez, su
revisor **lee tu repositorio como lo haría un humano** — abre archivos de
contexto, busca sitios de llamada con grep, compara con la rama base — antes
de concluir. Una votación multipaso más un verificador independiente mantiene
bajos los falsos positivos, y cada hallazgo se rastrea entre commits hasta que
se corrige.

## ¿Por qué HoverStare?

- 🔍 **Consciente del repo, no solo del diff.** El modelo dispone de
  herramientas de solo lectura (`read_file` / `grep` / `glob` /
  `show_base_file`) y verifica sus sospechas antes de reportar. Detecta bugs
  que se esconden *fuera* del diff — como una función modificada cuyos
  invocadores se rompen dos archivos más allá.
- 🗳️ **Votación multipaso + verificador.** Tres pasadas independientes
  (corrección / concurrencia / seguridad) votan los hallazgos; los de un solo
  voto deben superar un verificador independiente con acceso a herramientas.
- 📌 **Comentarios en línea precisos.** Los números de línea se validan contra
  el diff real y se ajustan al ancla válida más cercana — los comentarios caen
  exactamente donde está el bug.
- 🔁 **Revisiones incrementales.** Al empujar una corrección, HoverStare revisa
  solo el delta, marca los hallazgos corregidos como resueltos (o deja una
  nota «✅ corrección confirmada») y nunca se repite.
- 🛡️ **Fail-open por diseño.** Problemas de red, límites de tasa o un modelo
  inestable nunca bloquearán tu CI.
- 🔑 **BYOK.** Trae tu propia clave: Anthropic o cualquier endpoint compatible
  con OpenAI (Kimi, DeepSeek, OpenRouter, …). El código va directo a tu
  proveedor.

## Cómo funciona

```mermaid
flowchart LR
    A[PR opened / synchronized] --> B{skip?}
    B -->|draft / bot / empty diff| Z((exit 0))
    B --> C[fetch diff]
    C --> D{prior review?}
    D -->|yes| E[delta diff]
    D -->|no| F[full diff]
    E --> G
    F --> G["N parallel review passes<br/>(read-only repo tools)"]
    G --> H[cluster & vote]
    H --> I[verifier pass]
    I --> J[validate & anchor lines]
    J --> K["post review<br/>+ resolve fixed threads<br/>+ status checks"]
```

Cada comentario en línea lleva una huella oculta (hash de
`ruta + línea de código + título`). En el siguiente push, HoverStare compara con su
revisión anterior, pregunta al modelo qué hallazgos abiertos están corregidos
y procesa esos hilos — inmune a la deriva de números de línea.

## Inicio rápido (2 minutos)

**1. Añade el workflow** — `.github/workflows/hoverstare.yml`:

```yaml
name: HoverStare
on:
  pull_request:
    types: [opened, reopened, synchronize]
  issue_comment:
    types: [created]
  pull_request_review_comment:
    types: [created]

permissions:
  contents: read
  pull-requests: write
  statuses: write

concurrency:
  # 不含 @hoverstare 的评论事件给独立组名，避免无意义的 run 取消正在跑的审查
  group: >-
    hoverstare-${{
      (github.event_name == 'issue_comment' || github.event_name == 'pull_request_review_comment')
      && !contains(github.event.comment.body, '@hoverstare')
      && format('noop-{0}', github.event.comment.id)
      || (github.event.pull_request.number || github.event.issue.number)
    }}
  cancel-in-progress: true

jobs:
  hoverstare:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: liuchong/hoverstare@v0.1.1
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          OPENAI_API_KEY: ${{ secrets.HOVERSTARE_LLM_KEY }}
          OPENAI_BASE_URL: ${{ vars.HOVERSTARE_LLM_BASE_URL }}
          HOVERSTARE_MODEL: ${{ vars.HOVERSTARE_MODEL }}   # p. ej. kimi-for-coding
```

**2. Configura las credenciales LLM** (elige una):

| Proveedor | Configuración |
|---|---|
| **Anthropic** | secreto `ANTHROPIC_API_KEY` (modelo por defecto `claude-sonnet-4-6`) |
| **Compatible con OpenAI** (Kimi, DeepSeek, OpenRouter…) | secreto `OPENAI_API_KEY`, variable `OPENAI_BASE_URL` (p. ej. `https://api.kimi.com/coding/v1`), nombre del modelo vía `HOVERSTARE_MODEL` o `model` en `.github/hoverstare.toml` |

> ⚠️ Con un endpoint compatible con OpenAI **debes** definir el nombre del
> modelo — el predeterminado `claude-sonnet-4-6` no existe ahí.

**3. (Opcional) Config del repo** — `.github/hoverstare.toml`, todos los campos opcionales:

```toml
model = "kimi-for-coding"             # modelo principal de revisión
reformat_model = "kimi-for-coding-highspeed"  # modelo barato para reparar la salida
passes = 3                            # pasadas en paralelo; 1 desactiva la votación
verify = true                         # verificador para hallazgos de un solo voto
severity_threshold = "medium"         # por debajo → solo sección Nitpicks
ignore = ["*.lock", "**/dist/**", "**/*.min.js"]
max_diff_kb = 400                     # presupuesto de diff (truncado por prioridad)
max_tool_calls = 20                   # presupuesto de llamadas a herramientas
timeout_secs = 900
review_drafts = false
fail_closed = false                   # true → los fallos de análisis rompen la CI
status_checks = false                 # escribir checks hoverstare / hoverstare-findings
report_coverage = true                # indicar la cobertura (unidades revisadas/total) en el cuerpo de la revisión
select_strict = false                 # omitir también rutas secretas, vendor/artefactos y tipos desconocidos
group_units = false                   # agrupar variantes de idioma / pares de migración / archivos de prueba en una unidad
rule_packs = true                   # inyectar puntos de control según el tipo de archivo
max_rule_packs = 2                    # máximo de paquetes por archivo
language = "en"        # idioma de salida: en/zh-CN/ru/fr/de/es
set_temperature = true                # false para endpoints que solo aceptan la temperatura por defecto
thinking = "enabled"
reasoning_effort = "medium"
context_tokens = 1000000
compaction = true
compaction_threshold_ratio = 0.75
compaction_keep_ratio = 0.25
summary_max_chars = 4000
max_rounds = 0
max_output_tokens = 0                 # límite de salida por llamada; 0 = derivado de la ventana (window/16, mín 4096, máx 65536); los tokens de razonamiento cuentan
commit_identity = "coauthor"          # identidad de commit: author (el disparador) / bot (hoverstare[bot]) / coauthor (disparador + Co-authored-by); por defecto coauthor
commit_author = ""                   # opcional "Name <email>"; vacío usa <login>@users.noreply.github.com
instructions = ""                     # enfoque de revisión del equipo, inyectado en el prompt de sistema
```

## Instrucciones del repositorio

HoverStare lee los archivos de reglas del repositorio y los aplica a las revisiones
(complementan pero nunca anulan las reglas integradas). Prioridad:

1. `hoverstare.md` / `.hoverstare.md` / `.hoverstare/*.md` / `.github/hoverstare.md`
2. `AGENTS.md`
3. `.github/copilot-instructions.md`, `CLAUDE.md`, `.cursorrules`

Los archivos se leen **desde la rama base** (un PR que edite AGENTS.md no puede
inyectar instrucciones). Las reglas de seguridad (herramientas de solo lectura,
verificación dirigida, solo defectos, contrato JSON) no se pueden anular.

## Opcional: identidad de marca (publicación como tu propio bot)

Por defecto, las revisiones se publican como `github-actions[bot]` — limitación
del `GITHUB_TOKEN`, y **es el modo recomendado para la mayoría** (cero config).

¿Quieres identidad de marca? Registra **tu propia** GitHub App
(5 minutos, sin servidor — el intercambio de tokens ocurre dentro de GitHub Actions):

1. Crea una GitHub App en *Settings → Developer settings → GitHub Apps*
   (webhook **desactivado**; permisos: contents read, pull-requests write,
   issues write, commit statuses write) e instálala en tu repo
2. Guarda su App ID y clave privada como secretos `APP_ID` / `APP_PRIVATE_KEY`
3. Pásalos:

```yaml
      - uses: liuchong/hoverstare@v0.1.1
        with:
          app_id: ${{ secrets.APP_ID }}
          app_private_key: ${{ secrets.APP_PRIVATE_KEY }}
```

Las revisiones se publican como **tu-app[bot]**, y `resolveReviewThread`
funciona sin la limitación del `GITHUB_TOKEN` (sin necesidad de `GH_PAT`).

> La identidad `hoverstare[bot]` sin configuración para todos está planeada
> como servicio webhook autoalojable opcional `hoverstare serve`.

## Comandos `@hoverstare`

Publica en un PR (solo colaboradores del repo):

| Comando | Qué hace |
|---|---|
| `@hoverstare review` | Fuerza una revisión completa |
| `@hoverstare explain` | Responde en el hilo con una explicación sencilla del hallazgo |
| Responder en el hilo de un hallazgo | Discutir el hallazgo con HoverStare en el hilo — no hace falta `@mention` |
| `@hoverstare help` | Lista de comandos |

## Modo desarrollo: issues y PRs como tu IDE de IA

HoverStare también puede *desarrollar* — los issues y PRs se convierten en un entorno de desarrollo guiado por conversación (spec 11/12). No es un pegamento de automatización que reacciona a eventos: es un lugar donde realmente construyes la funcionalidad, de principio a fin.

### El recorrido de una funcionalidad

**Línea de issue** — abre un issue mencionando `@hoverstare`:

1. Investiga el repo y responde con un análisis + plan (en comentarios).
2. Conversa simplemente respondiendo; cada ronda se contesta en el hilo.
3. `@hoverstare go` — crea una rama, implementa, empuja y abre un PR (con `Closes #N`).

**Línea de PR** — en cualquier PR de este repo:

- `@hoverstare <instrucción>` — cambia a la rama del PR, desarrolla, hace commit (Conventional Commits, autor `hoverstare[bot]`), empuja de vuelta a la rama y reporta en un comentario. Las rondas que agotan el presupuesto se autocontinúan (máx. 10 rondas por PR).
- Los humanos pueden ajustar con commits directos en la rama — la siguiente ronda siempre sincroniza primero con la cabeza remota y nunca sobrescribe tus commits.
- `@hoverstare merge` — cuando los checks están en verde y no hay conflictos, fusiona con squash **y elimina la rama de origen**.

### Configuración

1. **Workflow**: añade los disparadores `issues` y `pull_request_review` y concede `contents: write` + `issues: write`. `.github/workflows/hoverstare.yml` en este repo es un ejemplo completo.
2. **Tokens** (dos roles, dos tokens):
   - *Identidad* (comentarios, reviews, abrir PRs): el `GITHUB_TOKEN` por defecto, o un token de GitHub App para la identidad `hoverstare[bot]`.
   - *Escritura* (git push, merge, borrar rama): un PAT con la entrada `gh_pat`, o una App con `contents: write`. Los pushes con el `GITHUB_TOKEN` por defecto **no** disparan CI, así que los checks requeridos nunca correrían en los commits del bot.
3. **Permisos** (opcional): declara quién puede qué en `.github/hoverstare.toml` `[permissions]` (spec 12). Por defecto: auto-review para anyone, `review`/`develop` para collaborator, `merge` para write.

### Notas

- Solo los colaboradores del repo pueden dar comandos; los PRs de fork están fuera de alcance.
- Los PRs abiertos por el bot pueden mostrar **1 workflow awaiting approval** en
  las comprobaciones `pull_request`. Es la puerta de aprobación de GitHub (FAQ
  más abajo), no una clave LLM ausente.
- Las tareas grandes se dividen en rondas presupuestadas; el bot se autocontinúa (máx. 10 rondas por PR). No puede ejecutar builds ni tests — los fallos de CI se transmiten como instrucciones a la siguiente ronda.

## Preguntas frecuentes

**¿Errores de permisos al publicar?**
Revisa los `permissions` del workflow (`pull-requests: write` requerido) y que
*Settings → Actions → General → Workflow permissions* esté en "Read and write".

**¿"model not found"?**
Configuraste un endpoint compatible con OpenAI pero no el nombre del modelo.
Define `HOVERSTARE_MODEL` (o `model` en `hoverstare.toml`).

**¿400 / invalid temperature?**
Tu endpoint solo acepta la temperatura por defecto. Pon
`set_temperature = false` en `hoverstare.toml`.

**¿Los hallazgos corregidos no se resuelven?**
Una limitación de la plataforma GitHub: el `GITHUB_TOKEN` por defecto no puede
llamar a `resolveReviewThread`. HoverStare responde entonces «✅ corrección
confirmada» en el hilo. Para resolución completa, guarda un PAT clásico
(`repo` scope) como secreto `GH_PAT` y pásalo en el env del workflow.

**¿GitHub Enterprise?**
Define `GITHUB_API_URL=https://<tu-host-ghe>/api/v3`.

**¿`@hoverstare merge` falla con 403?**
El endpoint de merge requiere `contents: write`. Pasa un PAT con `gh_pat`, o concede a la GitHub App Contents: Read and write (y acepta la actualización en la instalación).

**¿Banner amarillo "1 workflow awaiting approval" / CI en action_required?**
Es la puerta de aprobación de GitHub para workflows `pull_request`, no un fallo
de HoverStare ni un secreto que falte.

GitHub comprueba **tanto** el autor del PR **como** el actor del evento que
arrancó el run. Son usuarios distintos: fusionar PRs de `hoverstare[bot]`
confía esa identidad de App (abrir el PR ya no muestra el banner). Un commit
posterior empujado **desde un workflow** suele atribuirse a
`github-actions[bot]`, otra cuenta que nunca ha mergeado un PR, así que la
política por defecto pide aprobación **en cada push de ese tipo**. Un PR de
un solo commit no llega al segundo caso; con `GH_PAT`, el push es el
colaborador y las rondas `continue` tampoco disparan la puerta.

Detectar: `gh run list --jq '.[]|select(.conclusion=="action_required")'`.
Desbloquear: **Approve workflows to run**, o
`gh api -X POST repos/OWNER/REPO/actions/runs/RUN_ID/approve`.
Evitar: PAT de colaborador en `gh_pat`/`GH_PAT` para el push; la App sigue
siendo la identidad de comentarios y del PR. Empujar solo con la App (el
fallback actual) no unifica el actor de CI: GitHub sigue registrando
`github-actions[bot]`. Relajar la política de fork-PR no ha saltado esta
puerta en el mismo repositorio. No uses `pull_request_target` para autoaprobar.

**¿Una ronda go/dev responde "sin cambios, no se creó PR"?**
Normalmente la tarea era demasiado vaga para una ronda presupuestada. Responde con una instrucción más concreta y pequeña (qué archivos tocar, criterios de aceptación) y repite `go` — el comentario del bot dice lo que realmente hizo.

## Desarrollo local

```bash
# Ver la selección sin llamar al modelo (no hace falta clave de API)
cargo run -- review --repo owner/repo --pr 123 --preview
cargo run -- review --repo owner/repo --pr 123 --preview --format json

# Resultado legible por máquina de una ejecución real (contrato: specs/16)
cargo run -- review --repo owner/repo --pr 123 --format json
cargo run -- review --repo owner/repo --pr 123 --format sarif --output out/result.sarif
# Dry-run de una revisión completa de un PR público (sin publicar)
export OPENAI_API_KEY=... OPENAI_BASE_URL=... HOVERSTARE_MODEL=...
cargo run -- review --repo owner/repo --pr 123 --dry-run

# Revisar un archivo diff local (imprime la traza de llamadas a herramientas)
cargo run --example local_review -- path/to.diff [base_ref]

cargo test                                   # tests unitarios + de contrato httpmock
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Las specs y el plan de hitos están en [`specs/`](../../specs/README.md) — la fuente
única de verdad para las decisiones de diseño.

## Contribuir

Las comprobaciones de calidad, la convención de mensajes de commit y el proceso de revisión de PR están en [`CONTRIBUTING.md`](../../CONTRIBUTING.md).

## Historial de estrellas y colaboradores

Actualizado diariamente por [RepoScope](https://github.com/liuchong/reposcope) — commits en la rama huérfana `reposcope`, nunca en `master`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history-dark.svg">
  <img alt="Star History" src="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history.svg">
</picture>

![Contributors](https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/contributors.svg)

## Licencia

[1PL — One Public License](https://license.pub/1pl/)
