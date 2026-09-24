# 14 — 审查单元与覆盖契约（ReviewUnit & coverage）

## 目标

把"这次审查到底答应了审什么、实际审了什么"从**模型的自觉**变成**机器的保证**。

本 spec 定义三件事：

1. **审查单元（ReviewUnit）**：形态无关的基本单位，同时充当并发单元、上下文隔离单元、
   覆盖分母单元、报告聚合单元与预算记账单元；
2. **确定性选择**：把"哪些改动进入审查、哪些被排除、为什么"收敛成一个纯函数，
   预览与真实运行调用同一个函数（禁止两处各算一遍）；
3. **覆盖账本**：以选中集合为分母，逐单元记录终态，并在报告/结构化输出中显式呈现。

## 为什么需要

- 现状（spec 03）：过滤与截断发生在喂模型之前，**没有留下"承诺集合"**；模型在大 changeset
  上自行裁剪文件时，外部无从判断漏审是"没改"还是"没看"。
- 现状（spec 05）：多 pass 与 voting 以 finding 为单位聚合，缺少"以文件/单元为单位"的
  完成度视角。
- 长期形态：审查的输入来源会从"PR diff"扩展到"本地变更集 / 单文件 / 目录巡检 / 平台事件"，
  这些形态必须共用同一个基本单位，否则每加一种形态就要复制一遍管线。

## 事实与边界

- 本 spec 只定义**选择、单位与记账**；不改变 prompt 契约（spec 04）、不改变锚定与渲染
  （spec 06）、不改变指纹语义（spec 07）、不改变 fail-open 区间（spec 01）。
- 审查域永远只读（spec 04 硬约定）；本 spec 不引入新工具、不引入新的外部调用。
- 排除原因枚举是**穷举**的：新增原因必须改本 spec。
- 覆盖账本是**运行期事实**，不作为新的持久化来源：跨 run 的持久化仍走 GitHub 侧标记
  （spec 07 原则"状态存在平台侧"）。

## 1. 审查单元（ReviewUnit）

```
ReviewUnit {
  unit_id      // 内容无关的稳定标识：来源 + 主路径（+ 侧别/范围）
  source       // changeset | file        （预留扩展位，v1 只实现 changeset）
  files        // 组内文件路径（v1 单文件；成组时有序）
  fingerprint  // 内容指纹：组内各文件的原始 diff/内容哈希按 unit_id 排序后折叠
}
```

承担的角色：

| 角色 | 说明 |
|---|---|
| 并发单元 | 派发与并发上限以 unit 计（替代"以文件计"的隐式约定） |
| 上下文隔离单元 | 一个 unit 一次派发，单元之间不共享对话历史（避免互相污染注意力） |
| 覆盖分母单元 | 覆盖账本的分子/分母都按 unit 计 |
| 报告聚合单元 | 轮次报告与结构化输出按 unit 汇总 finding 数量与状态 |
| 预算记账单元 | token/工具调用预算可按 unit 归因（用于诊断"哪个单元吃掉了预算"） |

**成组规则（v1 只做确定性成组，且默认可关闭）**：同一目录相邻的镜像文件
（如多语言资源文件、`up`/`down` 成对的迁移脚本）可以合并为一个 unit。
成组只影响派发与记账粒度，**不改变** findings 的路径与行号语义。

## 2. 确定性选择（唯一的选择实现）

**签名**：

```
select(changeset, config, snapshot) -> Vec<Decision>
Decision { unit | excluded(ExcludeReason), estimated_tokens }
```

**要求**：

- **纯函数**：无网络、无模型、无 GitHub 调用、无全局状态；同一输入必得同一输出；
- **与预览同源**：`--preview`（§3）与真实运行必须调用同一实现，禁止各自推导
  （两处各算一遍必然漂移，这是本 spec 存在的直接理由）；
- **在派发前完成**：选择结果在第一次模型调用之前冻结，之后不再变动；
- **不感知运行结果**：预算耗尽、provider 失败、重试属于执行结果，不是选择理由。

