# M17–M20 开发设计（实现视角）

> 阅读顺序：**spec 定义"要什么"，本文定义"怎么落进现在的代码里"**。
> 本文与 `specs/` 冲突时以 spec 为准；实现中发现 spec 不成立，先改 spec 再改代码（AGENTS.md §4.1）。
> 施工顺序、批次与验收见 [`m17-m20-plan.md`](m17-m20-plan.md)。
>
> 文中行号锚定提交 `e576780`（spec 14–17 定稿那次）；行号会随代码演进漂移，施工时以符号名为准。

## 0. 现状锚点（施工前必须对得上）

主链路（review 形态）：

```
orchestrator::run_review            src/orchestrator.rs:129
  ├─ 取 PR diff（全量 / compare 增量）           :213 / :239
  ├─ diff::filter_text(&diff, &cfg.ignore)      :213 / :239   → (String, 排除计数)
  ├─ diff::truncate_text(&filtered, max_diff_kb) :214 / :240  → Truncation{ text, truncated_files }
  ├─ orchestrator::analyze                      :591
  │    └─ pipeline::run(backend, cfg, parsed, diff_text,
  │                     truncated_files, shared, mode, instructions)   :613
  ├─ report::build_review(...) → BuiltReview    report.rs:59
  ├─ gh.create_review(...)                      :391  ── 失败 → report::render_fallback_comment（:249）
  └─ post_status_checks(...)（可选）             :519
```

关键既有类型（施工时直接复用，不新造平行概念）：

| 类型 | 位置 | 施工关注点 |
|---|---|---|
| `ParsedDiff` / `FileDiff` / `FileKind` / `Hunk` / `DiffLine` | `src/diff.rs:14-50` | 选择器的输入；`FileKind` 已能区分删除 |
| `Truncation { text, truncated_files }` | `src/diff.rs:321` | 只能给出"整文件被丢"的名单，没有逐文件原因 |
| `Finding` / `Location` / `AnalysisResult` | `src/findings.rs:13-36` | 输出契约的数据源 |
| `PipelineOutcome` / `PipelineStats` | `src/pipeline.rs:57-72` | 覆盖与 usage 的落点 |
| `BuiltReview` / `ReviewContext` | `src/report.rs:15-58` | 摘要渲染落点 |
| `RepoInstructions { render() }` | `src/instructions.rs:40-90` | 规则包注入的邻居（不合并，见 §3） |
| `ReviewMode { incremental_from, open_findings }` | `src/prompt.rs:61` | 提示构建参数聚合的地方 |
| `ToolShared` | `src/agent/tools.rs:41` | 工具上下文；spec 14 不新增工具 |
| `Config` / `Severity` | `src/config.rs:18-77` | 新配置项落点 |
| `T`（i18n 文本目录） | `src/i18n.rs:56+` | 新增用户可见文案的落点；机器可读内容不本地化 |
| `Usage` | `src/agent/mod.rs:76` | 每次调用的 token 计数（含 cached）；已聚合到 `ReviewRun` |

现状缺口（每条对应一个 spec，施工时逐条消掉）：

1. 选择逻辑**只在内部**：`filter_text` 只回"排除了几段"，`truncate_text` 只回被丢的整文件名单——
   没有逐文件原因、没有承诺集合、没有预览（spec 14）。
2. 规则只有**仓库级**（`RepoInstructions`），没有按文件语言聚焦的检查要点（spec 15）。
3. 输出只有人类可读评论 + `--dry-run` 的调试打印；**日志默认写 stdout**
   （`src/cli.rs:104-108` 的 `tracing_subscriber::fmt().init()`），因此当前 stdout 不能直接当管道用（spec 16）。
4. 工程门禁**没有脚本**：`scripts/` 为空目录，`cargo fmt/clippy/test` 之外无自动化校验（spec 17）。

## 1. 文件落点总览

| spec | 新增 | 修改 |
|---|---|---|
| 14 | `src/units.rs` | `src/diff.rs`（`select` 成为唯一判定，旧函数变薄包装）、`src/orchestrator.rs`、`src/pipeline.rs`、`src/report.rs`、`src/cli.rs`、`src/i18n.rs` |
| 15 | `src/rules.rs`、`src/rules/*.json`（`include_str!` 内置） | `src/prompt.rs`、`src/cli.rs`、`src/config.rs`、`src/i18n.rs` |
| 16 | `src/output.rs` | `src/cli.rs`、`src/findings.rs`、`src/pipeline.rs`、`src/i18n.rs` |
| 17 | `scripts/verify-action-pins.sh`、`scripts/check-doc-structure.sh`、`scripts/verify-spec-index.sh`、`scripts/verify-all.sh`、`deny.toml`、`scripts/coverage-baseline.txt`、`.gitleaks.toml` | `.github/workflows/ci.yml`、`action.yml` + 四个 workflow（pin）、六份 README（结构对齐） |

