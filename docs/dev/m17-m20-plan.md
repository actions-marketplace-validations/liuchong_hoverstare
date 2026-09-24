# M17–M20 施工计划

> 目标状态由 [`specs/14`–`17`](../../specs/README.md) 定义，落地方式由
> [`m17-m20-design.md`](m17-m20-design.md) 定义，本文定义**怎么排、谁依赖谁、怎么验收**。
> 冲突时以 spec 为准；实现中发现 spec 不成立，先改 spec 再改代码。

## 0. 目标（goal 文案）

```
按 specs/14–17 与 docs/dev/m17-m20-plan.md，在 hoverstare 落地 M17–M20：
(1) M17 审查单元与覆盖契约：units::select 纯函数（预览与运行同源）、--preview 零模型调用、
    覆盖账本（分母冻结 / 单元状态机 / 终态 ok|partial|empty）+ 摘要覆盖声明；
(2) M18 语言规则包：内置规则包数据形态与校验、路径匹配（含歧义嗅探与显式回退）、
    [LANGUAGE RULES] 注入契约、rules list / rules check 只读自检；
(3) M19 输出契约：--format human|json|sarif + --output、JSON 契约、SARIF 2.1.0 映射、
    日志改 stderr 保证 stdout 纯净、usage 聚合；
(4) M20 工程门禁：引用 pin / 文档结构 / 依赖审计 / 覆盖率不劣化 / 密钥扫描 / spec 索引检查，
    verify-all.sh 一条命令，CI 阻塞，含 15 处引用 pin、六份 README 结构对齐等一次性收尾。

约束：spec 是单一事实来源（先改 spec 再改代码）；每个任务完成即跑
cargo fmt --all -- --check、cargo clippy --workspace --all-targets -- -D warnings、
cargo test --workspace 与 scripts/verify-all.sh，并单独提交（Conventional Commits、签名提交）；
文档（specs / docs / AGENTS.md 运维经验）与实现同步；不做任何发布
（不打 tag、不 cargo publish、不建 Release、不推 Marketplace）。

完成定义：specs/14–17 的 §验收全部满足，specs/README.md 的 M17–M20 勾选完成，CI 全绿。
```

## 1. 批次、依赖与估时

```
P0 门禁脚本骨架 ──► S1 M17 审查单元与覆盖 ──┬──► S2a M19 输出契约 ──┐
（无依赖，立即收益）                        └──► S2b M18 规则包 ────┴──► S3 M20 收尾
```

| 阶段 | 内容 | 依赖 | 估时 |
|---|---|---|---|
| P0 | G1/G2/G6/G7 脚本 + `verify-all.sh`（先给后续每个提交做自检） | 无 | 1 天 |
| S1 | M17：审查单元、`select`、`--preview`、覆盖账本 | P0（便于每步自检） | 3–4 天 |
| S2a | M19：`--format`/`--output`、JSON、SARIF、日志改 stderr | S1（`units`/`terminal`/`usage` 来源） | 2–3 天 |
| S2b | M18：规则包数据、匹配、注入、`rules check` | 可与 S2a 并行（仅共享 `config.rs`/`i18n.rs` 的小改动） | 2–3 天 |
| S3 | M20 收尾：pin 全部引用、README 结构对齐、覆盖率基线、密钥扫描、CI 接线 | 全部 | 1–2 天 |

合计约 9–13 人日（单人全职口径）。**P0 先做**的理由：门禁是自己后续每个提交的安全网，
且不依赖任何代码改动。

## 2. P0 — 门禁脚本骨架