**排除原因枚举（ExcludeReason）**：

| 原因 | 判据 |
|---|---|
| `binary` | 平台侧报告该文件没有 patch（二进制，或大到无法内联）；文本层面看不到该文件 |
| `deleted` | 整文件删除（无新内容可审，但仍进入锚定范围） |
| `generated` | 内容启发式：新增的前 5 行含 `Code generated ... DO NOT EDIT` |
| `secret-path` | 内建敏感路径（密钥、凭据、`.env` 类文件） |
| `ignored` | 用户 `ignore` glob 命中 |
| `extension` | 不在可审扩展名白名单 |
| `default-path` | 内建默认排除路径（vendor、构建产物、锁文件等） |
| `oversized` | 超出 `max_diff_kb` 预算被整文件丢弃（按文件优先级，首个文件保底保留） |

**评估顺序（固定，决定原因归属）**：

1. `ignored`（用户 `ignore` glob）
2. `secret-path` —— 仅 `select_strict = true`
3. `default-path` —— 仅 `select_strict = true`
4. `generated`（内容启发式：新增前 5 行含 `Code generated … DO NOT EDIT`）
5. `extension` —— 仅 `select_strict = true`
6. `deleted`（排除出分母，但留在文本里供锚定）
7. 尺寸预算超出 → `oversized`

顺序固定，因此同一文件只会得到唯一的原因归属；`select_strict = false` 时第 2/3/5 步整体跳过，
默认行为与引入本 spec 之前逐字相同。

**默认启用**：`ignored`、`generated`、`deleted`、`binary`（由平台侧"无 patch"体现）、
`oversized` 五类与改造前等价，因此默认开启。

**`select_strict = true` 启用**（会**缩小**审查范围，因此默认关闭）：

| 原因 | 语料类别（具体模式在代码里，随单测一起演进） |
|---|---|
| `secret-path` | 环境变量与凭据文件（`.env*`）、私钥与证书容器（`*.pem`/`*.key`/`*.p12`/`*.pfx`/`*.jks`/`id_*`）、凭据配置（`.netrc`/`.npmrc`/`credentials*`/`service-account*.json`/`secrets.{yml,yaml}`/`kubeconfig`/`.ssh/**`）、状态文件（`*.tfstate*`） |
| `default-path` | 依赖与产物目录（`node_modules`/`vendor`/`dist`/`build`/`target`）、压缩与映射产物（`*.min.js`/`*.min.css`/`*.map`）、锁文件（`*.lock`/`package-lock.json`/`yarn.lock`/`pnpm-lock.yaml`/`go.sum`）、生成代码（`*.generated.*`/`*.pb.go`/`*.g.dart`） |

> **与默认 `ignore` 的重叠**：锁文件与压缩产物已在 spec 03 的内建 `ignore` 默认值里，
> 因此默认配置下它们的归属是 `ignored`（用户层优先）；`default-path` 的同类模式是
> **用户清空 `ignore` 默认值时的兜底**，不是重复实现。实测（真实 PR）确认了这一点：
> `Cargo.lock` 在两种模式下都归 `ignored`。
| `extension` | 不在可审扩展名白名单内**且**不在无扩展名/前缀白名单内（`Dockerfile`/`Dockerfile.prod`/`Makefile.am`/`.gitignore` 等仍可审） |

`extension` 的白名单 = spec 03 的优先级表 **加上** 一份显式补充清单（依赖清单 `go.mod`/`go.work`、
schema 定义 `*.proto`/`*.graphql`、基础设施与模板 `*.tf`/`*.tfvars`/`*.hcl`/`*.j2`/`*.hbs`、
底层语言 `*.s`/`*.asm`/`*.sv`/`*.v`/`*.zig`/`*.nix`/`*.sol`）。补充清单的来历是实测：在真实 PR 上
跑严格模式时，**`lustre/go.mod` 是唯一被误判为"未知类型"的文件**——沿用旧扩展名表会漏掉
"值得看的清单文件"，因此把这类补上并写进单测。