原则：**一个 spec 对应一个 `src/` 顶层模块**（specs/README.md 开发约定）；新增模块先补 spec。

## 2. spec 14：审查单元与覆盖契约

### 2.1 新类型（`src/units.rs`）

```rust
/// 单元的输入来源。v1 只实现 Changeset，`File` 是后续形态的预留位。
pub enum UnitSource { Changeset, File }

/// 审查单元：并发 / 上下文隔离 / 覆盖分母 / 报告聚合 / 预算记账的最小单位。
pub struct ReviewUnit {
    pub unit_id: String,        // 内容无关："{source}:{主路径}"（成组时按路径排序后拼接）
    pub source: UnitSource,
    pub files: Vec<String>,     // v1 单文件；`group_units` 开启时可能多文件，保持有序
    pub unit_fp: String,        // 内容指纹（与 finding 指纹区分命名，避免混用）
}

/// 选择结果：派发文本 + 分母 + 逐项原因。
pub struct Selection {
    pub text: String,                  // 实际喂模型的 diff 文本
    pub units: Vec<ReviewUnit>,        // 覆盖分母
    pub excluded: Vec<ExcludedFile>,   // { path, reason, kept_in_text }
    pub estimated_tokens: u64,
}

pub enum ExcludeReason {
    Binary, Deleted, Generated, Ignored, Oversized, SecretPath, Extension, DefaultPath,
}

/// 覆盖账本（运行期事实，不持久化）。
pub enum UnitState { Pending, Running, Covered, Failed(String), Truncated(String) }
pub struct CoverageLedger { /* 分母：Vec<String>（派发前冻结）；状态：BTreeMap<String, UnitState> */ }
pub enum TerminalState { Ok, Partial, Empty }
```

两个指纹必须**命名可分**：`unit_fp`（内容指纹，用于回答"这次审的是这份内容吗"）与
`state::fingerprint`（finding 指纹，用于跨 commit 追踪，spec 07）职责不同，不得互相替代。

### 2.2 选择器（唯一实现）

```rust
/// 纯函数：无 IO、无网络、无模型、无全局状态；同一输入必得同一输出。
/// 带预算的入口给分析路径用；锚定路径需要"不截断的同一份判定"，走 select_unbounded。
pub fn select(input: &str, ignore: &GlobSet, max_diff_kb: usize) -> Selection
pub fn select_unbounded(input: &str, ignore: &GlobSet) -> Selection
```

**已实现（T17.1–T17.3）时的两点实现事实**：

1. 选择作用于 **diff 文本**（与 spec 03 的"解析前过滤"同层），而**单元是从最终派发的文本里派生**的——
   这样分母永远不会大于模型实际收到的内容；已截断/被排除的文件不可能混进分母。
2. `Selection.excluded` 是两类排除的**并集**：路径门禁（ignore glob / generated 启发式）与尺寸预算
   （`oversized`）。历史语义由两个访问器分别取回：`path_gate_excluded_count()`（对应旧的
   `filter_text` 计数，用于"all changes filtered out by rules (N files)"与 prompt 说明）与
   `oversized_dropped()`（对应旧的 `truncate_text.truncated_files`）。**T17.4 接线时必须用这两个
   访问器**，不要用 `excluded_count()` 顶替其中一个。
3. `estimated_tokens` 复用 `agent::compaction::estimate_tokens`（与上下文压缩同一套估算口径，
   避免出现第二套 token 账）。

- 复用现有的 `diff::looks_generated`、`diff::path_priority`、`split_sections`/`section_path`
  等内部能力（必要时把它们从 `fn` 提升为 `pub(crate)`，不复制实现）；
- **v1 启用范围**：`deleted`、`binary`（平台侧"无 patch"视为 binary）、`ignore-rule`、
  `oversized`——这四类与现状等价，因此默认开启；`secret-path`、`extension`、`default-path`
  默认关闭，由 `select_strict = true` 开启（spec 14 §2 的"v1 启用原因集合"）；
- `diff::filter_text` / `diff::truncate_text` 保留为**薄包装**（内部调用 `select` 后拼回文本），
  以保住 `src/diff.rs` 既有 12 项单测与其它调用点；旧签名标 `#[deprecated(note = "use units::select")]`
  并在 M17 内把仓库内调用点全部迁走；
- 预览与真实运行**必须**调用同一个 `select`（spec 14 §2 的要求）；禁止在预览路径里
  另写一份判定。

