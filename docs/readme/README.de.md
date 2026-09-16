<p align="center">
  <img src=".github/assets/logo.svg" width="128" alt="hoverstare logo" />
  <h1 align="center">HoverStare</h1>
  <p align="center">
    <b>KI-Code-Review, das dein Repository wirklich liest.</b>
  </p>
  <p align="center">
    <i>Der Name stammt aus dem Filmgag „凌空瞪“ von Stephen Chow: ein losgelöster Augapfel, der in der Luft schwebt und einen anstarrt.</i>
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
    <b>Deutsch</b> ·
    <a href="README.es.md">Español</a>
  </p>
</p>

<br/>

HoverStare ist ein KI-Code-Review-Bot für GitHub-Pull-Requests, geschrieben in
Rust und ausgeliefert als einzelne statische Binärdatei, die als GitHub Action
läuft. Statt den Diff in einem Zug an ein Modell zu werfen, **liest der
Reviewer dein Repository wie ein Mensch** — öffnet Kontextdateien, sucht
Aufrufstellen per grep, vergleicht mit dem Basis-Branch — bevor er etwas
meldet. Ein Mehrfach-Voting plus unabhängiger Verifizierer hält falsch
Positive niedrig, und jeder Befund wird über Commits hinweg verfolgt, bis er
behoben ist.

## Warum HoverStare?

- 🔍 **Repo-bewusst statt nur Diff.** Das Modell bekommt schreibgeschützte
  Werkzeuge (`read_file` / `grep` / `glob` / `show_base_file`) und prüft
  Verdachtsfälle vor der Meldung nach. Es findet Bugs, die *außerhalb* des
  Diffs liegen — z. B. eine geänderte Funktion, deren Aufrufer zwei Dateien
  weiter brechen.
- 🗳️ **Mehrfach-Voting + Verifizierer.** Drei unabhängige Durchläufe
  (Korrektheit / Nebenläufigkeit / Sicherheit) stimmen über Befunde ab;
  Befunde mit nur einer Stimme müssen einen unabhängigen Verifizierer mit
  Werkzeugzugriff bestehen.
- 📌 **Präzise Inline-Kommentare.** Zeilennummern werden gegen den echten Diff
  validiert und auf den nächsten gültigen Ankerpunkt gesnappt — Kommentare
  landen exakt dort, wo der Bug ist.