| 任务 | 内容 | 交付物 | 验收 |
|---|---|---|---|
| ✅ T20.1 | `scripts/verify-action-pins.sh`（行首锚定正则，`./` 豁免，非法形态一律失败） | 脚本 + 正例/反例 fixture | 在干净树上失败并列出全部 15 处待 pin 引用（此时是预期红） |
| ✅ T20.2 | `scripts/check-doc-structure.sh`（`##` 序列比对，打印差异行号） | 脚本 + fixture | 六份 README 比对输出英文 12 / 其余 11 的差异点 |
| ✅ T20.3 | `scripts/verify-spec-index.sh`（spec 索引 + `src/` 模块对应，含豁免清单） | 脚本 | 当前树全绿（spec 14–17 已在索引内） |
| ✅ T20.4 | `scripts/verify-all.sh`（默认 G1/G2/G6/G7；`--full` 加 G3/G4） | 脚本 | 退出码聚合正确；`--full` 在缺工具时给出安装提示而非静默跳过 |
| T20.5 | （不在 P0 做）CI 接线留到 S3 的 T20.10 | — | 见下方说明：避免 CI 长时间红 |

> 说明：P0 的脚本在树还没修好时必然报红。因此 **P0 只把脚本做出来并在本地使用，
> CI 接线与一次性修复一起放到 S3**，避免 CI 长时间红着失去信号价值。

**P0 实测结果（交付时）**：

- `scripts/tests/test-gates.sh`：18 项断言全过（正例/反例 fixture 覆盖 G1 与 G2 的各类形态）；
- `scripts/verify-all.sh` 在真实树上：G1 FAIL（列出全部 15 处待 pin 引用）、G2 FAIL
  （五份翻译各少一个小节）、G6 PASS、G7 PASS；
- `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`
  干净，`cargo test --workspace` **219 passed / 0 failed**；
- 过程中修掉门禁自身的两个缺陷：G2 在"数量不同"时给出的位置没有意义（改为报告数量差异）、
  G6 失败时未计入汇总（改为每次运行每个门禁必有一行结果，并由自测锁定）。

## 3. S1 — M17 审查单元与覆盖契约

| 任务 | 内容 | 交付物 |
|---|---|---|
| ✅ T17.1 | `src/units.rs`：`ReviewUnit`/`Selection`/`ExcludeReason`/`UnitState`/`CoverageLedger`/`TerminalState` | 类型 + 单测 |
| ✅ T17.2 | `units::select`：纯函数，复用 `looks_generated`/`path_priority`/`split_sections`；v1 启用四类原因 | 单测（纯函数性、四类原因、`select_strict` 开关矩阵） |
| ✅ T17.3 | `diff::filter_text`/`truncate_text` 改为 `select` 薄包装并标 `deprecated`；迁移仓库内调用点 | 12 项既有 diff 单测仍绿 |
| ✅ T17.4 | `orchestrator::run_review` 用 `select` 一次算出 `Selection`，传进 `analyze` → `pipeline::run` | 选择结果在预览与运行间逐项一致（回归护栏测试） |
| ✅ T17.5 | 覆盖账本接线（分母冻结、状态迁移、终态计算），`Outcome::Published` 增加 `terminal`/`usage` | 状态机四类终态单测 |
| ✅ T17.6 | `--preview`（`ReviewArgs`）+ 在 LLM 凭据校验前短路；人类可读输出 + `--format json` 的 `units` 段 | 端到端：真实 PR 预览零 provider 请求（日志断言） |
| ✅ T17.7 | `report::build_review` 渲染覆盖声明行（`T::coverage_line`，`report_coverage` 默认开） | 摘要渲染单测 + 真实 PR 目视 |
| ✅ T17.8 | 文档同步：AGENTS.md 运维经验（若有坑）+ 本设计文档行号刷新 | diff 检查 |
| ✅ T17.9 | httpmock 端到端失败链路测试：`status_checks = true` + 模型不可达 → 状态检查描述携带覆盖计数 | 新增集成测试（`analysis_failure_status_check_states_coverage`：断言描述含 `coverage 0/1` 与 `analysis failed (fail-open)`，且 outcome 仍为 `AnalysisFailed`） |
| ✅ T17.10 | `select_strict`：启用 `secret-path` / `extension` / `default-path` 三类保留原因 | 交付：`SelectOptions`（strict 开关 + 未来 group_units 的落点）、内建语料表（密钥/产物）、固定评估顺序、`is_reviewable_path`；5 项新单测（默认零变化 / 严格排除带原因 / 多命中归属 / 语料编译与根目录+嵌套形式 / 扩展名白名单） |
| ✅ T17.11 | `group_units`：确定性成组 | 交付：三条配对规则（语言/地区变体、迁移方向对、测试伴随文件）、`SelectOptions.group_units`、组 id **由组键派生**（不是成员路径）；6 项新单测覆盖"默认不成组 / 变体合并且 id 稳定 / 方向对与测试伴随 / 无关文件不合并 / 成组只改记账不改派发文本 / 指纹形态" |

