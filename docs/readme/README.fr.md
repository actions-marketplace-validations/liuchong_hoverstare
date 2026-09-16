<p align="center">
  <img src=".github/assets/logo.svg" width="128" alt="hoverstare logo" />
  <h1 align="center">HoverStare</h1>
  <p align="center">
    <b>La revue de code IA qui lit vraiment votre dépôt.</b>
  </p>
  <p align="center">
    <i>Le nom vient du gag du film de Stephen Chow « 凌空瞪 » : un œil désincarné flottant dans les airs qui vous fixe.</i>
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
    <b>Français</b> ·
    <a href="README.de.md">Deutsch</a> ·
    <a href="README.es.md">Español</a>
  </p>
</p>

<br/>

HoverStare est un bot de revue de code IA pour les pull requests GitHub, écrit en
Rust et distribué comme un binaire statique unique s'exécutant en tant que
GitHub Action. Au lieu d'envoyer le diff à un modèle en une seule passe, son
réviseur **lit votre dépôt comme le ferait un humain** — il ouvre les fichiers
de contexte, cherche les sites d'appel avec grep, compare avec la branche de
base — avant de conclure. Un vote multi-passes et un vérificateur indépendant
réduisent les faux positifs, et chaque anomalie signalée est suivie d'un
commit à l'autre jusqu'à sa correction.

## Pourquoi HoverStare ?

- 🔍 **Conscient du dépôt, pas seulement du diff.** Le modèle dispose d'outils
  en lecture seule (`read_file` / `grep` / `glob` / `show_base_file`) et
  vérifie ses soupçons avant de signaler. Il détecte des bugs cachés *hors* du
  diff — comme une fonction modifiée dont les appelants cassent à deux fichiers
  de distance.
- 🗳️ **Vote multi-passes + vérificateur.** Trois passes indépendantes
  (exactitude / concurrence / sécurité) votent sur les anomalies ; celles à
  une seule voix doivent passer un vérificateur indépendant avec accès aux outils.
- 📌 **Commentaires en ligne précis.** Les numéros de ligne sont validés contre
  le vrai diff et ajustés à l'ancre valide la plus proche — les commentaires
  tombent exactement là où se trouve le bug.
- 🔁 **Revues incrémentales.** Poussez un correctif et HoverStare ne revoit que le
  delta, marque les anomalies corrigées comme résolues (ou laisse une note
  « ✅ correction confirmée ») et ne se répète jamais.
- 🛡️ **Fail-open par conception.** Problèmes réseau, limites de débit ou modèle
  capricieux ne bloqueront jamais votre CI.
- 🔑 **BYOK.** Apportez votre clé : Anthropic ou tout endpoint compatible
  OpenAI (Kimi, DeepSeek, OpenRouter, …). Le code va directement chez votre
  fournisseur.

## Comment ça marche

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

Chaque commentaire en ligne porte une empreinte cachée (hachage
`chemin + ligne de code + titre`). Au prochain push, HoverStare compare avec sa
revue précédente, demande au modèle quelles anomalies ouvertes sont corrigées,
et traite ces fils — insensible à la dérive des numéros de ligne.

## Démarrage rapide (2 minutes)

**1. Ajoutez le workflow** — `.github/workflows/hoverstare.yml` :

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
          HOVERSTARE_MODEL: ${{ vars.HOVERSTARE_MODEL }}   # ex. kimi-for-coding
```

**2. Configurez les identifiants LLM** (au choix) :

| Fournisseur | Paramètres |
|---|---|
| **Anthropic** | secret `ANTHROPIC_API_KEY` (modèle par défaut `claude-sonnet-4-6`) |
| **Compatible OpenAI** (Kimi, DeepSeek, OpenRouter…) | secret `OPENAI_API_KEY`, variable `OPENAI_BASE_URL` (ex. `https://api.kimi.com/coding/v1`), nom du modèle via `HOVERSTARE_MODEL` ou `model` dans `.github/hoverstare.toml` |

> ⚠️ Avec un endpoint compatible OpenAI, vous **devez** définir le nom du
> modèle — le défaut `claude-sonnet-4-6` n'existe pas là-bas.

**3. (Optionnel) Config du dépôt** — `.github/hoverstare.toml`, tous champs optionnels :

```toml
model = "kimi-for-coding"             # modèle principal de revue
reformat_model = "kimi-for-coding-highspeed"  # modèle bon marché pour la réparation de sortie
passes = 3                            # passes parallèles ; 1 désactive le vote
verify = true                         # vérificateur pour les anomalies à une voix
severity_threshold = "medium"         # en dessous → section Nitpicks uniquement
ignore = ["*.lock", "**/dist/**", "**/*.min.js"]
max_diff_kb = 400                     # budget de diff (troncature par priorité)
max_tool_calls = 20                   # budget d'appels d'outils
timeout_secs = 900
review_drafts = false
fail_closed = false                   # true → les échecs d'analyse font échouer la CI
status_checks = false                 # écrire les checks hoverstare / hoverstare-findings
language = "en"        # langue de sortie : en/zh-CN/ru/fr/de/es
set_temperature = true                # false pour les endpoints n'acceptant que la température par défaut
thinking = "enabled"
reasoning_effort = "medium"
context_tokens = 1000000
instructions = ""                     # focus de revue de l'équipe, injecté dans le prompt système
```

