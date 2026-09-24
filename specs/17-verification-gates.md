# 17 — 工程门禁（verification gates）

## 目标

把"每次都要记得检查"的纪律变成**机器门禁**：能在本地一条命令跑完，在 CI 上阻塞合并。
门禁失败属于工程问题，**不进入 fail-open 区间**（fail-open 是运行时契约，spec 01），
两者不可混为一谈。

## 事实与边界

- 门禁只校验"我们自己的仓库与产物"；不改变产品运行时行为。
- 门禁脚本放在 `scripts/`，不引入新的运行时依赖（用 shell/awk 等已存在的工具；
  需要 Rust 工具链的用 `cargo install --locked` 显式声明版本）。
- 现状与门禁存在已知落差，落地时必须一并收拾（§4）：外部 action 引用目前是浮动 tag、
  六份 README 的二级标题数量不一致（英文 12 条、其余 11 条）、无覆盖率基线、
  无密钥扫描。

## 1. 门禁清单

| 编号 | 门禁 | 判据 | 失败语义 |
|---|---|---|---|
| G1 | 引用 pin | `action.yml` 与 `.github/workflows/*.yml` 中每个外部 `uses:` 都是 40 位 commit SHA + **单 token 来源注释**（`# vX.Y.Z`；上游只有分支引用时用分支名，如 `# stable`） | 阻塞 |
| G2 | 文档结构对齐 | 六份 README 的二级标题**结构**一致（数量 + 层级序列，不比文字） | 阻塞 |
| G3 | 依赖与许可审计 | 无已知安全公告；依赖许可在白名单内 | 阻塞 |
| G4 | 覆盖率不劣化 | 总行覆盖率不低于仓库内基线，且不大于基线以下 0.5 个百分点 | 阻塞 |
| G5 | 密钥扫描 | 提交中不含凭据样式的字符串 | 阻塞 |
| G6 | workflow 有效性 | `.github/workflows/*.yml` 通过 actionlint（保留现有 CI 步骤） | 阻塞 |
| G7 | spec 与模块一致 | `specs/` 下每个 spec 都在索引中；每个 `src/` 顶层模块都有对应 spec | 阻塞 |

### G1 引用 pin（`scripts/verify-action-pins.sh`）

- 只接受"整行都是 pin 引用"：`uses:` + `owner/repo@<40 hex>` + 一个来源注释 token，
  其它写法（引号、flow 映射、多余尾注、多 token 注释）一律判为不合规（宁可失败也不要猜）；
- **注释 token 的两种合法形态**：`vX.Y.Z`（有版本标签的上游）或标识符（分支名 / 三段版本号，
  如 `stable`、`1.83.0`）。**裸大版本如 `# v4` 仍然拒绝**——它无法说明 pin 的是哪个发布，
  这正是"看似 pin 住、实则含糊"的写法。注释只是来源记录，**pin 的是 SHA**；
- 为什么允许分支名：有些上游没有版本标签（`dtolnay/rust-toolchain` 用分支引用发布），
  对它只能 pin 到分支当时的 commit。注意这类 action 的语义来自 ref：pin 到 SHA 后必须显式
  传 `with: toolchain: …`，否则它无法判断要装哪个工具链；
- 本地引用（`./…`）豁免；
- **理由（针对本仓库的事实）**：`action.yml` 是对外发布的 composite action，
  使用者用 commit SHA pin 住我们之后，内层浮动 tag 仍会改变实际执行的代码；
  而 `.github/workflows/hoverstare.yml`（dogfood）持有 App private key、`gh_pat` 与
  GPG 签名密钥，`release.yml` 持有 `contents: write` 令牌——浮动上游 tag 一旦被移动，
  凭据的影响面是整个仓库，不只是 CI。

### G2 文档结构对齐（`scripts/check-doc-structure.sh`）

- 比对对象：`README.md` 与 `docs/readme/README.*.md`（六份），以及 `docs/` 下成对出现的
  多语言文档（若存在）；
- 比对**结构而非文字**：二级标题的数量与层级序列（翻译必然不同字，结构必须同形）；
- 起因：当前英文 README 12 条二级标题、其余五份 11 条——漂移已经发生，且没有任何检查
  能发现它；翻译漏更新会长期静默；
- 落地时先做一次结构对齐（补齐/合并差异标题），门禁随后生效。

### G3 依赖与许可审计（`cargo deny` + `cargo audit`）

- 安全公告阻塞；`deny.toml` 里显式列白名单例外（含理由），不允许"静默放行"；
- 许可：与本仓库 1PL 兼容的许可集合显式列出，新增依赖不匹配即失败；
- 建议随每次依赖变更运行，本地 `scripts/verify-all.sh` 覆盖。

### G4 覆盖率不劣化（`cargo llvm-cov`）

- 基线写在仓库内（`scripts/coverage-baseline.txt`），只允许上调；
  下调必须伴随本 spec 的修订说明；
- 门禁是"不劣化"，不是"追求数字"：新增能力的验收标准仍写在各自 spec 的 §验收里；
- 允许按目录排除（例如 `main.rs` 薄入口），排除项同样写进基线文件。

### G5 密钥扫描（`gitleaks` + 平台侧推送保护）

- CI 阻塞 + 可选 pre-commit；
- 同步在平台上开启推送保护（凭据被提交时直接拒绝）；
- 触发理由：本仓库有 `.env~` 备份文件泄漏被拦截的前科（AGENTS.md §4.7），
  仅靠人工评审不足以防住这类文件。

### G6 workflow 与脚本有效性