**S1 进度（滚动记录）**：

- ✅ T17.1 + T17.2 + T17.3 已交付：`src/units.rs`（类型 + `select`/`select_unbounded` 纯函数 +
  10 项单测）、`diff::filter_text` 收敛为薄包装、diff 内部辅助改为 `pub(crate)`；
- 实测：`cargo test --workspace` **229 passed / 0 failed**（基线 219 + 新增 10）、
  `cargo fmt --all -- --check` 干净、`cargo clippy --workspace --all-targets -- -D warnings` 干净、
  `scripts/verify-all.sh` 仍为 G1/G2 预期红（S3 收尾）而 G6/G7 绿；
- 过程中修掉一个真实回归风险：`Selection.excluded` 若与旧的"路径门禁计数"混用，
  prompt 里的排除说明会把预算截断也算进去——因此加了 `path_gate_excluded_count()` /
  `oversized_dropped()` 两个访问器，并在平价测试里锁定二者与旧行为的对应关系。
- ✅ T17.4 已交付：`run_review` 的两处 `filter_text`+`truncate_text` 换成一次
  `units::select`（锚定与增量各读同一份判定的 text），`diff` 在 orchestrator 的用法消失；
- ✅ T17.5 已交付：`CoverageLedger`（分母冻结 / 单元状态机 / `TerminalState`）+ 单测，
  `run_review` 在派发前冻结分母、派发前置 `running`、成功置 `covered`、失败置 `failed(reason)`、
  超预算放弃置 `truncated(reason)`，并把终态通过 `Outcome::Published` 透出（命令回复路径
  标为 `empty`，因为它们不涉及审查单元）；spec 14 §4 补了 v1 状态语义表；
- 计划偏差（有意，已在 spec 与本文记录）：`Outcome` 里的 `usage` 聚合随 T19.6 一起做，
  本任务只透出 `terminal`——`Usage` 的聚合点属于输出契约那条线，提前做会出现两处 token 账。
- ✅ T17.6 已交付：`prepare_inputs` 抽出（预览与运行同源的结构保证）、`orchestrator::preview`、
  `review --preview` / `--format human|json`、`Outcome::Previewed`；
  `Config::load_read_only()` 让只读命令不需要模型凭据（spec 01 同步说明）；`--format json`
  在非预览时**明确报错**而不是静默忽略（结构化输出属 M19）；
- 前移项（有意）：spec 16 §4.3 的"日志改 stderr"提前到本任务完成——否则预览的 stdout 会被日志
  污染，预览 JSON 不可用；T19.5 仍需补自动化 stdout 纯净性断言；
- ✅ T17.7 已交付：`CoverageSummary`（账本派生，报告/元数据/输出共用一份计数）、
  `T::coverage_line` 与 5 个预览标签（六语言）、`report_coverage` 配置（默认 true）、
  review 正文与 `hoverstare-meta` 同时带上覆盖（`units_total` / `units_covered` / `terminal`）、
  失败注记带覆盖（`failure_note`，状态检查描述不再只说"失败"）；
- 真实验证（T17.7 追加）：失败链路（假 provider、真实 PR）→ exit 0、日志
  `coverage: 0/1 unit(s) covered (terminal=partial)`、不写任何评论（fail-open 未被削弱）；
  中文预览 → `预览：全量审查 — 1 个审查单元，约 329 tokens，未调用模型`；
- ✅ T17.11 已交付：成组是"记账与派发粒度"，不是"派发内容"——单测直接断言
  `plain.text == grouped.text`。实现中单测抓到一处真实缺陷并已修：组 id 最初取"成员路径字典序最小者"，
  加入 `README.ja.md` 后组身份从 `group:README.md` 变成 `group:README.ja.md`（同一个组换身份），
  改为由组键派生后稳定（spec 14 §1 已记录这条教训与对应测试名）；真实验证以单测为准——
  公开 PR 里"同时改语言变体/测试伴随对"的样本是偶发的，不为凑证据去构造