### 2.3 预览（`--preview`）——T17.6，未实现

- 落点：`src/cli.rs` 的 `ReviewArgs` 增加 `--preview`；
- 时序：**在 LLM 凭据校验之前短路**（沿用 `help` 命令"无凭据可跑"的先例）——
  预览不需要模型，只需要读事件与 diff 的 GitHub 权限；
- 输出：人类可读表格（`will_review` / `excluded(reason)` / 汇总行）；`--format json`
  时输出 spec 16 的 `units` 段；
- 退出码：0（即使有排除项）；配置错误仍按 spec 01 走 exit 1；
- 日志走 stderr（见 §4.3）。

### 2.4 覆盖账本接线——T17.5，未实现

| 时点 | 动作 |
|---|---|
| `select` 之后、第一次派发之前 | 用选中的 non-deleted 单元**冻结分母** |
| `pipeline::run` 每次单元开始/结束 | `Running` → `Covered` |
| 单元级失败（provider 失败、输出不合契约、超时） | `Failed(reason)` |
| 预算/超时导致的主动放弃 | `Truncated(budget)` |
| run 结束 | 计算 `TerminalState`，交给报告与输出层 |

- 记账点放在 `pipeline::run` 内部（那里才知道每个单元是否真的完成），**不放在**模型输出解析里——
  模型说"我看完了"不算完成；
- `report::build_review` 新增参数 `&CoverageLedger`，摘要行由 `T::coverage_line(...)` 渲染
  （新增 i18n 文案；机器可读部分不进文案）；
- `Outcome::Published` 增加 `terminal: TerminalState` 与 `usage: Usage` 字段，供 spec 16 的输出层消费。

## 3. spec 15：语言规则包

### 3.1 数据与加载（`src/rules.rs` + `src/rules/*.json`）

```rust
pub struct RulePack {
    pub id: String, pub version: String, pub language: String,
    pub scope: Vec<String>,          // 路径 glob，声明顺序即优先级
    pub checks: Vec<RuleCheck>,      // { title, items: Vec<String> }
    pub examples: Vec<RuleExample>,  // { positive: bool, note, code } —— 不进 prompt
}
pub struct Resolution { pub pack_id: String, pub matched_pattern: String, pub sniffed: bool }
pub fn packs() -> &'static [RulePack]          // include_str! 编译进产物
pub fn resolve(path: &str, packs: &[RulePack], max: usize) -> Vec<Resolution>
```

- **编译进产物**（`include_str!` + 启动时 `OnceLock` 解析一次）：不引入新依赖、不读工作区，
  因此不新增外部输入面（spec 15 §事实与边界）；
- `RulePack::validate()` 在单测里跑：id/version/scope/checks/examples 齐备、文本中不得出现
  指令性表述（"忽略前述规则"等），这条是评审拒绝项。

### 3.2 匹配与嗅探

- 规范化路径后按 scope **first match wins**；
- 多包命中：按"模式专指性（更长且更具体优先）→ 声明顺序"取最多 `max_rule_packs`（默认 2），
  被丢弃的命中写日志；
- 歧义嗅探：只对**声明了歧义**的扩展名（首批：`.m`）读文件开头 ≤4KB 判语言，
  读失败/超时 → `sniffed=false` 回退到声明顺序命中，并写日志（不静默）；
- 无命中 → `default` 包。

### 3.3 注入

- 注入点在 `src/prompt.rs`：`system_prompt(...)` 增加 `rule_hit: Option<&Resolution>`，
  渲染 `[LANGUAGE RULES]` 段（标题固定），段首声明"与核心规则冲突时以核心规则为准"；
- 字节上限沿用仓库指令的常量风格（`MAX_FILE_BYTES` = 4096、`MAX_TOTAL_BYTES` = 8192，`src/instructions.rs:34-36`），
  超限截断并显式标记；
- `RepoInstructions` **保持不动**：规则包是另一段材料，不合并进仓库指令的渲染（权威层次不同）；
- 与多 pass 正交：`LENSES`（`src/pipeline.rs:50` 附近）不动——不新增第四路 pass。

### 3.4 自检命令

- `Command::Rules(RulesArgs { list | check{ path } })`：只读，不校验 LLM 凭据，不写平台；
- 输出人类可读部分走 `T`；`pack id` / `matched_pattern` 等机器可读字段不本地化。

## 4. spec 16：输出契约

### 4.1 新模块（`src/output.rs`）