三条硬性约束：

- **`ignore` 只能增加排除**：用户配置无法豁免 `secret-path`（安全门禁不可被反向覆盖）；
- **不得静默**：被排除的文件必须出现在预览的 `excluded` 与结构化输出的同一字段里，
  不得只在日志里一闪而过；
- **与默认行为隔离**：`select_strict` 开关是唯一入口，任何"默认顺手多排一点"都视为回归。

预算类排除（`budget`）**不属于**选择：它发生在运行期，记入覆盖账本（§4）而非排除原因。

## 3. 零成本预览

`hoverstare review --preview`（其他形态提供等价呈现：本地 CLI 直接打印，serve 提供
只读端点）打印选择结果：**不发模型调用、不写 GitHub、不消耗额度**。

输出内容：

- **人类可读**（默认）：一行汇总（作用域 / 单元数 / 估算 token）+ `will review` 列表
  + `excluded` 列表（每项带原因枚举，并标注"为锚定保留"的删除文件）；
- **`--format json`**：与 spec 16 的 `units` 段同构——`units[]` 为
  `{ unit_id, files, status }`，预览时 **`status` 恒为 `pending`**（尚未派发），
  另附 `mode`（`full` / `incremental`）、`estimated_tokens`、`excluded[]`
  （`{ path, reason, kept_in_text }`）与 `truncated[]`（被预算丢弃的路径）。

**同源性要求**：预览与真实运行必须走**同一段准备逻辑**（事件解析 → 变更集获取 →
`select` → 账本冻结）。实现上不允许预览自己再写一遍取 diff 与选择的代码——这是
"预览与运行同源"的落点，不是风格要求。

**范围界定**：`--preview` 只报告、不派发、不发布、不写状态检查；退出码沿用 fail-open
（GitHub 读取失败按分析区失败处理，配置错误仍 exit 1）。

## 4. 覆盖账本

**分母**：选择结果中的 `deleted` 之外的单元集合，在派发前冻结（此后不与实际情况互相
修订——分母不可被结果污染）。

**单元状态**：

| 状态 | 含义 |
|---|---|
| `pending` | 已选中，尚未派发 |
| `running` | 正在派发 |
| `covered` | 完成了该单元的审查（无论是否产出 finding） |
| `failed(reason)` | 该单元审查失败（模型/provider/超时/输出不合契约） |
| `truncated(budget)` | 因预算/超时被主动放弃 |

**终态判定**：

| 终态 | 条件 |
|---|---|
| `ok` | 分母为空或全部 `covered` |
| `partial` | 存在 `failed` 或 `truncated`（分母中仍有未覆盖项） |
| `empty` | 选择结果为空（全部被排除） |

**v1 的状态来源（与 spec 05 的派发方式一致）**：v1 的派发是"整份变更集 × N 路 pass"，
没有按单元派发的 worker，因此单元状态由**该次派发的结局**决定：

| 事件 | 单元状态 |
|---|---|
| 派发开始（模型调用前） | 全部 `running` |
| 分析成功返回 | 全部 `covered` |
| 分析区失败（全部 pass 失败、输出不合契约等） | 全部 `failed(reason)` |
| 主动放弃（超预算、超时） | 全部 `truncated(reason)` |

按单元区分的差异只在引入成组/按单元派发（`group_units` 或后续输入形态）之后才会出现。
`CoverageLedger` 提供单元级 setter 供那时使用，但**M17 不伪造单元级差异**：宁可诚实地
报"全部覆盖/全部失败"，也不假装知道模型真正逐文件看了什么。

**呈现**：轮次报告与 review 摘要输出一行覆盖声明，例如
`覆盖：12/14 个单元（2 个因预算截断）`。**`partial` 不改变退出码**（沿用 fail-open，
spec 01），但**不允许静默**：单元级失败必须显式出现在报告与结构化输出里。

