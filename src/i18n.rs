//! Localization (spec 01 §language).
//!
//! Supported languages mirror the README set: English, Simplified Chinese,
//! Russian, French, German, Spanish. Anything not recognized falls back to
//! English. Machine-readable payloads (hoverstare-meta, fingerprints, JSON
//! schema, command names) are never localized.

/// Supported UI languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    ZhCn,
    Ru,
    Fr,
    De,
    Es,
}

impl Lang {
    /// Parse a free-form language tag ("en", "zh-CN", "zh_CN", "zh", "ru",
    /// "fr", "de", "es", "english", "中文"...). Anything else → English.
    pub fn parse_loose(s: &str) -> Lang {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "zh" | "zh-cn" | "zh-hans" | "cn" | "chinese" | "中文" => Lang::ZhCn,
            "ru" | "ru-ru" | "russian" => Lang::Ru,
            "fr" | "fr-fr" | "french" => Lang::Fr,
            "de" | "de-de" | "german" => Lang::De,
            "es" | "es-es" | "spanish" => Lang::Es,
            _ => Lang::En,
        }
    }

    /// Resolve from env override, then config file, then default.
    pub fn resolve(env: Option<&str>, toml: Option<&str>) -> Lang {
        env.filter(|v| !v.trim().is_empty())
            .map(Lang::parse_loose)
            .or_else(|| toml.filter(|v| !v.trim().is_empty()).map(Lang::parse_loose))
            .unwrap_or_default()
    }

    /// Human-readable language name (for the LLM output-language directive).
    pub fn display_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::ZhCn => "Simplified Chinese",
            Lang::Ru => "Russian",
            Lang::Fr => "French",
            Lang::De => "German",
            Lang::Es => "Spanish",
        }
    }
}

/// Localized text bundle for one language.
pub struct T(pub Lang);

impl T {
    pub fn new(lang: Lang) -> T {
        T(lang)
    }

    // ------------------------------------------------------------------
    // Review body (report.rs)
    // ------------------------------------------------------------------

