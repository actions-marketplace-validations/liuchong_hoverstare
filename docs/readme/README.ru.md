<p align="center">
  <img src=".github/assets/logo.svg" width="128" alt="hoverstare logo" />
  <h1 align="center">HoverStare</h1>
  <p align="center">
    <b>ИИ-ревью кода, которое действительно читает ваш репозиторий.</b>
  </p>
  <p align="center">
    <i>Название — от гэга из фильма Стивена Чоу «凌空瞪»: отделённый от тела глаз, парящий в воздухе и пристально смотрящий на вас.</i>
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
    <b>Русский</b> ·
    <a href="README.fr.md">Français</a> ·
    <a href="README.de.md">Deutsch</a> ·
    <a href="README.es.md">Español</a>
  </p>
</p>

<br/>

HoverStare — это ИИ-бот для код-ревью пул-реквестов GitHub, написанный на Rust и
поставляемый как единый статический бинарник, работающий как GitHub Action.
Вместо того чтобы отправить diff модели за один проход, ревьюер **читает ваш
репозиторий, как это сделал бы человек** — открывает файлы с контекстом, ищет
места вызова через grep, сравнивает с базовой веткой — и только потом делает
выводы. Многопроходное голосование и независимый верификатор снижают число
ложных срабатываний, а каждая найденная проблема отслеживается между коммитами
до её исправления.

## Почему HoverStare?

- 🔍 **Понимает репозиторий, а не только diff.** У модели есть набор
  read-only инструментов (`read_file` / `grep` / `glob` / `show_base_file`),
  и она проверяет подозрения перед отчётом. Находит баги *за пределами* diff —
  например, когда изменённая функция ломает вызывающий её код в других файлах.
- 🗳️ **Многопроходное голосование + верификатор.** Три независимых прохода
  (корректность / конкурентность / безопасность) голосуют по находкам;
  находки с одним голосом проходят независимую проверку с доступом к инструментам.
- 📌 **Точные встроенные комментарии.** Номера строк проверяются по реальному
  diff и привязываются к ближайшей допустимой строке — комментарии попадают
  именно туда, где находится баг.
- 🔁 **Инкрементальные ревью.** После пуша исправления HoverStare проверяет только
  дельту, отмечает исправленные находки как resolved (или оставляет
  «✅ исправление подтверждено») и никогда не повторяется.
- 🛡️ **Fail-open по дизайну.** Сетевые сбои, рейт-лимиты или нестабильная
  модель никогда не заблокируют ваш CI.
- 🔑 **BYOK.** Свой ключ: Anthropic или любой OpenAI-совместимый endpoint
  (Kimi, DeepSeek, OpenRouter, …). Код идёт напрямую к вашему провайдеру.

## Как это работает

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

Каждый встроенный комментарий содержит скрытый отпечаток (хэш
`путь + строка кода + заголовок`). При следующем пуше HoverStare сравнивает со
своим прошлым ревью, спрашивает у модели, какие открытые находки исправлены,
и обрабатывает эти треды — дрейф номеров строк не влияет на отпечаток.

## Быстрый старт (2 минуты)

**1. Добавьте workflow** — `.github/workflows/hoverstare.yml`:

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
          HOVERSTARE_MODEL: ${{ vars.HOVERSTARE_MODEL }}   # например kimi-for-coding
```

**2. Настройте доступ к LLM** (на выбор):

| Провайдер | Настройки |
|---|---|
| **Anthropic** | секрет `ANTHROPIC_API_KEY` (модель по умолчанию `claude-sonnet-4-6`) |
| **OpenAI-совместимый** (Kimi, DeepSeek, OpenRouter…) | секрет `OPENAI_API_KEY`, переменная `OPENAI_BASE_URL` (например `https://api.kimi.com/coding/v1`), имя модели через `HOVERSTARE_MODEL` или `model` в `.github/hoverstare.toml` |

> ⚠️ Для OpenAI-совместимого endpoint **обязательно** укажите имя модели —
> модели по умолчанию `claude-sonnet-4-6` там не существует.

**3. (Опционально) Конфиг репозитория** — `.github/hoverstare.toml`, все поля необязательны:

```toml
model = "kimi-for-coding"             # основная модель ревью
reformat_model = "kimi-for-coding-highspeed"  # дешёвая модель для восстановления вывода
passes = 3                            # параллельные проходы; 1 отключает голосование
verify = true                         # верификатор для находок с одним голосом
severity_threshold = "medium"         # ниже порога — только раздел Nitpicks
ignore = ["*.lock", "**/dist/**", "**/*.min.js"]
max_diff_kb = 400                     # бюджет diff (усечение по приоритету)
max_tool_calls = 20                   # бюджет вызовов инструментов
timeout_secs = 900
review_drafts = false
fail_closed = false                   # true — ошибки анализа ломают CI
status_checks = false                 # писать проверки hoverstare / hoverstare-findings
report_coverage = true                # указывать покрытие (единиц проверено/всего) в теле обзора
select_strict = false                 # дополнительно пропускать секретные пути, vendor/артефакты, неизвестные типы
group_units = false                   # объединять языковые варианты / пары миграций / тестовые файлы в одну единицу
language = "en"        # язык вывода: en/zh-CN/ru/fr/de/es
set_temperature = true                # false для endpoint'ов, принимающих только температуру по умолчанию
thinking = "enabled"
reasoning_effort = "medium"
context_tokens = 1000000
compaction = true
compaction_threshold_ratio = 0.75
compaction_keep_ratio = 0.25
summary_max_chars = 4000
max_rounds = 0
max_output_tokens = 0                 # лимит вывода на один вызов; 0 = из окна (window/16, мин. 4096, макс. 65536); токены рассуждений учитываются
commit_identity = "coauthor"          # идентичность commit в режиме разработки: author (триггер) / bot (hoverstare[bot]) / coauthor (триггер + Co-authored-by trailer); по умолчанию coauthor
commit_author = ""                   # необязательное "Name <email>"; пусто = <login>@users.noreply.github.com
instructions = ""                     # фокус ревью команды, добавляется в системный промпт
```

## Инструкции репозитория

HoverStare читает файлы правил репозитория и применяет их к обзорам
(они дополняют, но никогда не переопределяют встроенные правила). Приоритет:

1. `hoverstare.md` / `.hoverstare.md` / `.hoverstare/*.md` / `.github/hoverstare.md`
2. `AGENTS.md`
3. `.github/copilot-instructions.md`, `CLAUDE.md`, `.cursorrules`

Файлы читаются **из базовой ветки** (PR, меняющий AGENTS.md, не может
внедрить инструкции). Основные правила безопасности (read-only инструменты,
точечная проверка, только дефекты, JSON-контракт) не переопределяются.

## Опционально: фирменная личность (публикации от вашего бота)

По умолчанию ревью публикуются от `github-actions[bot]` — ограничение
`GITHUB_TOKEN`, и это **рекомендуемый режим для большинства** (без лишней настройки).

Хотите фирменную личность? Зарегистрируйте **собственное** GitHub App
(5 минут, сервер не нужен — обмен токенов происходит внутри GitHub Actions):

1. Создайте GitHub App в *Settings → Developer settings → GitHub Apps*
   (webhook **выключен**; права: contents read, pull-requests write,
   issues write, commit statuses write) и установите в репозиторий
2. Сохраните App ID и private key как секреты `APP_ID` / `APP_PRIVATE_KEY`
3. Передайте:

```yaml
      - uses: liuchong/hoverstare@v0.1.1
        with:
          app_id: ${{ secrets.APP_ID }}
          app_private_key: ${{ secrets.APP_PRIVATE_KEY }}
```

Ревью будут публиковаться от **ваш-app[bot]**, а `resolveReviewThread`
работает без ограничения `GITHUB_TOKEN` (без `GH_PAT`).

> Нулевая настройка личности `hoverstare[bot]` для всех — в планах как
> опциональный self-hosted сервис `hoverstare serve`.

## Команды `@hoverstare`

В комментариях к PR (только для коллабораторов репозитория):

| Команда | Действие |
|---|---|
| `@hoverstare review` | Принудительный полный повторный ревью |
| `@hoverstare explain` | Ответить в треде простым объяснением находки |
| Ответ в треде находки | Обсудить находку с HoverStare в треде — `@mention` не требуется |
| `@hoverstare help` | Список команд |

## Режим разработки: issues и PR как AI-IDE

HoverStare умеет и *разрабатывать* — issues и PR превращаются в среду разработки, управляемую диалогом (spec 11/12). Это не автоматический клей, реагирующий на события, а место, где вы действительно строите фичу от начала до конца.

### Как идёт фича

**Линия issue** — создайте issue с `@hoverstare`:

1. Он исследует репозиторий и ответит анализом + планом (в комментариях).
2. Обсуждайте, просто отвечая; каждый раунд получает ответ в треде.
3. `@hoverstare go` — он создаст ветку, реализует, запушит и откроет PR (с `Closes #N`).

**Линия PR** — на любом PR из этого репозитория:

- `@hoverstare <инструкция>` — он переключится на ветку PR, разработает, закоммитит (Conventional Commits, автор `hoverstare[bot]`), запушит обратно в ветку и отчитается комментарием. Раунды, исчерпавшие бюджет, продолжаются автоматически (максимум 10 раундов на PR).
- Люди могут править прямыми коммитами в ветку — следующий раунд всегда сначала синхронизируется с удалённой головой и никогда не перезапишет ваши коммиты.
- `@hoverstare merge` — когда проверки зелёные и нет конфликтов, он сделает squash-merge **и удалит исходную ветку**.

### Настройка