**与 findings 的关系**：覆盖账本不参与投票，不改变"宁缺毋滥"的精度策略——它只负责
诚实交代范围。

## 5. 与既有 spec 的关系

| Spec | 变化 |
|---|---|
| 03（diff 引擎） | §过滤 / §截断的用户可见规则不变，但**判定收敛到本 spec §2 的纯函数**；03 保留解析与可评论行映射职责 |
| 04（agent backend） | 不变（工具集与 prompt 契约不受影响） |
| 05（审查管线） | 派发、并发、聚合的单位由"文件"改为 unit；投聚合逻辑不变 |
| 06（报告发布） | 摘要新增覆盖声明行；findings 渲染不变 |
| 07（增量状态） | 覆盖声明可随审查元数据一起写入（供后续轮次核对），指纹语义不变 |
| 13（上下文压缩） | 不变；压缩的确定性台账与覆盖账本各司其职（前者是"做了什么"，后者是"覆盖了什么"） |

## 6. 配置

| 配置 | 默认 | 说明 |
|---|---|---|
| `report_coverage` | `true` | 是否在摘要/结构化输出中呈现覆盖声明 |
| `group_units` | `false` | 是否启用确定性成组（§1） |
| `select_strict` | `false` | 是否启用会缩小审查范围的三类排除（§2） |

`--preview` 是 CLI 参数，不写进配置文件。

## 7. 可观测性

结构化日志（spec 04 的日志契约）：

- `selection_decided`：分母大小、排除原因分布、估算 token；
- `unit_dispatched` / `unit_covered` / `unit_failed`：unit_id、状态、耗时；
- `run_terminal`：终态（ok/partial/empty）与未覆盖清单。

## 8. 非目标

- 不做"用模型判断该怎么分组"——分组策略必须是确定性的或可确定性回退；
- 不做多来源并行（v1 只实现 changeset 来源；`file` 来源是为后续形态预留的字段）；
- 不把覆盖声明变成门禁（默认不改退出码，不阻塞 CI）；
- 不新增持久化存储（状态仍只在运行内存 + 平台侧标记里）。

## 9. 测试要点

- 纯函数性质测试：同输入同输出；**预览与运行同源**由结构保证（共用 `prepare_inputs` 与
  `units::select`），并由"`units::select` 与改造前 filter/truncate 逐项一致"的平价测试锁定；
- 排除原因枚举：**每个**原因至少一个 fixture（`secret-path` / `default-path` / `extension`
  用 `select_strict = true` 触发），并断言评估顺序（同时命中多类时归属到更靠前的原因）；
- `select_strict = false` 时三类严格门禁**逐项不发生**（对 `.env`、`node_modules/**`、
  未知扩展名各有反例），保证默认行为零变化；
- 覆盖账本状态机：全 covered / 混合 failed / 预算截断 / 空分母 四类终态；
- 分母冻结：运行期失败不得反向修改分母；
- 成组：镜像文件成组后，findings 的路径与行号不带组信息。

## 10. 验收

- `hoverstare review --preview` 在真实 PR 上零模型调用输出选择结果（日志中无 provider 请求）；
- 人为让一个单元失败：覆盖声明可见且标出未覆盖原因（终态 `partial`），退出码仍为 0（默认
  fail-open）——落在两处载体：状态检查描述（`status_checks = true` 时）与运行日志；
- 预览与运行同源：共用 `prepare_inputs`/`select`（结构保证）+ 平价单测 + 真实 PR 上
  `--preview` 与 `--preview --format json` 的数字一致；
- **真实 PR 上的严格模式对照**（154 文件的公开 PR）：默认 141 个单元（含 46 个 `*.pb.go`）
  → 严格模式 95 个单元、46 个 `default-path`、`extension` 归零，被排除项全部带原因；
- 单测覆盖 §9 全部条目，`cargo test --workspace` / clippy / fmt 全绿。

## 11. 里程碑

M17（见 [README 的里程碑计划](README.md#里程碑计划)）。