- ✅ T17.10 已交付 + 真实验证（公开 PR `googleapis/google-cloud-go#20570`，154 文件）：
  默认 → 141 单元（含 46 个 `*.pb.go`）；`select_strict = true` → 95 单元、46 个
  `default-path`、`extension` 归零、被排除项全部带原因；`Cargo.lock` 在两种模式下都归
  `ignored`（spec 03 的内建 ignore 默认值先命中，spec 14 已注明这层与 `default-path` 的关系）
- **实测抓到一个真实缺口并修掉**：严格模式下 `lustre/go.mod` 是唯一被误判为"未知类型"的文件
  ——`extension` 白名单原本只沿用 spec 03 的优先级表，缺"依赖清单/schema/基础设施"类；已补显式
  补充清单（proto/tf/tfvars/hcl/mod/work/…）并写进单测与 AGENTS.md §7
- 本轮曾如实在 spec 标注的一处差距：`select_strict` 与三类保留原因**尚未实现**，
  spec 14 §2/§6/§9 已改为"保留、等 T17.10"，不留"existence by documentation"；

- 真实验证（无模型凭据、真实 PR `0xPlaygrounds/rig#2162`）：`--preview` 输出
  `preview: full scope — 1 review unit(s), ~329 tokens, no model calls`，stdout 无日志、
  stderr 无任何 provider 请求；`--format json` 的 stdout 通过 `jq` 形状校验
  （`units[0].status == "pending"`、`mode == "full"`、`unit_id` 前缀 `changeset:`）；
  带假凭据时 `--format json` 非预览 → exit 1 且给出明确提示；无凭据真实运行 → 仍按 spec 01
  exit 1（配置错误路径未被削弱）。

**验收**：spec 14 §10 四条。

**M17 收口（✅ 2026-09-24）——逐条证据**：

| spec 14 §10 | 证据 |
|---|---|
| 1. `--preview` 零模型调用、日志无 provider 请求 | 真实 PR `0xPlaygrounds/rig#2162`（无模型凭据）：stdout `preview: Full review — 1 review unit(s), ~329 tokens, no model calls`；stderr 中 provider 相关命中 0 |
| 2. 单元失败时覆盖可见、退出码仍 0 | 集成测试 `analysis_failure_status_check_states_coverage`（状态描述含 `coverage 0/1`）+ 真实失败链路日志 `coverage: 0/1 unit(s) covered (terminal=partial)` 且 exit 0 |
| 3. 预览与运行同源 | 结构保证（共用 `prepare_inputs`/`select`）+ 平价单测（与旧 filter/truncate 逐项一致）+ 真实 PR 上 `--preview` 与 `--format json` 数字一致 |
| 4. 单测覆盖 §9、fmt/clippy/test 全绿 | `cargo test --workspace` 250 passed / 0 failed；fmt 干净；`clippy --all-targets -D warnings` 干净；门禁自测 18/18 |

**M17 交付清单**：`src/units.rs`（Selection / ExcludeReason / CoverageLedger / CoverageSummary /
SelectOptions / 成组）、`--preview`（人类可读 + JSON）、覆盖声明（正文 + meta + 失败注记）、
`select_strict`、`group_units`；spec 01/14/16 同步修订；AGENTS.md §7 #34/#35 记录两条运维经验。

## 4. S2a — M19 输出契约 / S2b — M18 规则包

### S2a（M19）