```rust
pub enum OutputFormat { Human, Json, Sarif }

pub struct RunMeta<'a> {
    pub form: &'a str,                 // action | serve | cli
    pub repository: &'a str,
    pub change_request: Option<u64>,
    pub revision: &'a str,             // 流程级 pin 的 revision
    pub model: &'a str,
    pub usage: &'a Usage,
    pub duration_ms: u128,
    pub terminal: TerminalState,       // 来自 spec 14 的覆盖账本
}

pub fn json(result: &AnalysisResult, units: &CoverageLedger, meta: &RunMeta) -> anyhow::Result<String>
pub fn sarif(result: &AnalysisResult, units: &CoverageLedger, meta: &RunMeta) -> anyhow::Result<String>
```

- **`FindingView`**：输出前把 `Finding` 投影为契约视图——`fingerprint`（复用
  `state::fingerprint`）、`path`（规范化：仓库相对 + 正斜杠）、`status`
  （`new`/`carried_over`/`resolved`，由既有增量判定给出）、`related[]`（`additional_locations`）；
- **稳定排序**：`units` 按 `unit_id`，`findings` 按 `path` → `line` → `fingerprint`；
  排序在组装视图时做一次，渲染函数不得各自排；
- **SARIF 映射**：按 spec 16 §3 的表实现；`fixes` 由 `suggestion` 生成
  （`artifactChanges` + `deletedRegion` 覆盖当前 finding 的行范围）；
  `partialFingerprints` 用 finding 指纹；`invocations[0].executionSuccessful` 取 `terminal == Ok`；
  账本中的 `failed`/`truncated` 单元写进 `toolExecutionNotifications`；
  没有可锚定行（spec 06 降级链末端）的 finding 输出**文件级** result（无 `region`）。

### 4.2 CLI 接线

- `ReviewArgs` 增加 `--format human|json|sarif`（默认 `human`）与 `--output <path>`；
- `--dry-run` 保留原语义（不发布）；与 `--format` 组合时以 `--format` 的结构化产物为准，
  旧的调试打印让位（避免两种 JSON 并存）；
- 路径安全：`--output` 相对路径经与工具沙箱同源的判定，禁止逃逸工作区。

### 4.3 stdout 纯净性（现存的坑）

现在 `tracing_subscriber::fmt().init()`（`src/cli.rs:104-108`）写的是 **stdout**，
一旦 `--format json|sarif` 走 stdout，日志就会污染管道。施工时必须：

- 初始化改成写 stderr（`.with_writer(std::io::stderr)`），并写一条注释说明原因；
- 增加一条**回归断言**：`--format json` 时 stdout 只包含一个可解析的 JSON 文档；
- 该改动影响所有形态的日志位置（人类可读形态下 stderr 同样是正确位置），
  需在 AGENTS.md 运维经验里记一笔（施工时一并更新）。

### 4.4 usage 聚合

`pipeline::run` 在各 pass 结束后把 `ReviewRun.usage` 相加写入 `PipelineStats.usage`
（`Usage::add` 已存在，`src/agent/mod.rs:87`）；`verifier` 与 `reformat` 的调用也计入——
否则"这次审查花了多少 token"会低报。

## 5. spec 17：工程门禁

### 5.1 脚本判定

| 脚本 | 判定 | 备注 |
|---|---|---|
| `scripts/verify-action-pins.sh` | 遍历 `action.yml` + `.github/workflows/*.yml`：每个 `uses:` 必须是 `owner/repo@<40hex>` 且行尾 `# vX.Y.Z`；非该形态（引号、flow 映射、短 SHA、缺注释）**一律失败** | 用 `grep -n` 定位 + `awk`/`sed` 判定；`./` 本地引用豁免 |
| `scripts/check-doc-structure.sh` | 提取六份 README 的 `##` 序列（数量 + 层级），不一致即失败并打印差异 | `awk` 提取、`diff` 比对；输出直接给出行号，便于修 |
| `scripts/verify-spec-index.sh` | `specs/*.md`（除 `README.md`、`validation-*.md`）必须出现在索引表；每个 `src/` 顶层模块的模块注释（前 12 行）必须声明它实现的 spec（`main`/`lib` 为薄入口豁免） | 豁免不靠硬编码清单：要求模块自证 spec，新增模块必须先写 spec 注释 |
| `scripts/verify-all.sh` | 默认 G1/G2/G6/G7；`--full` 追加 G3/G4/G5；`--strict` 把缺工具算失败（CI 用） | 每个执行过的门禁必须有一行结果；退出码聚合；CI 用同一入口 |
| `scripts/tests/test-gates.sh` | 门禁自测：正例/反例 fixture 断言退出码与诊断文本 | 覆盖 G1 的五种非法形态、G2 的围栏忽略与漂移、汇总完整性 |