## Instructions du dépôt

HoverStare lit les fichiers de règles du dépôt et les applique aux revues
(ils complètent mais ne remplacent jamais les règles de base). Priorité :

1. `hoverstare.md` / `.hoverstare.md` / `.hoverstare/*.md` / `.github/hoverstare.md`
2. `AGENTS.md`
3. `.github/copilot-instructions.md`, `CLAUDE.md`, `.cursorrules`

Les fichiers sont lus **depuis la branche de base** (une PR modifiant
AGENTS.md ne peut pas injecter d'instructions). Les règles de sécurité de base
(outils en lecture seule, vérification ciblée, défauts uniquement, contrat JSON)
ne sont jamais remplacées.

## Option : identité de marque (publication sous votre bot)

Par défaut, les revues sont publiées en `github-actions[bot]` — limitation du
`GITHUB_TOKEN`, et **c'est le mode recommandé pour la plupart** (zéro config).

Vous voulez une identité de marque ? Enregistrez **votre propre** GitHub App
(5 minutes, sans serveur — l'échange de jetons a lieu dans GitHub Actions) :

1. Créez une GitHub App dans *Settings → Developer settings → GitHub Apps*
   (webhook **désactivé** ; permissions : contents read, pull-requests write,
   issues write, commit statuses write) et installez-la sur votre dépôt
2. Ajoutez son App ID et sa clé privée comme secrets `APP_ID` / `APP_PRIVATE_KEY`
3. Passez-les :

```yaml
      - uses: liuchong/hoverstare@v0.0.5
        with:
          app_id: ${{ secrets.APP_ID }}
          app_private_key: ${{ secrets.APP_PRIVATE_KEY }}
```

Les revues sont alors publiées en **votre-app[bot]**, et `resolveReviewThread`
fonctionne sans la limitation du `GITHUB_TOKEN` (pas besoin de `GH_PAT`).

> L'identité `hoverstare[bot]` sans configuration pour tous est prévue comme
> service webhook auto-hébergeable optionnel `hoverstare serve`.

## Commandes `@hoverstare`

Dans les commentaires d'une PR (collaborateurs du dépôt uniquement) :

| Commande | Effet |
|---|---|
| `@hoverstare review` | Force une re-revue complète |
| `@hoverstare explain` | Répond dans le fil avec une explication en langage clair |
| Répondre dans le fil d'une constat | Discuter de la constat avec HoverStare dans le fil — pas besoin de `@mention` |
| `@hoverstare help` | Liste des commandes |

## Mode développement : issues et PR comme IDE IA

HoverStare sait aussi *développer* — les issues et PR deviennent un environnement de développement piloté par la conversation (spec 11/12). Pas une glue d'automatisation qui réagit aux événements : un lieu où vous construisez réellement la fonctionnalité, de bout en bout.

### Le cycle d'une fonctionnalité

**Voie issue** — ouvrez une issue mentionnant `@hoverstare` :

1. Il explore le dépôt et répond par une analyse + un plan (en commentaires).
2. Discutez en répondant simplement ; chaque tour obtient une réponse dans le fil.
3. `@hoverstare go` — il crée une branche, implémente, pousse et ouvre une PR (avec `Closes #N`).

**Voie PR** — sur toute PR de ce dépôt :

- `@hoverstare <instruction>` — il bascule sur la branche de la PR, développe, committe (Conventional Commits, auteur `hoverstare[bot]`), pousse sur la branche et rend compte en commentaire. Les tours qui épuisent leur budget se relancent automatiquement (10 tours max par PR).
- Les humains peuvent ajuster par des commits directs sur la branche — le tour suivant se synchronise toujours d'abord sur la tête distante et n'écrase jamais vos commits.
- `@hoverstare merge` — une fois les checks verts et sans conflit, il fusionne en squash **et supprime la branche source**.

### Configuration

1. **Workflow** : ajoutez les déclencheurs `issues` et `pull_request_review` et accordez `contents: write` + `issues: write`. `.github/workflows/hoverstare.yml` de ce dépôt est un exemple complet.
2. **Jetons** (deux rôles, deux jetons) :
   - *Identité* (commentaires, reviews, ouverture de PR) : le `GITHUB_TOKEN` par défaut, ou un jeton de GitHub App pour l'identité `hoverstare[bot]`.
   - *Écriture* (git push, merge, suppression de branche) : un PAT via l'entrée `gh_pat`, ou une App avec `contents: write`. Les push du `GITHUB_TOKEN` par défaut ne déclenchent **pas** la CI, et les checks requis ne tourneraient jamais sur les commits du bot.
3. **Permissions** (optionnel) : déclarez qui peut quoi dans `.github/hoverstare.toml` `[permissions]` (spec 12). Défauts : auto-review pour anyone, `review`/`develop` pour collaborator, `merge` pour write.

### Remarques

- Seuls les collaborateurs du dépôt peuvent donner des commandes ; les PR de fork sont hors scope.
- Les PR ouvertes par le bot peuvent afficher **1 workflow awaiting approval**
  sur les checks `pull_request`. C'est la barrière maintainer de GitHub (FAQ
  ci-dessous), pas une clé LLM manquante.
- Les grosses tâches sont découpées en rounds budgétés ; le bot se relance tout seul (max 10 rounds par PR). Il ne peut pas lancer de builds ni de tests — les échecs CI sont relayés comme instructions au round suivant.

## FAQ

**Erreurs de permission à la publication ?**
Vérifiez les `permissions` du workflow (`pull-requests: write` requis) et que
*Settings → Actions → General → Workflow permissions* est sur "Read and write".

**"model not found" ?**
Vous avez configuré un endpoint compatible OpenAI mais pas de nom de modèle.
Définissez `HOVERSTARE_MODEL` (ou `model` dans `hoverstare.toml`).

**400 / invalid temperature ?**
Votre endpoint n'accepte que la température par défaut. Mettez
`set_temperature = false` dans `hoverstare.toml`.

**Les anomalies corrigées ne sont pas résolues ?**
Limitation de la plateforme GitHub : le `GITHUB_TOKEN` par défaut ne peut pas
appeler `resolveReviewThread`. HoverStare répond alors « ✅ correction confirmée »
dans le fil. Pour une résolution complète, stockez un PAT classique
(`repo` scope) comme secret `GH_PAT` et passez-le dans l'env du workflow.

**GitHub Enterprise ?**
Définissez `GITHUB_API_URL=https://<votre-hote-ghe>/api/v3`.

**`@hoverstare merge` échoue avec 403 ?**
L'endpoint de merge exige `contents: write`. Passez un PAT via `gh_pat`, ou accordez à la GitHub App Contents: Read and write (et acceptez la mise à niveau sur l'installation).