- actionlint：本地优先用已安装的 `actionlint` 二进制，CI 沿用 docker 镜像；
  配置与现有 CI 一致（`-shellcheck=`，只拦"工作流无效"类错误）；
- shellcheck：`scripts/*.sh` 与 `scripts/tests/*.sh` 全部检查（脚本是我们自己的门禁，
  不能自己带病）；两者任一失败即 G6 失败。

### G7 spec 与模块一致

- `specs/` 下每个 `.md`（除 `README.md`、`validation-*.md`）必须出现在索引表中；
- `src/` 下每个顶层模块名必须能对上某个 spec（豁免清单写进脚本）；
- 目的：spec-first 是硬约定，缺少索引就等于没有单一事实来源。

## 2. 运行方式

```
scripts/verify-all.sh                  # 默认：G1 G2 G6 G7（快，无需额外工具链）
scripts/verify-all.sh --full           # 追加 G3 G4 G5（依赖审计 / 覆盖率 / 密钥扫描）
scripts/verify-all.sh --strict         # 缺工具即算失败（CI 用这个）
scripts/verify-all.sh --list           # 列出全部门禁
scripts/tests/test-gates.sh            # 门禁自身的自测（正例/反例 fixture）
```

- **工具缺失的语义**：默认把缺工具记为 `SKIP` 并打印安装提示（本地不装 cargo-deny
  也能跑日常门禁）；`--strict` 下改为 `FAIL`。CI 使用 `--strict`，因此"缺工具"在 CI
  一定是阻塞项，不会静默通过。
- **汇总完整性**：每次运行都必须为每个执行过的门禁打印一行结果（`PASS`/`FAIL`/`SKIP`），
  失败但不出现在汇总里视为门禁自身的缺陷（已由 `scripts/tests/test-gates.sh` 锁定）。
- ✅ CI（M20 已接线）：`ci.yml` 新增 `gates` job，跑 `scripts/verify-all.sh --full --strict`
  （工具由 pin 住的 `taiki-e/install-action` 安装：actionlint / gitleaks / cargo-deny /
  cargo-audit / cargo-llvm-cov，并装 `llvm-tools-preview`），随后跑门禁自测
  `scripts/tests/test-gates.sh`。**本地能跑的就是 CI 强制的**：同一个入口脚本。

### G2 的诊断诚实性

层级序列（`22222223332222` 这类字符串）能发现结构漂移，但**无法定位缺失的小节**——
所有二级标题在序列里长得一样。因此：

- 二级标题数量不同 → 报告数量差异（例如 `reference=12, this=11`），并提示"有小节缺失或多出"；
- 数量相同而序列不同 → 才报告"首个不同的标题位置"。

不得用同一个"首个差异位置"去描述这两种情况（那会给出一个没有意义的位置）。

## 3. 与既有约定的关系

- `CONTRIBUTING.md` 的"四命令质量门"（build/fmt/clippy/test）保持不变，本 spec 是其超集；
- `actionlint` 保留原位（G6），不重复实现；
- 门禁不替代运行时契约：fail-open、只读工具集、密钥处理等由各自 spec 规定。

## 4. 落地时必须一并完成的事（M20 交付内容）

1. ✅ 已完成：`action.yml` 与四个 workflow 里的 15 处浮动引用改为 40 位 SHA + 来源注释
   （同主版本的最新补丁，不是顺手升大版本——升级是独立决定，流程写在 CONTRIBUTING）；
2. ✅ 六份 README 结构对齐：五份翻译补 `## Contributing` 段并同步 `--preview`/`--format` 用法；
3. ✅ 覆盖率基线：`scripts/coverage-baseline.txt` 记录实测 80.36% 行覆盖（M20 落地时），
   阈值 79.8%（比实测低半点为正常重构留余量；下调必须在文件里写明什么变得不可测）；
4. ✅ 密钥扫描：`.gitleaks.toml`（保留默认规则，只给"夹具目录"加白名单——那里的 token 是
   故意的假值）；平台侧推送保护需在仓库设置里开启，属于运维动作；
5. ✅ 门禁脚本与 `verify-all.sh` 早已就位（P0），本里程碑完成 CI 接线与一次性收尾。

**诚实边界**：`gitleaks`、`cargo-deny`、`cargo-audit` 在编写机上没有安装，因此
G3/G5 的**首次真实运行发生在 CI**（`--strict` 下缺工具即失败，不会静默通过）；
G4 已在本地用真实 `cargo llvm-cov` 跑出基线与判定结果。

## 5. 非目标

- 不做自动依赖升级机器人（依赖变更仍由人发起、门禁只做校验）；
- 不做性能回归门禁（构建时长/运行时长）——需要时另立 spec；
- 不做二进制体积门禁（同上）。

## 6. 测试要点

- G1：构造合法/非法两种引用（浮动 tag、短 SHA、缺版本注释、引号写法、flow 映射），
  断言只放行合法形式；
- G2：构造结构一致的文档对与故意漂移的文档对，断言失败并指出差异位置；
- G7：构造缺索引的 spec、缺 spec 的模块，断言失败并给出清单；
- `verify-all.sh`：在干净树上全绿；人为破坏任一项后返回非零。

## 7. 验收

- 本地 `scripts/verify-all.sh` 全绿，退出码 0；
- CI 上新增门禁 job 全绿；人为把任一门禁破坏后 CI 变红；
- §4 的五件事全部完成（含一次性 pin 与文档对齐）；
- 门禁脚本自身通过 shellcheck。

## 8. 里程碑

M20（见 [README](README.md#里程碑计划)）。