### 5.2 一次性工作（施工时不能漏）

1. **pin 全部外部引用**（实测共 **15 处** `uses:` 行、7 个上游 action）：
   `action.yml` 2 处（`actions/cache@v4`、`actions/create-github-app-token@v2`）、
   `.github/workflows/ci.yml` 4 处、`hoverstare.yml` 5 处、`release.yml` 3 处、
   `reposcope.yml` 1 处；逐处替换为 40 位 SHA + `# vX.Y.Z` 注释。
   （注意 `grep "uses:"` 会把 `statuses: write` 这类行误判为命中，脚本必须用
   行首锚定的模式 `^\s*(-\s+)?uses:`。）
2. **六份 README 结构对齐**：英文有 12 个二级标题，其余五份 11 个——缺的是
   `## Contributing`（对应各语言的"贡献"段）。补齐该段（翻译自英文原文，保持六语内容一致）
   或按结构重排，二选一但要六份一致；
3. **覆盖率基线**：先记录现状，再判定"不劣化"；`scripts/coverage-baseline.txt` 记录
   总行覆盖率与排除项；
4. **密钥扫描**：`.gitleaks.toml`（含允许清单与理由）+ 平台侧推送保护；
5. **CI 接线**：`ci.yml` 新增 `gates` job；`cargo-deny`/`cargo-llvm-cov`/`gitleaks` 版本固定
   （`cargo install --locked --version <x.y.z>`），避免门禁自身漂移。

## 6. 跨 spec 的公共改动

| 位置 | 改动 |
|---|---|
| `src/config.rs` | 新增 `report_coverage`（默认 true）、`group_units`（false）、`select_strict`（false）、`rule_packs`（true）、`max_rule_packs`（2）；`Config::load` 增加校验与错误文案 |
| `src/i18n.rs` | 新增 `coverage_line`、预览标题、`rules check` 人类可读部分、结构化输出的人类提示；机器可读字段一律不进 `T` |
| `src/cli.rs` | `--preview` / `--format` / `--output`；`rules` 子命令；tracing writer 改 stderr |
| `Outcome` | 增加 `terminal` 与 `usage`，让"这次审了什么、花了多少"成为一等输出 |
| 日志 | 新增 `selection_decided` / `unit_dispatched` / `unit_covered` / `unit_failed` / `rule_resolved` / `rule_injected` / `run_terminal`，全部走 tracing（stderr） |

## 7. 测试与验证落点

| 层 | 内容 |
|---|---|
| `src/units.rs` 单测 | `select` 的纯函数性（同输入同输出）、四类启用原因各一 fixture、`select_strict` 开关矩阵、成组开关、账本状态机四类终态、分母冻结 |
| `src/rules.rs` 单测 | 每包 schema 校验、文本无指令性表述、first match wins、专指性排序、多包上限、嗅探命中与失败回退、无命中走 default |
| `src/output.rs` 单测 | JSON schema 校验、封闭枚举、稳定排序、路径规范化、SARIF 级别映射逐项、`fixes` 两种形态、`executionSuccessful` 与终态一致、region 缺失走文件级、stdout 纯净、输出中不含凭据样式字符串与绝对路径 |
| 脚本 | G1/G2 的正例与反例 fixture（合法/非法 `uses:` 行、结构一致/漂移的文档对） |
| 端到端 | `examples/local_review` 增加 `--preview`、`--format sarif` 两种本地跑法；真实 PR 上各跑一次（预览零模型调用：日志无 provider 请求） |

## 8. 风险与缓解

| 风险 | 影响 | 缓解 |
|---|---|---|
| `pipeline::run` 参数继续膨胀 | 调用点难改、易错 | 用 `ReviewPrompt` 结构体聚合 `instructions` + `rule_hit`，不同步加参数 |
| 新排除规则改变审查范围 | 覆盖口径与范围同时变化，无法归因 | v1 只启用与现状等价的四类；其余走 `select_strict`（spec 14 §2） |
| tracing 改 stderr 影响既有日志消费者 | 现有 dogfood/文档假设 stdout | 改动与该假设一起更新（§4.3），并在 AGENTS.md 运维经验记一笔 |
| 覆盖率门禁一上来就红 | 阻塞所有开发 | 基线先记录现状，只要求"不劣化"，上调单独提 |
| README 结构对齐改动用户可见文档 | 影响六语言一致性 | 只补结构（补 `## Contributing`），不重写内容；六份同批提交 |
| pin 之后上游 action 不再自动跟进修 | 安全修复需人工升级 | 在门禁脚本的报错信息里写明升级步骤；依赖升级作为常规维护动作记入 CONTRIBUTING |