- 🔁 **Inkrementelle Reviews.** Nach einem Fix reviewt HoverStare nur das Delta,
  markiert behobene Befunde als resolved (oder hinterlässt „✅ Fix bestätigt")
  und wiederholt sich nie.
- 🛡️ **Fail-open by Design.** Netzwerkprobleme, Rate-Limits oder ein
  unzuverlässiges Modell blockieren niemals deine CI.
- 🔑 **BYOK.** Eigener Schlüssel: Anthropic oder jeder OpenAI-kompatible
  Endpoint (Kimi, DeepSeek, OpenRouter, …). Code geht direkt an deinen
  Anbieter.

## Wie es funktioniert

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

Jeder Inline-Kommentar trägt einen versteckten Fingerabdruck (Hash aus
`Pfad + Codezeile + Titel`). Beim nächsten Push vergleicht HoverStare mit seinem
vorherigen Review, fragt das Modell, welche offenen Befunde behoben sind, und
behandelt diese Threads — immun gegen Zeilennummern-Drift.

## Schnellstart (2 Minuten)

**1. Workflow hinzufügen** — `.github/workflows/hoverstare.yml`:

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
      - uses: liuchong/hoverstare@v0.0.5
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          OPENAI_API_KEY: ${{ secrets.HOVERSTARE_LLM_KEY }}
          OPENAI_BASE_URL: ${{ vars.HOVERSTARE_LLM_BASE_URL }}
          HOVERSTARE_MODEL: ${{ vars.HOVERSTARE_MODEL }}   # z. B. kimi-for-coding
```

**2. LLM-Zugangsdaten konfigurieren** (eine Option):

| Anbieter | Einstellungen |
|---|---|
| **Anthropic** | Secret `ANTHROPIC_API_KEY` (Standardmodell `claude-sonnet-4-6`) |
| **OpenAI-kompatibel** (Kimi, DeepSeek, OpenRouter…) | Secret `OPENAI_API_KEY`, Variable `OPENAI_BASE_URL` (z. B. `https://api.kimi.com/coding/v1`), Modellname via `HOVERSTARE_MODEL` oder `model` in `.github/hoverstare.toml` |

> ⚠️ Bei einem OpenAI-kompatiblen Endpoint **musst** du den Modellnamen
> setzen — das Standardmodell `claude-sonnet-4-6` existiert dort nicht.

**3. (Optional) Repo-Konfiguration** — `.github/hoverstare.toml`, alle Felder optional:

```toml
model = "kimi-for-coding"             # Haupt-Review-Modell
reformat_model = "kimi-for-coding-highspeed"  # günstiges Modell für Ausgabe-Reparatur
passes = 3                            # parallele Durchläufe; 1 deaktiviert Voting
verify = true                         # Verifizierer für Ein-Stimmen-Befunde
severity_threshold = "medium"         # darunter → nur Nitpicks-Abschnitt
ignore = ["*.lock", "**/dist/**", "**/*.min.js"]
max_diff_kb = 400                     # Diff-Budget (prioritätsgesteuerte Kürzung)
max_tool_calls = 20                   # Werkzeug-Budget der Agentenschleife
timeout_secs = 900
review_drafts = false
fail_closed = false                   # true → Analysefehler lassen CI fehlschlagen
status_checks = false                 # hoverstare / hoverstare-findings Checks schreiben
language = "en"        # Ausgabesprache: en/zh-CN/ru/fr/de/es
set_temperature = true                # false für Endpoints, die nur Standard-Temperatur akzeptieren
thinking = "enabled"
reasoning_effort = "medium"
context_tokens = 1000000
instructions = ""                     # team-spezifischer Review-Fokus, wird in den Systemprompt injiziert
```

## Repository-Anweisungen

HoverStare liest Regeldateien des Repos und wendet sie auf Reviews an
(sie ergänzen, überschreiben aber niemals die eingebauten Kernregeln). Priorität:

1. `hoverstare.md` / `.hoverstare.md` / `.hoverstare/*.md` / `.github/hoverstare.md`
2. `AGENTS.md`
3. `.github/copilot-instructions.md`, `CLAUDE.md`, `.cursorrules`

Dateien werden **aus dem Basis-Branch** gelesen (ein PR, der AGENTS.md ändert,
kann keine Anweisungen einschleusen). Kern-Sicherheitsregeln (Read-only-Tools,
gezielte Verifikation, nur Defekte, JSON-Vertrag) sind nicht überschreibbar.

## Optional: Markenidentität (Veröffentlichung als eigener Bot)

Standardmäßig erscheinen Reviews als `github-actions[bot]` — eine
`GITHUB_TOKEN`-Einschränkung, und **der empfohlene Modus für die meisten**
(keine Extrakonfiguration).

Markenidentität gewünscht? Registriere **deine eigene** GitHub App
(5 Minuten, ohne Server — Token-Austausch läuft innerhalb von GitHub Actions):

1. Erstelle eine GitHub App unter *Settings → Developer settings → GitHub Apps*
   (Webhook **aus**; Berechtigungen: contents read, pull-requests write,
   issues write, commit statuses write) und installiere sie im Repo
2. Hinterlege App ID und private key als Secrets `APP_ID` / `APP_PRIVATE_KEY`
3. Übergib sie:

```yaml
      - uses: liuchong/hoverstare@v0.0.5
        with:
          app_id: ${{ secrets.APP_ID }}
          app_private_key: ${{ secrets.APP_PRIVATE_KEY }}
```

Reviews erscheinen dann als **deine-app[bot]**, und `resolveReviewThread`
funktioniert ohne die `GITHUB_TOKEN`-Einschränkung (kein `GH_PAT` nötig).

> Die Zero-Config-Identität `hoverstare[bot]` für alle ist als optionaler
> selbst-hostbarer Webhook-Dienst `hoverstare serve` geplant.

## `@hoverstare`-Befehle

In einem PR posten (nur Repo-Kollaboratoren):

| Befehl | Wirkung |
|---|---|
| `@hoverstare review` | Erzwingt ein komplettes Re-Review |
| `@hoverstare explain` | Antwortet im Thread mit einer verständlichen Erklärung des Befunds |
| Im Thread eines Befunds antworten | Den Befund mit HoverStare im Thread diskutieren — kein `@mention` nötig |
| `@hoverstare help` | Befehlsliste |

## Entwicklungsmodus: Issues & PRs als KI-IDE

HoverStare kann auch *entwickeln* — Issues und PRs werden zu einer dialoggesteuerten Entwicklungsumgebung (spec 11/12). Kein Automatisierungskleber, der auf Events reagiert, sondern ein Ort, an dem du das Feature wirklich von Anfang bis Ende baust.

### Der Weg eines Features

**Issue-Hauptlinie** — lege ein Issue mit `@hoverstare` an:

1. Es untersucht das Repo und antwortet mit Analyse + Plan (als Kommentar).
2. Diskutiere einfach durch Antworten; jede Runde wird im Thread beantwortet.
3. `@hoverstare go` — es erstellt einen Branch, implementiert, pusht und öffnet einen PR (mit `Closes #N`).

**PR-Hauptlinie** — auf jedem PR dieses Repos:

- `@hoverstare <Anweisung>` — es checkt den PR-Branch aus, entwickelt, committet (Conventional Commits, Autor `hoverstare[bot]`), pusht zurück auf den Branch und berichtet per Kommentar. Runden, die ihr Budget ausschöpfen, setzen sich selbst fort (max. 10 Runden pro PR).
- Menschen können per direktem Commit auf dem Branch nachjustieren — die nächste Runde synchronisiert immer zuerst mit dem Remote-Stand und überschreibt deine Commits nie.
- `@hoverstare merge` — sobald die Checks grün sind und keine Konflikte bestehen, mergt es per Squash **und löscht den Quell-Branch**.

### Einrichtung

1. **Workflow**: füge die Trigger `issues` und `pull_request_review` hinzu und vergib `contents: write` + `issues: write`. `.github/workflows/hoverstare.yml` in diesem Repo ist ein vollständiges Beispiel.
2. **Tokens** (zwei Aufgaben, zwei Tokens):
   - *Identität* (Kommentare, Reviews, PRs öffnen): das Standard-`GITHUB_TOKEN` oder ein GitHub-App-Token für die Identität `hoverstare[bot]`.
   - *Schreiboperationen* (git push, merge, Branch löschen): ein PAT über den Input `gh_pat` oder eine App mit `contents: write`. Pushes mit dem Standard-`GITHUB_TOKEN` lösen **keine** CI aus, Required Checks würden auf Bot-Commits nie laufen.
3. **Berechtigungen** (optional): deklariere in `.github/hoverstare.toml` `[permissions]`, wer was darf (spec 12). Defaults: Auto-Review für anyone, `review`/`develop` für collaborator, `merge` für write.

### Hinweise

- Nur Repo-Collaborators können Befehle erteilen; Fork-PRs sind ausgeschlossen.
- CI für vom Bot geöffnete PRs kann **1 workflow awaiting approval** zeigen.
  Das ist GitHubs Maintainer-Freigabe (siehe FAQ), kein fehlender LLM-Schlüssel.
- Große Aufgaben werden in budgetierte Runden geschnitten; der Bot setzt sich selbst fort (max. 10 Runden pro PR). Er kann keine Builds oder Tests ausführen — CI-Fehler werden als Anweisungen an die nächste Runde weitergereicht.

## FAQ

**Berechtigungsfehler beim Veröffentlichen?**
Prüfe die `permissions` im Workflow (`pull-requests: write` erforderlich) und
ob unter *Settings → Actions → General → Workflow permissions* "Read and
write" gesetzt ist.

**"model not found"?**
Du hast einen OpenAI-kompatiblen Endpoint, aber keinen Modellnamen gesetzt.
Setze `HOVERSTARE_MODEL` (oder `model` in `hoverstare.toml`).

**400 / invalid temperature?**
Dein Endpoint akzeptiert nur die Standard-Temperatur. Setze
`set_temperature = false` in `hoverstare.toml`.

**Behobene Befunde werden nicht aufgelöst?**
Eine Plattform-Einschränkung von GitHub: Der Standard-`GITHUB_TOKEN` kann
`resolveReviewThread` nicht aufrufen. HoverStare antwortet dann mit „✅ Fix
bestätigt" im Thread. Für vollständiges Resolve hinterlege einen klassischen
PAT (`repo`-Scope) als Secret `GH_PAT` und übergib ihn im Workflow-Env.

**GitHub Enterprise?**
Setze `GITHUB_API_URL=https://<dein-ghe-host>/api/v3`.

**`@hoverstare merge` schlägt mit 403 fehl?**
Der Merge-Endpunkt braucht `contents: write`. Übergib ein PAT über `gh_pat` oder gib der GitHub App Contents: Read and write (und akzeptiere das Upgrade in der Installation).

**Gelbes Banner „1 workflow awaiting approval“ / Checks bei action_required?**
Das ist GitHubs Freigabe-Tor für `pull_request`-Workflows, kein HoverStare-Fehler
und kein fehlendes Secret.

GitHub prüft **sowohl** den PR-Autor **als auch** den Actor des auslösenden
Events. Das sind verschiedene Konten: gemergte `hoverstare[bot]`-PRs vertrauen
nur der App-Identität (das Öffnen des PR zeigt das Banner nicht mehr). Ein
späterer Commit **aus einem Workflow** wird oft `github-actions[bot]`
zugeschrieben — ein anderes Konto ohne gemergten PR — daher verlangt die
Standardrichtlinie die Freigabe bei **jedem solchen Push**. Ein Ein-Commit-PR
trifft den zweiten Fall nicht; mit `GH_PAT` pusht der Collaborator, und
`continue`-Runden lösen das Tor nicht aus.

Erkennen: `gh run list --jq '.[]|select(.conclusion=="action_required")'`.
Entsperren: **Approve workflows to run**, oder
`gh api -X POST repos/OWNER/REPO/actions/runs/RUN_ID/approve`.
Vermeiden: Collaborator-PAT als `gh_pat`/`GH_PAT` für Push; Kommentare und PR
bleiben die App. Nur die App zum Pushen (der heutige Fallback) vereinheitlicht
den CI-Actor nicht — GitHub bleibt bei `github-actions[bot]`. Die gelockerte
Fork-PR-Richtlinie hat dieses Tor im selben Repo nicht übersprungen. Kein
`pull_request_target` nur zum Auto-Approven.

**Eine go/dev-Runde meldet „keine Änderungen, kein PR erstellt“?**
Meist war die Aufgabe zu vage für eine budgetierte Runde. Antworte mit einer schärferen, kleineren Anweisung (welche Dateien, Akzeptanzkriterien) und starte `go` erneut — im Kommentar des Bots steht, was er tatsächlich getan hat.

## Lokale Entwicklung

```bash
# Vollständiges Review eines öffentlichen PRs als Dry-Run (ohne Veröffentlichung)
export OPENAI_API_KEY=... OPENAI_BASE_URL=... HOVERSTARE_MODEL=...
cargo run -- review --repo owner/repo --pr 123 --dry-run

# Lokale Diff-Datei reviewen (gibt die Werkzeug-Aufrufspur aus)
cargo run --example local_review -- path/to.diff [base_ref]

cargo test                                   # Unit- + httpmock-Vertragstests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Specs und Meilensteinplan liegen in [`specs/`](specs/README.md) — die
Single Source of Truth für Design-Entscheidungen.

## Star-Verlauf & Mitwirkende

Täglich automatisch aktualisiert von [RepoScope](https://github.com/liuchong/reposcope) — Commits gehen in den Orphan-Branch `reposcope`, niemals in `master`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history-dark.svg">
  <img alt="Star History" src="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history.svg">
</picture>

![Contributors](https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/contributors.svg)

## Lizenz

[1PL — One Public License](https://license.pub/1pl/)