    pub fn scope_heading(&self) -> &'static str {
        match self.0 {
            Lang::En => "Review scope",
            Lang::ZhCn => "审查范围",
            Lang::Ru => "Область обзора",
            Lang::Fr => "Périmètre de la revue",
            Lang::De => "Review-Umfang",
            Lang::Es => "Alcance de la revisión",
        }
    }

    pub fn scope_full(&self) -> &'static str {
        match self.0 {
            Lang::En => "Full review",
            Lang::ZhCn => "全量审查",
            Lang::Ru => "Полный обзор",
            Lang::Fr => "Revue complète",
            Lang::De => "Vollständiges Review",
            Lang::Es => "Revisión completa",
        }
    }

    pub fn scope_incremental(&self, prior: &str) -> String {
        match self.0 {
            Lang::En => format!("Incremental review (since {prior})"),
            Lang::ZhCn => format!("增量审查（自 {prior} 以来）"),
            Lang::Ru => format!("Инкрементальный обзор (с {prior})"),
            Lang::Fr => format!("Revue incrémentale (depuis {prior})"),
            Lang::De => format!("Inkrementelles Review (seit {prior})"),
            Lang::Es => format!("Revisión incremental (desde {prior})"),
        }
    }

    pub fn files_count(&self, n: usize) -> String {
        match self.0 {
            Lang::En => format!("{n} file(s)"),
            Lang::ZhCn => format!("{n} 个文件"),
            Lang::Ru => format!("файлов: {n}"),
            Lang::Fr => format!("{n} fichier(s)"),
            Lang::De => format!("{n} Datei(en)"),
            Lang::Es => format!("{n} archivo(s)"),
        }
    }

    pub fn excluded_note(&self, n: usize) -> String {
        match self.0 {
            Lang::En => format!(" (plus {n} lockfile/generated file(s) skipped by rules)"),
            Lang::ZhCn => format!("（另有 {n} 个锁定/生成文件按规则跳过）"),
            Lang::Ru => format!(" (ещё {n} lock/сгенерированных файлов пропущено по правилам)"),
            Lang::Fr => format!(" (plus {n} fichier(s) lock/généré(s) ignoré(s) par les règles)"),
            Lang::De => {
                format!(" (zusätzlich {n} Lock-/generierte Datei(en) per Regel übersprungen)")
            }
            Lang::Es => format!(" (además {n} archivo(s) lock/generado(s) omitido(s) por reglas)"),
        }
    }

    /// Coverage line (spec 14 §4): what this run promised to review and what it
    /// actually covered. `failed`/`truncated` are appended only when non-zero, so
    /// the common case stays one short sentence.
    pub fn coverage_line(
        &self,
        covered: usize,
        total: usize,
        failed: usize,
        truncated: usize,
    ) -> String {
        let mut s = match self.0 {
            Lang::En => format!("Coverage: {covered}/{total} review unit(s)"),
            Lang::ZhCn => format!("覆盖：{covered}/{total} 个审查单元"),
            Lang::Ru => format!("Покрытие: {covered}/{total} единиц(ы) проверки"),
            Lang::Fr => format!("Couverture : {covered}/{total} unité(s) de revue"),
            Lang::De => format!("Abdeckung: {covered}/{total} Review-Einheit(en)"),
            Lang::Es => format!("Cobertura: {covered}/{total} unidad(es) de revisión"),
        };
        if failed > 0 || truncated > 0 {
            s.push_str(&match self.0 {
                Lang::En => format!("; {failed} failed, {truncated} truncated"),
                Lang::ZhCn => format!("；{failed} 个失败，{truncated} 个被截断"),
                Lang::Ru => format!("; ошибок: {failed}, усечено: {truncated}"),
                Lang::Fr => format!(" ; {failed} en échec, {truncated} tronquée(s)"),
                Lang::De => format!("; {failed} fehlgeschlagen, {truncated} abgeschnitten"),
                Lang::Es => format!("; {failed} con error, {truncated} truncada(s)"),
            });
        }
        s
    }

    /// `--preview` summary line (spec 14 §3). Human-readable only: the JSON form
    /// is machine-readable and never localized.
    pub fn preview_summary(&self, scope: &str, units: usize, tokens: u64) -> String {
        match self.0 {
            Lang::En => format!(
                "preview: {scope} — {units} review unit(s), ~{tokens} tokens, no model calls"
            ),
            Lang::ZhCn => {
                format!("预览：{scope} — {units} 个审查单元，约 {tokens} tokens，未调用模型")
            }
            Lang::Ru => format!(
                "предпросмотр: {scope} — единиц: {units}, ~{tokens} токенов, без вызова модели"
            ),
            Lang::Fr => format!(
                "aperçu : {scope} — {units} unité(s), ~{tokens} jetons, aucun appel de modèle"
            ),
            Lang::De => format!(
                "Vorschau: {scope} — {units} Einheit(en), ~{tokens} Token, keine Modellaufrufe"
            ),
            Lang::Es => format!(
                "vista previa: {scope} — {units} unidad(es), ~{tokens} tokens, sin llamadas al modelo"
            ),
        }
    }

    pub fn preview_will_review(&self) -> &'static str {
        match self.0 {
            Lang::En => "will review:",
            Lang::ZhCn => "将审查：",
            Lang::Ru => "будет проверено:",
            Lang::Fr => "sera examiné :",
            Lang::De => "wird geprüft:",
            Lang::Es => "se revisará:",
        }
    }

    pub fn preview_nothing(&self) -> &'static str {
        match self.0 {
            Lang::En => "(nothing)",
            Lang::ZhCn => "（无）",
            Lang::Ru => "(ничего)",
            Lang::Fr => "(rien)",
            Lang::De => "(nichts)",
            Lang::Es => "(nada)",
        }
    }

    pub fn preview_excluded(&self) -> &'static str {
        match self.0 {
            Lang::En => "excluded:",
            Lang::ZhCn => "已排除：",
            Lang::Ru => "исключено:",
            Lang::Fr => "exclu :",
            Lang::De => "ausgeschlossen:",
            Lang::Es => "excluido:",
        }
    }

    pub fn preview_kept_for_anchoring(&self) -> &'static str {
        match self.0 {
            Lang::En => ", kept for anchoring",
            Lang::ZhCn => "，为锚定保留",
            Lang::Ru => ", оставлено для привязки",
            Lang::Fr => ", conservé pour l'ancrage",
            Lang::De => ", für Verankerung behalten",
            Lang::Es => ", conservado para el anclaje",
        }
    }

    pub fn clean_verdict(&self) -> &'static str {
        match self.0 {
            Lang::En => "✅ No defects found.",
            Lang::ZhCn => "✅ 未发现缺陷。",
            Lang::Ru => "✅ Дефектов не найдено.",
            Lang::Fr => "✅ Aucun défaut trouvé.",
            Lang::De => "✅ Keine Defekte gefunden.",
            Lang::Es => "✅ No se encontraron defectos.",
        }
    }

    pub fn stats_line(&self, inline: usize, cross: usize, threshold: &str) -> String {
        match self.0 {
            Lang::En => format!(
                "{inline} inline comment(s), {cross} cross-file/unanchored finding(s) (threshold: {threshold})."
            ),
            Lang::ZhCn => format!(
                "共 {inline} 条行内评论、{cross} 条跨文件/未锚定发现（阈值：{threshold}）。"
            ),
            Lang::Ru => format!(
                "Встроенных комментариев: {inline}, межфайловых/непривязанных находок: {cross} (порог: {threshold})."
            ),
            Lang::Fr => format!(
                "{inline} commentaire(s) en ligne, {cross} constat(s) transversal(aux)/non ancré(s) (seuil : {threshold})."
            ),
            Lang::De => format!(
                "{inline} Inline-Kommentar(e), {cross} dateiübergreifende/nicht verankerte Befund(e) (Schwelle: {threshold})."
            ),
            Lang::Es => format!(
                "{inline} comentario(s) en línea, {cross} hallazgo(s) entre archivos/sin anclar (umbral: {threshold})."
            ),
        }
    }

    pub fn related_locations(&self) -> &'static str {
        match self.0 {
            Lang::En => "**Related locations**",
            Lang::ZhCn => "**相关位置**",
            Lang::Ru => "**Связанные места**",
            Lang::Fr => "**Emplacements associés**",
            Lang::De => "**Verwandte Stellen**",
            Lang::Es => "**Ubicaciones relacionadas**",
        }
    }

    pub fn snap_note(&self, orig_line: u64) -> String {
        match self.0 {
            Lang::En => format!(
                "> ⚠️ *Reported line {orig_line} is not in the diff; anchored to the nearest changed line.*"
            ),
            Lang::ZhCn => format!(
                "> ⚠️ *模型报告的行为第 {orig_line} 行（不在 diff 中），已吸附到最近的变更行。*"
            ),
            Lang::Ru => format!(
                "> ⚠️ *Строка {orig_line} из отчёта не входит в diff; привязано к ближайшей изменённой строке.*"
            ),
            Lang::Fr => format!(
                "> ⚠️ *La ligne {orig_line} signalée n'est pas dans le diff ; ancrée à la ligne modifiée la plus proche.*"
            ),
            Lang::De => format!(
                "> ⚠️ *Gemeldete Zeile {orig_line} ist nicht im Diff; auf die nächste geänderte Zeile verankert.*"
            ),
            Lang::Es => format!(
                "> ⚠️ *La línea {orig_line} reportada no está en el diff; anclada a la línea modificada más cercana.*"
            ),
        }
    }

    pub fn fallback_header(&self) -> &'static str {
        match self.0 {
            Lang::En => "### All findings (unanchored)",
            Lang::ZhCn => "### 全部发现（未锚定）",
            Lang::Ru => "### Все находки (без привязки)",
            Lang::Fr => "### Toutes les constats (non ancrés)",
            Lang::De => "### Alle Befunde (nicht verankert)",
            Lang::Es => "### Todos los hallazgos (sin anclar)",
        }
    }

    // ------------------------------------------------------------------
    // mention.rs
    // ------------------------------------------------------------------

    pub fn help_text(&self) -> String {
        let lines: &[&str] = match self.0 {
            Lang::En => &[
                "👁 **HoverStare commands**",
                "",
                "Review (in a PR comment):",
                "- `@hoverstare review` — force a full re-review of this PR",
                "- `@hoverstare explain` — explain a finding (reply in its thread)",
                "- Reply directly in a HoverStare finding thread to discuss it — no @mention needed",
                "- `@hoverstare help` or `@hoverstare /help` — show this help",
                "",
                "Develop (issue/PR discussion):",
                "- `@hoverstare <question>` — discuss an issue and propose a plan",
                "- `@hoverstare go` — implement the agreed plan",
                "- `@hoverstare continue` / `@hoverstare merge` — run the next PR dev round / merge",
                "- `@hoverstare` (bare) — show this help",
                "- A development thread auto-continues up to 10 rounds and stays on the same repo branch.",
                "",
                "Configuration: `.github/hoverstare.toml`",
                "Docs: `specs/`",
            ],
            Lang::ZhCn => &[
                "👁 **HoverStare 命令列表**",
                "",
                "审查（PR 评论中）：",
                "- `@hoverstare review` — 强制全量重审本 PR",
                "- `@hoverstare explain` — 解释某条审查发现（在对应线程中回复）",
                "- 直接在 HoverStare 发现的线程里回复即可讨论该发现 —— 无需 @mention",
                "- `@hoverstare help` 或 `@hoverstare /help` — 显示本帮助",
                "",
                "开发（Issue/PR 讨论）：",
                "- `@hoverstare <问题>` — 讨论 Issue 并生成方案",
                "- `@hoverstare go` — 执行已确认的方案",
                "- `@hoverstare continue` / `@hoverstare merge` — 下一轮 PR 开发迭代 / 合并",
                "- 单独的 `@hoverstare` — 显示本帮助",
                "- 开发线程会自动继续，最多 10 轮，且必须在同一仓库分支内。",
                "",
                "配置：`.github/hoverstare.toml`",
                "文档：`specs/`",
            ],
            Lang::Ru => &[
                "👁 **Команды HoverStare**",
                "",
                "Ревью (в комментарии к PR):",
                "- `@hoverstare review` — принудительный полный повторный обзор PR",
                "- `@hoverstare explain` — объяснить находку (ответ в её треде)",
                "- Ответьте прямо в треде находки HoverStare, чтобы обсудить её — @mention не нужен",
                "- `@hoverstare help` или `@hoverstare /help` — показать эту справку",
                "",
                "Разработка (обсуждение issue/PR):",
                "- `@hoverstare <вопрос>` — обсудить issue и предложить план",
                "- `@hoverstare go` — реализовать согласованный план",
                "- `@hoverstare continue` / `@hoverstare merge` — следующий раунд разработки PR / слияние",
                "- `@hoverstare` без команды — показать эту справку",
                "- Поток разработки продолжается автоматически до 10 раундов и должен оставаться в одном репозитории/ветке.",
                "",
                "Конфигурация: `.github/hoverstare.toml`",
                "Документация: `specs/`",
            ],
            Lang::Fr => &[
                "👁 **Commandes HoverStare**",
                "",
                "Revue (dans un commentaire de PR):",
                "- `@hoverstare review` — forcer une nouvelle revue complète de cette PR",
                "- `@hoverstare explain` — expliquer une constat (répondre dans son fil)",
                "- Répondez directement dans le fil d'une constat HoverStare pour la discuter — pas besoin de @mention",
                "- `@hoverstare help` ou `@hoverstare /help` — afficher cette aide",
                "",
                "Développement (discussion issue/PR):",
                "- `@hoverstare <question>` — discuter d'une issue et proposer un plan",
                "- `@hoverstare go` — implémenter le plan convenu",
                "- `@hoverstare continue` / `@hoverstare merge` — tour de développement PR suivant / fusionner",
                "- `@hoverstare` seul — afficher cette aide",
                "- Un fil de développement se poursuit automatiquement jusqu'à 10 tours, sur la même branche du même dépôt.",
                "",
                "Configuration : `.github/hoverstare.toml`",
                "Docs : `specs/`",
            ],
            Lang::De => &[
                "👁 **HoverStare-Befehle**",
                "",
                "Review (im PR-Kommentar):",
                "- `@hoverstare review` — vollständiges Re-Review dieses PRs erzwingen",
                "- `@hoverstare explain` — einen Befund erklären (in seinem Thread antworten)",
                "- Antworten Sie direkt im Thread eines HoverStare-Befunds, um ihn zu diskutieren — kein @mention nötig",
                "- `@hoverstare help` oder `@hoverstare /help` — diese Hilfe anzeigen",
                "",
                "Entwicklung (Issue-/PR-Diskussion):",
                "- `@hoverstare <Frage>` — ein Issue besprechen und einen Plan vorschlagen",
                "- `@hoverstare go` — den vereinbarten Plan umsetzen",
                "- `@hoverstare continue` / `@hoverstare merge` — nächste PR-Entwicklungsrunde / mergen",
                "- `@hoverstare` allein — diese Hilfe anzeigen",
                "- Ein Entwicklungs-Thread läuft automatisch bis zu 10 Runden weiter, im selben Repo/Zweig.",
                "",
                "Konfiguration: `.github/hoverstare.toml`",
                "Dokumentation: `specs/`",
            ],
            Lang::Es => &[
                "👁 **Comandos de HoverStare**",
                "",
                "Revisión (en un comentario de PR):",
                "- `@hoverstare review` — forzar una revisión completa de este PR",
                "- `@hoverstare explain` — explicar un hallazgo (responder en su hilo)",
                "- Responde directamente en el hilo de un hallazgo de HoverStare para discutirlo — no hace falta @mention",
                "- `@hoverstare help` o `@hoverstare /help` — mostrar esta ayuda",
                "",
                "Desarrollo (discusión de issue/PR):",
                "- `@hoverstare <pregunta>` — discutir un issue y proponer un plan",
                "- `@hoverstare go` — implementar el plan acordado",
                "- `@hoverstare continue` / `@hoverstare merge` — siguiente ronda de desarrollo del PR / merge",
                "- `@hoverstare` solo — mostrar esta ayuda",
                "- Un hilo de desarrollo continúa automáticamente hasta 10 rondas, en la misma rama del mismo repositorio.",
                "",
                "Configuración: `.github/hoverstare.toml`",
                "Documentación: `specs/`",
            ],
        };
        lines.join("\n")
    }

    pub fn explain_header(&self) -> &'static str {
        match self.0 {
            Lang::En => "👁 **HoverStare Explanation**",
            Lang::ZhCn => "👁 **HoverStare 解释**",
            Lang::Ru => "👁 **HoverStare: объяснение**",
            Lang::Fr => "👁 **Explication HoverStare**",
            Lang::De => "👁 **HoverStare-Erklärung**",
            Lang::Es => "👁 **Explicación de HoverStare**",
        }
    }

    pub fn resolved_reply(&self) -> &'static str {
        match self.0 {
            Lang::En => "✅ HoverStare confirmed fixed",
            Lang::ZhCn => "✅ HoverStare 已确认修复",
            Lang::Ru => "✅ HoverStare: исправление подтверждено",
            Lang::Fr => "✅ HoverStare : correction confirmée",
            Lang::De => "✅ HoverStare: Fix bestätigt",
            Lang::Es => "✅ HoverStare: corrección confirmada",
        }
    }

    pub fn command_failed(&self, err: &str) -> String {
        match self.0 {
            Lang::En => format!("👁 Command failed: {err}"),
            Lang::ZhCn => format!("👁 命令执行失败：{err}"),
            Lang::Ru => format!("👁 Ошибка выполнения команды: {err}"),
            Lang::Fr => format!("👁 Échec de la commande : {err}"),
            Lang::De => format!("👁 Befehl fehlgeschlagen: {err}"),
            Lang::Es => format!("👁 Error al ejecutar el comando: {err}"),
        }
    }

    pub fn permission_denied(&self) -> &'static str {
        match self.0 {
            Lang::En => "Permission denied: you do not meet the requirements for this command.",
            Lang::ZhCn => "权限不足：你未满足当前命令的权限要求。",
            Lang::Ru => "Недостаточно прав: вы не соответствуете требованиям этой команды.",
            Lang::Fr => {
                "Permission refusée : vous ne remplissez pas les conditions pour cette commande."
            }
            Lang::De => {
                "Berechtigung verweigert: Sie erfüllen nicht die Anforderungen für diesen Befehl."
            }
            Lang::Es => "Permiso denegado: no cumples los requisitos para este comando.",
        }
    }

    // ------------------------------------------------------------------
    // Status checks (orchestrator.rs)
    // ------------------------------------------------------------------

    pub fn status_review_done(&self) -> &'static str {
        match self.0 {
            Lang::En => "Review completed",
            Lang::ZhCn => "审查完成",
            Lang::Ru => "Обзор завершён",
            Lang::Fr => "Revue terminée",
            Lang::De => "Review abgeschlossen",
            Lang::Es => "Revisión completada",
        }
    }

    pub fn status_no_high(&self) -> &'static str {
        match self.0 {
            Lang::En => "No high-severity findings",
            Lang::ZhCn => "无高危发现",
            Lang::Ru => "Нет находок высокой важности",
            Lang::Fr => "Aucune constat de gravité élevée",
            Lang::De => "Keine Befunde hoher Schwere",
            Lang::Es => "Sin hallazgos de gravedad alta",
        }
    }

    pub fn status_high_found(&self, new: usize, open: usize) -> String {
        match self.0 {
            Lang::En => format!("Unresolved high-severity findings (new: {new}, open: {open})"),
            Lang::ZhCn => format!("存在未解决的高危发现（新 {new} 条，历史未关闭 {open} 条）"),
            Lang::Ru => {
                format!("Нерешённые находки высокой важности (новых: {new}, открытых: {open})")
            }
            Lang::Fr => format!(
                "Constats de gravité élevée non résolus (nouveaux : {new}, ouverts : {open})"
            ),
            Lang::De => format!("Ungelöste Befunde hoher Schwere (neu: {new}, offen: {open})"),
            Lang::Es => {
                format!("Hallazgos de gravedad alta sin resolver (nuevos: {new}, abiertos: {open})")
            }
        }
    }

    pub fn status_skipped(&self, reason: &str) -> String {
        match self.0 {
            Lang::En => format!("Skipped: {reason}"),
            Lang::ZhCn => format!("跳过：{reason}"),
            Lang::Ru => format!("Пропущено: {reason}"),
            Lang::Fr => format!("Ignoré : {reason}"),
            Lang::De => format!("Übersprungen: {reason}"),
            Lang::Es => format!("Omitido: {reason}"),
        }
    }

    pub fn status_nothing_to_review(&self, reason: &str) -> String {
        match self.0 {
            Lang::En => format!("Nothing to review, no findings ({reason})"),
            Lang::ZhCn => format!("无可审内容，无发现（{reason}）"),
            Lang::Ru => format!("Нечего проверять, находок нет ({reason})"),
            Lang::Fr => format!("Rien à revoir, aucune constat ({reason})"),
            Lang::De => format!("Nichts zu prüfen, keine Befunde ({reason})"),
            Lang::Es => format!("Nada que revisar, sin hallazgos ({reason})"),
        }
    }

    // ------------------------------------------------------------------
    // Orchestrator logs
    // ------------------------------------------------------------------

    pub fn log_findings(&self, findings: usize, resolved: usize) -> String {
        match self.0 {
            Lang::En => format!("Model reported {findings} finding(s), {resolved} marked fixed"),
            Lang::ZhCn => format!("模型报告 {findings} 条 finding，判定已修复 {resolved} 条"),
            Lang::Ru => format!("Модель сообщила находок: {findings}, исправленных: {resolved}"),
            Lang::Fr => format!(
                "Le modèle a rapporté {findings} constat(s), {resolved} marqué(s) corrigé(s)"
            ),
            Lang::De => {
                format!("Modell meldete {findings} Befund(e), {resolved} als behoben markiert")
            }
            Lang::Es => format!(
                "El modelo reportó {findings} hallazgo(s), {resolved} marcado(s) como corregido(s)"
            ),
        }
    }

    pub fn log_tool_calls(&self, n: usize, names: &str) -> String {
        match self.0 {
            Lang::En => format!("{n} tool call(s): {names}"),
            Lang::ZhCn => format!("工具调用 {n} 次: {names}"),
            Lang::Ru => format!("Вызовов инструментов: {n}: {names}"),
            Lang::Fr => format!("{n} appel(s) d'outils : {names}"),
            Lang::De => format!("{n} Werkzeugaufruf(e): {names}"),
            Lang::Es => format!("{n} llamada(s) a herramientas: {names}"),
        }
    }

    pub fn log_resolved_threads(&self, resolved: usize, replied: usize) -> String {
        match self.0 {
            Lang::En => {
                format!("Fixed threads handled: {resolved} resolved, {replied} marked via reply")
            }
            Lang::ZhCn => format!("已修复线程处理：resolve {resolved} 个，降级标记 {replied} 个"),
            Lang::Ru => format!(
                "Обработано исправленных тредов: resolved {resolved}, отмечено ответом {replied}"
            ),
            Lang::Fr => {
                format!("Fils corrigés traités : {resolved} résolus, {replied} marqués par réponse")
            }
            Lang::De => {
                format!("Behobene Threads: {resolved} aufgelöst, {replied} per Antwort markiert")
            }
            Lang::Es => {
                format!("Hilos corregidos: {resolved} resueltos, {replied} marcados por respuesta")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_languages() {
        assert_eq!(Lang::parse_loose("en"), Lang::En);
        assert_eq!(Lang::parse_loose("zh-CN"), Lang::ZhCn);
        assert_eq!(Lang::parse_loose("zh_cn"), Lang::ZhCn);
        assert_eq!(Lang::parse_loose("中文"), Lang::ZhCn);
        assert_eq!(Lang::parse_loose("RU"), Lang::Ru);
        assert_eq!(Lang::parse_loose("fr"), Lang::Fr);
        assert_eq!(Lang::parse_loose("de"), Lang::De);
        assert_eq!(Lang::parse_loose("es"), Lang::Es);
        assert_eq!(Lang::parse_loose("klingon"), Lang::En);
        assert_eq!(Lang::parse_loose(""), Lang::En);
    }

    #[test]
    fn resolve_precedence() {
        assert_eq!(Lang::resolve(Some("fr"), Some("es")), Lang::Fr);
        assert_eq!(Lang::resolve(None, Some("de")), Lang::De);
        assert_eq!(Lang::resolve(None, None), Lang::En);
        assert_eq!(Lang::resolve(Some("  "), Some("ru")), Lang::Ru);
    }

    #[test]
    fn coverage_and_preview_lines_exist_in_every_language() {
        for lang in [Lang::En, Lang::ZhCn, Lang::Ru, Lang::Fr, Lang::De, Lang::Es] {
            let t = T::new(lang);
            let coverage = t.coverage_line(3, 3, 0, 0);
            assert!(
                coverage.contains('3'),
                "coverage line for {lang:?}: {coverage}"
            );
            assert!(
                !coverage.contains("failed") || lang == Lang::En,
                "the failure suffix must stay out of a fully covered run ({lang:?})"
            );
            let partial = t.coverage_line(1, 3, 1, 1);
            assert_ne!(partial, coverage, "partial coverage must differ ({lang:?})");
            let preview = t.preview_summary(t.scope_full(), 2, 120);
            assert!(
                preview.contains('2') && preview.contains("120"),
                "preview summary for {lang:?}: {preview}"
            );
            for label in [
                t.preview_will_review(),
                t.preview_nothing(),
                t.preview_excluded(),
                t.preview_kept_for_anchoring(),
            ] {
                assert!(!label.trim().is_empty(), "empty preview label for {lang:?}");
            }
        }
        assert_eq!(
            T::new(Lang::En).coverage_line(3, 3, 0, 0),
            "Coverage: 3/3 review unit(s)"
        );
        assert!(
            T::new(Lang::ZhCn)
                .coverage_line(1, 2, 1, 0)
                .starts_with("覆盖：1/2")
        );
    }

    #[test]
    fn help_text_covers_commands() {
        for lang in [Lang::En, Lang::ZhCn, Lang::Ru, Lang::Fr, Lang::De, Lang::Es] {
            let text = T::new(lang).help_text().to_lowercase();
            assert!(
                text.contains("review"),
                "help_text for {lang:?} should mention review"
            );
            assert!(
                text.contains("merge"),
                "help_text for {lang:?} should mention merge"
            );
        }
    }

    #[test]
    fn every_key_has_all_languages() {
        for lang in [Lang::En, Lang::ZhCn, Lang::Ru, Lang::Fr, Lang::De, Lang::Es] {
            let t = T::new(lang);
            assert!(!t.scope_full().is_empty());
            assert!(!t.clean_verdict().is_empty());
            assert!(!t.related_locations().is_empty());
            assert!(!t.fallback_header().is_empty());
            assert!(!t.help_text().is_empty());
            assert!(!t.explain_header().is_empty());
            assert!(!t.resolved_reply().is_empty());
            assert!(!t.status_review_done().is_empty());
            assert!(!t.status_no_high().is_empty());
            assert!(!t.snap_note(3).is_empty());
            assert!(!t.stats_line(1, 2, "medium").is_empty());
            assert!(!t.log_findings(1, 1).is_empty());
        }
    }
}