| 任务 | 内容 | 交付物 |
|---|---|---|
| ✅ T19.1 | `src/output.rs`：`OutputFormat`、`RunMeta`、`FindingView`、稳定排序、路径规范化 | 单测 |
| ✅ T19.2 | JSON 渲染（封闭枚举、`schema_version`） | schema 校验单测 |
| ✅ T19.3 | SARIF 2.1.0 渲染（级别映射、`partialFingerprints`、`fixes`、`invocations`、文件级 result） | 映射逐项断言 |
| ✅ T19.4 | `ReviewArgs` 增加 `--format`/`--output`；`--output` 路径沙箱 | 路径逃逸拒绝单测 |
| ✅ T19.5 | tracing 初始化改 stderr（`src/cli.rs`）+ stdout 纯净性回归断言 | 断言：`--format json` 时 stdout 只有一个可解析 JSON |
| ✅ T19.6 | `pipeline::run` 聚合各 pass + verifier + reformat 的 `Usage` 到 `PipelineStats.usage` | 聚合单测 |
| T19.7 | 文档：README 补 `--format` 用法；threat-model 补"输出不含凭据/绝对路径"的验证方式 | diff 检查 |

**S2a 进度（滚动记录）**：

- ✅ T19.1/T19.2/T19.6 已交付：`src/output.rs`（OutputFormat / FindingView / RunMeta /
  Report / json / units 段 / 路径规范化 / `emit` 沙箱）、`report::BuiltReview.findings`
  （与评论渲染同一遍循环产出，两处不可能各说各话）、`UsageTotal` 聚合（含失败 pass、
  verifier 与 reformat 调用——低报成本是真实缺陷）；预览 JSON 也收敛到 output 模块，
  全仓只剩一处 JSON 组装
- 🟡 T19.4 部分：`--format json` 与 `--output`（工作区内、拒绝绝对路径与词法逃逸）已可用；
  `--format sarif` 目前**明确报错**而不是静默降级（T19.3 落地后放开）
- 端到端证据：集成测试 `run_review_emits_the_json_contract`——mock GitHub + mock provider，
  走完"3 路 pass → 两票入选 → 发布 review → 落盘契约文档"，断言 schema_version / run 元数据
  （terminal=ok、usage.calls=3、input_tokens=300）/ findings（path/line/side/severity/status）/
  units（covered）/ coverage / resolutions 空
- ✅ T19.3 已交付：`output::sarif`（2.1.0）——severity→level 逐项映射、指纹进
  `partialFingerprints`（跨 run 去重的依据）、`suggestion`→`fixes`（含 deletedRegion）、
  无法锚定的 finding 走**文件级**结果（无 region，不编造行号）、`invocations.executionSuccessful`
  由覆盖终态决定、未覆盖单元进 `toolExecutionNotifications`；5 项单测 + 真实运行路径的端到端断言
- ✅ T19.5 已交付：集成测试 `binary_keeps_stdout_clean_for_structured_output` 跑**真实二进制**，
  断言 `--format json` 的 stdout 是单个可解析文档、其中不含 INFO/WARN，且 stderr 确实有日志
- 🟡 T19.7 部分：英文 README 补了 `--preview` / `--format` 用法、威胁模型补了输出边界验证方式；
  **五份翻译 README 的同步随 T20.7（README 结构对齐）一起做**，避免同一段文字改两遍

**验收**：spec 16 §10 四条。

### S2b（M18）

| 任务 | 内容 | 交付物 |
|---|---|---|
| T18.1 | `src/rules.rs`：`RulePack`/`RuleCheck`/`RuleExample`/`Resolution`，`include_str!` 加载 + `validate()` | schema 校验单测（含"无指令性表述"） |
| T18.2 | 匹配：first match wins → 专指性排序 → 多包上限（`max_rule_packs`）→ 无命中走 default | 匹配矩阵单测 |
| T18.3 | 歧义嗅探（首批 `.m`，≤4KB，失败回退 + 日志） | 嗅探命中/失败回退单测 |
| T18.4 | 注入：`prompt::system_prompt` 增加 `rule_hit`，渲染 `[LANGUAGE RULES]`（字节上限 + 截断标记） | 提示渲染单测 |
| T18.5 | 初始包 rust / go / ts-js / python / ci-yaml / default（含正反样例） | 六份包文件 + 评审 |
| T18.6 | `rules list` / `rules check <path>` 子命令（只读、免 LLM 凭据） | 命令级测试 |
| T18.7 | 配置：`rule_packs`、`max_rule_packs` + 校验文案 | 配置单测 |

