# 16 — 输出契约（JSON 与 SARIF）

## 目标

给下游一个**稳定出口**：同一份审查结果，既能以行内评论发到平台，也能以结构化格式
交给 CI、安全面板、编辑器与其它 agent。输出契约是"形态无关"的：action、serve、
本地 CLI、未来的平台都产出同一种结构。

## 事实与边界

- 结构化输出是**附加出口**，不替代评论语义：评论仍按 spec 06 渲染，findings 仍按
  spec 05 投票、spec 06 锚定、spec 07 追踪。
- 输出层**不改变**判定结果，也不改变 fail-open 区间（spec 01）：输出失败本身按
  发布失败处理（走既有降级/退出码规则）。
- 契约层不持有凭据、不回传原始 prompt 或模型原始输出（只含结构化 finding 与计数）。
- SARIF 的消费方（平台安全面板）有自己的严重级别与去重语义，因此映射规则必须固定可测。

## 1. 命令行接口

| 参数 | 取值 | 默认 | 说明 |
|---|---|---|---|
| `--format` | `human` / `json` / `sarif` | `human` | 结构化格式写 stdout 的**唯一**内容 |
| `--output <path>` | 文件路径 | 无（stdout） | 写入文件；相对路径限定在工作区内 |

日志一律走 stderr（沿用现有日志契约），保证 stdout 可被管道直接消费。

## 2. JSON 契约

```
{
  "schema_version": "1.0",
  "run": {
    "form": "action" | "serve" | "cli",
    "repository": "owner/name",
    "change_request": 123,          // 无则省略
    "revision": "sha",              // 本次构建使用的版本（沿用流程级 pin）
    "model": "…",
    "usage": { "input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0 },
    "timing": { "started_at": "…", "duration_ms": 0 },
    "terminal": "ok" | "partial" | "empty"     // spec 14 的覆盖终态
  },
  "units": [ { "unit_id": "…", "files": ["…"],
               "status": "covered" | "failed" | "truncated" | "pending",
               "reason": "…" } ],   // pending 只出现在 --preview 的文档里（尚未派发）
  "findings": [ {
      "fingerprint": "…",           // spec 07 的稳定标识
      "path": "src/a.rs", "line": 12, "end_line": 14, "side": "new",
      "severity": "critical" | "high" | "medium" | "low",
      "title": "…", "body": "…",
      "suggestion": "…",            // 可选：建议替换内容
      "status": "new" | "carried_over" | "resolved",
      "related": [ { "path": "…", "line": 0 } ]
  } ],
  "resolutions": [ "fingerprint…" ]   // 本轮判定已修复
}
```

- **稳定排序**：`units` 按 `unit_id`，`findings` 按 `path` → `line` → `fingerprint`；
- **路径规范**：一律仓库相对、正斜杠、不使用绝对路径；
- **可枚举性**：`severity`、`status`、`terminal` 都是封闭枚举，新增值必须改本 spec；
  `units[].status` 的 `pending` 专用于 `--preview` 文档（预览不派发，因此单元状态只能是 pending）。

## 3. SARIF 契约

版本 2.1.0，单 run：

| SARIF 位置 | 我们的取值 |
|---|---|
| `runs[0].tool.driver.name` | `hoverstare` |
| `runs[0].tool.driver.version` | 二进制版本 |
| `runs[0].tool.driver.rules[]` | 每个严重级别一条规则（`hoverstare/critical` … ），`defaultConfiguration.level` 固定 |
| `results[].ruleId` | 上表对应规则 id |
| `results[].level` | `critical`/`high` → `error`；`medium` → `warning`；`low` → `note` |
| `results[].message.text` | `title` + 空行 + `body` |
| `results[].locations[0].physicalLocation.artifactLocation.uri` | 仓库相对路径 |
| `results[].locations[0].physicalLocation.region` | `startLine` / `endLine` |
| `results[].partialFingerprints` | 我们的 finding 指纹（跨 run 去重依赖它） |
| `results[].fixes[]` | 有 `suggestion` 时给出 `artifactChanges`（含 `deletedRegion` 与 `replacement`） |
| `runs[0].invocations[0].executionSuccessful` | 覆盖终态为 `ok` 时 true；`partial`/`empty` 或分析失败时 false |
| `runs[0].invocations[0].toolExecutionNotifications[]` | 覆盖账本中 `failed`/`truncated` 的单元逐条列出 |