1. **Workflow**: добавьте триггеры `issues` и `pull_request_review` и выдайте `contents: write` + `issues: write`. Полный рабочий пример — `.github/workflows/hoverstare.yml` в этом репозитории.
2. **Токены** (две роли, два токена):
   - *Идентичность* (комментарии, ревью, открытие PR): обычный `GITHUB_TOKEN` или токен GitHub App для личности `hoverstare[bot]`.
   - *Операции записи* (git push, merge, удаление ветки): PAT через вход `gh_pat` или App с `contents: write`. Пуши обычным `GITHUB_TOKEN` **не** запускают CI, и обязательные проверки на коммитах бота никогда не выполнятся.
3. **Разрешения** (опционально): объявите, кто что может, в `.github/hoverstare.toml` `[permissions]` (spec 12). По умолчанию: auto_review для anyone, `review`/`develop` для collaborator, `merge` для write.

### Примечания

- Команды может отдавать только коллаборатор репозитория; fork PR не поддерживаются.
- CI для PR, созданных ботом, может показать **1 workflow awaiting approval**.
  Это шлюз GitHub для maintainer (см. FAQ), а не отсутствующий ключ LLM.
- Большие задачи делятся на бюджетные раунды; бот продолжается сам (максимум 10 раундов на PR). Он не может запускать сборку или тесты — падения CI передаются как инструкции следующему раунду.

## Частые вопросы

**Ошибки прав при публикации ревью?**
Проверьте `permissions` в workflow (нужен `pull-requests: write`) и что в
*Settings → Actions → General → Workflow permissions* выбрано "Read and write".

**"model not found"?**
Вы настроили OpenAI-совместимый endpoint, но не указали имя модели.
Задайте `HOVERSTARE_MODEL` (или `model` в `hoverstare.toml`).

**400 / invalid temperature?**
Endpoint принимает только температуру по умолчанию.
Установите `set_temperature = false` в `hoverstare.toml`.

**`@hoverstare merge` падает с 403?**
Эндпоинт merge требует `contents: write`. Передайте PAT через `gh_pat` или выдайте GitHub App права Contents: Read and write (и примите обновление в установке).

**Жёлтая плашка «1 workflow awaiting approval» / проверки в action_required?**
Это шлюз одобрения GitHub для workflow `pull_request`, а не ошибка HoverStare и
не пропавший секрет.

GitHub проверяет **и** автора PR, **и** актора события, запустившего run. Это
разные аккаунты: смерженные PR от `hoverstare[bot]` доверяют только
идентичности App (открытие PR больше не показывает плашку). Следующий commit,
запушенный **из workflow**, часто приписывается `github-actions[bot]` — другой
аккаунт без смерженного PR — поэтому политика по умолчанию снова просит
одобрение **на каждом таком push**. PR из одного commit не попадает во второй
случай; с `GH_PAT` push делает коллаборатор, и раунды `continue` шлюз не
включают.

Обнаружить: `gh run list --jq '.[]|select(.conclusion=="action_required")'`.
Разблокировать: **Approve workflows to run** или
`gh api -X POST repos/OWNER/REPO/actions/runs/RUN_ID/approve`.
Предотвратить: PAT коллаборатора в `gh_pat`/`GH_PAT` для push; комментарии и PR
остаются App. Пуш только токеном App (текущий fallback) не унифицирует актора
CI — GitHub всё равно пишет `github-actions[bot]`. Ослабленная политика fork-PR
этот шлюз в том же репозитории не снимала. Не используйте `pull_request_target`
для автоодобрения.

**Раунд go/develop отвечает «нет изменений, PR не создан»?**
Обычно задача слишком размыта для одного бюджетного раунда. Ответьте более точной и малой инструкцией (какие файлы трогать, критерии приёмки) и повторите `go` — в комментарии бота написано, что он реально сделал.

**Исправленные находки не закрываются?**
Ограничение платформы GitHub: стандартный `GITHUB_TOKEN` не может вызывать
`resolveReviewThread`. HoverStare отвечает в треде «✅ исправление подтверждено».
Для полного resolve сохраните classic PAT (`repo` scope) как секрет `GH_PAT`
и передайте его в env workflow.

**GitHub Enterprise?**
Установите `GITHUB_API_URL=https://<ваш-ghe-хост>/api/v3`.

## Локальная разработка

```bash
# Полный dry-run ревью публичного PR (без публикации)
export OPENAI_API_KEY=... OPENAI_BASE_URL=... HOVERSTARE_MODEL=...
cargo run -- review --repo owner/repo --pr 123 --dry-run

# Ревью локального diff-файла (печатает трассу вызовов инструментов)
cargo run --example local_review -- path/to.diff [base_ref]

cargo test                                   # модульные + контрактные тесты httpmock
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Спецификации и план вех — в [`specs/`](../../specs/README.md), единственный
источник правды по проектным решениям.

## История звёзд и участники

Автоматически обновляется ежедневно с помощью [RepoScope](https://github.com/liuchong/reposcope) — коммиты в orphan-ветку `reposcope`, никогда в `master`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history-dark.svg">
  <img alt="Star History" src="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history.svg">
</picture>

![Contributors](https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/contributors.svg)

## Лицензия

[1PL — One Public License](https://license.pub/1pl/)