**Bandeau jaune « 1 workflow awaiting approval » / checks en action_required ?**
C'est la barrière d'approbation GitHub pour les workflows `pull_request`, pas un
bug HoverStare ni un secret manquant.

GitHub vérifie **à la fois** l'auteur de la PR **et** l'acteur de l'événement
qui a démarré le run. Ce sont deux comptes : fusionner des PR de
`hoverstare[bot]` ne fait confiance qu'à l'identité App (ouvrir la PR ne
montre plus le bandeau). Un commit ultérieur poussé **depuis un workflow**
est souvent attribué à `github-actions[bot]`, un autre compte sans PR
fusionnée, donc la politique par défaut redemande une approbation **à chaque
push de ce type**. Une PR à un seul commit n'atteint pas le second cas ;
avec `GH_PAT`, le push est le collaborateur et les rounds `continue` ne
déclenchent pas la barrière.

Détecter : `gh run list --jq '.[]|select(.conclusion=="action_required")'`.
Débloquer : **Approve workflows to run**, ou
`gh api -X POST repos/OWNER/REPO/actions/runs/RUN_ID/approve`.
Prévenir : PAT collaborateur en `gh_pat`/`GH_PAT` pour le push ; commentaires
et PR restent l'App. Pousser uniquement avec l'App (le repli actuel) ne
unifie pas l'acteur CI : GitHub enregistre encore `github-actions[bot]`.
Assouplir la politique fork-PR n'a pas sauté cette barrière sur le même
dépôt. Pas de `pull_request_target` pour auto-approuver.

**Un round go/dev répond « aucun changement, pas de PR créée » ?**
La tâche était probablement trop vague pour un round budgété. Répondez avec une instruction plus précise et plus petite (fichiers à toucher, critères d'acceptation) et relancez `go` — le commentaire du bot indique ce qu'il a réellement fait.

## Développement local

```bash
# Dry-run d'une revue complète d'une PR publique (sans publication)
export OPENAI_API_KEY=... OPENAI_BASE_URL=... HOVERSTARE_MODEL=...
cargo run -- review --repo owner/repo --pr 123 --dry-run

# Revue d'un fichier diff local (affiche la trace des appels d'outils)
cargo run --example local_review -- path/to.diff [base_ref]

cargo test                                   # tests unitaires + contrats httpmock
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Les specs et le plan de jalons sont dans [`specs/`](specs/README.md) — la
source de vérité unique pour les décisions de conception.

## Historique des étoiles et contributeurs

Mis à jour quotidiennement par [RepoScope](https://github.com/liuchong/reposcope) — commits sur la branche orpheline `reposcope`, jamais sur `master`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history-dark.svg">
  <img alt="Star History" src="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history.svg">
</picture>

![Contributors](https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/contributors.svg)

## Licence

[1PL — One Public License](https://license.pub/1pl/)