**跨文件与不可锚定 finding**：spec 06 降级链末端的 finding（无法落到具体行）在评论区
只进正文段落，但在 SARIF 中仍可表达——没有 `region` 时按文件级结果输出。这样"看得出来
但没法行内评论"的信息不需要污染 PR 评论，也能被安全面板与下游工具看到。

## 4. 接入平台侧安全面板（仅文档片段）

使用者自行在 workflow 中上传（本 spec 只定义产物形态，不实现上传动作）：

```yaml
permissions:
  security-events: write
steps:
  - run: hoverstare review --format sarif --output hoverstare.sarif
  - uses: github/codeql-action/upload-sarif@<sha>   # 版本由使用者自行 pin
    with:
      sarif_file: hoverstare.sarif
```

## 5. 安全与隐私

- 输出内容**不含**密钥、令牌、原始 prompt、模型原始响应、工作区绝对路径；
- `--output` 相对路径限定在工作区内，禁止路径逃逸（与工具路径沙箱同一套判定）；
- `run.revision` 只记录版本/提交，不记录凭据或内部主机名；
- `fixes` 只包含被替换的那几行，不附带周边代码或整文件内容；

## 6. 兼容策略

- `schema_version` 语义化：新增可选字段不升大版本；删除/改语义升大版本；
- SARIF 侧严格按 2.1.0 输出，字段只增不改；
- 契约变更必须同步改本 spec 与 §9 的测试要点。

## 7. 与既有 spec 的关系

| Spec | 变化 |
|---|---|
| 01（CLI 配置） | 新增 `--format` / `--output` 参数（不写进配置文件） |
| 06（报告发布） | 渲染层拆出"契约层"：评论渲染与结构化输出共享同一份 findings 数据模型 |
| 07（增量状态） | 指纹直接复用到 `partialFingerprints`；`resolutions` 取自既有 resolve 判定 |
| 14（覆盖契约） | `units` 与 `terminal` 来自覆盖账本 |

## 8. 非目标

- 不做 CI 门禁策略（是否因 findings 让流水线变红由使用者自行决定，默认 fail-open 不变）；
- 不做 SARIF 之外的格式（JUnit、checkstyle 等）——留口，需要时另立 spec；
- 不实现上传动作本身（使用者按 §4 自行接入）。

## 9. 测试要点

- JSON：schema 校验（封闭枚举、必填字段）、稳定排序、路径规范化（反斜杠/绝对路径拒绝）；
- SARIF：级别映射表逐项断言、`partialFingerprints` 与指纹一致、有/无 suggestion 两种
  `fixes` 形态、`executionSuccessful` 与覆盖终态一致、`region` 缺失时的文件级结果；
- stdout 纯净性：`--format json|sarif` 时 stdout 不含日志；
- 安全：构造含密钥样式字符串与绝对路径的假 finding，断言输出中不出现；
- 兼容：旧字段集合仍可解析（未知字段忽略）。

## 10. 验收

- 真实 PR 上 `--format json` 与 `--format sarif` 产出可被对应解析器消费
  （JSON 通过 schema 校验，SARIF 通过 2.1.0 校验）；
- 同一 finding 的指纹在两次运行中一致，SARIF 去重不产生重复告警；
- 默认 `--format human` 行为与现状逐字一致（回归护栏）；
- 单测覆盖 §9 全部条目，`cargo test --workspace` / clippy / fmt 全绿。

## 11. 里程碑

M19（见 [README](README.md#里程碑计划)）。