**验收**：spec 15 §12 四条。

> S2a 与 S2b 的写集重叠很小（都碰 `config.rs`/`i18n.rs`）：**不建议并行改同一文件**，
> 若并行推进，约定 S2a 独占 `config.rs` 的 `--format` 相关改动、S2b 独占规则包相关字段，
> 各自提交后再合并 `i18n.rs` 的新增文案。

## 5. S3 — M20 收尾（含一次性工作）

| 任务 | 内容 | 验收 |
|---|---|---|
| T20.6 | pin 全部 15 处 `uses:`（action.yml 2 / ci 4 / hoverstare 5 / release 3 / reposcope 1）为 40 位 SHA + `# vX.Y.Z` | `scripts/verify-action-pins.sh` 绿 |
| T20.7 | 六份 README 结构对齐（补英文本已有的 `## Contributing` 段，六语同批） | `check-doc-structure.sh` 绿 |
| T20.8 | 覆盖率基线（记录现状 + 排除项）与 `--full` 判定 | `verify-all.sh --full` 绿 |
| T20.9 | 密钥扫描（`.gitleaks.toml` + 平台推送保护） | gitleaks 干净 |
| T20.10 | CI 接线：`gates` job（G1–G7），工具版本固定 | CI 全绿；人为破坏任一项 → 变红 |
| T20.11 | CONTRIBUTING 增补门禁说明与上游 action 升级步骤 | diff 检查 |

**验收**：spec 17 §7 四条。

## 6. 提交与回滚策略

- **一个任务一次提交**，Conventional Commits（`feat(units): …`、`feat(rules): …`、`feat(output): …`、
  `chore(gates): …`、`docs(specs): …`），全部**签名**提交；任务内先跑质量门再提交；
- **spec 先于代码**：任何行为调整先落在 spec 提交里，代码提交引用 spec 章节；
- **不做发布**：不打 tag、不 `cargo publish`、不建 Release、不推 Marketplace（AGENTS.md §4.8）；
- **回滚**：按任务 revert（代码与其同批文档一起回）；spec 不回滚——它是目标状态，
  若目标变化就改 spec 而不是恢复旧 spec；
- **进度登记**：每完成一个任务就在本文表格的任务号前打 `✅`（`| ✅ T20.1 |`）；
  里程碑**整体**完成（该 spec 的 §验收逐条通过）后才勾选 `specs/README.md` 的对应条目。

## 7. 风险登记

| 风险 | 触发条件 | 影响 | 缓解 |
|---|---|---|---|
| `pipeline::run` 参数继续膨胀 | S1 加 `Selection`、S2b 加 `rule_hit` | 调用点难改 | 用 `ReviewPrompt` 结构体一次性聚合，后续只加字段 |
| 排除规则改变审查范围 | 误把 `select_strict` 默认开 | 覆盖口径与范围同时变化 | v1 只启用与现状等价四类（spec 14 §2） |
| tracing 改 stderr 影响既有假设 | S2a T19.5 | 文档/使用方假设 stdout | 与假设同批更新，并记入 AGENTS.md 运维经验 |
| 覆盖率门禁一上来就红 | S3 T20.8 | 阻塞开发 | 基线记录现状，只要求"不劣化" |
| 一次 pin 太多上游 | S3 T20.6 | 人工核对量大、易漏 | 脚本一次列全，逐处替换后由脚本复核 |
| CI 因门禁长时间红 | P0 就接线 | 信号疲劳 | P0 只本地跑；CI 接线与修复同批（S3） |

## 8. 完成定义（DoD）

1. `specs/14`–`17` 的 §验收 全部满足，可逐条指出证据（测试名 / 日志 / 真实 PR 记录）；
2. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets -- -D warnings`、
   `cargo test --workspace`、`scripts/verify-all.sh --full` 全绿；
3. `specs/README.md` 的 M17–M20 全部勾选，本计划表格状态列更新；
4. 文档与实现无漂移：spec、`docs/DESIGN.md`、`docs/threat-model.md`、AGENTS.md 运维经验同步；
5. 无任何发布动作，工作区干净、提交已推送。
