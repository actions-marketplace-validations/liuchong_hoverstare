# 自部署 HoverStare serve（零配置 hoverstare[bot]）

> spec 10：用户安装 GitHub App 后，无需 workflow、无需任何 secrets，
> 即可获得 hoverstare[bot] 的 PR 审查与 `@hoverstare` 命令。

## 工作原理

```
GitHub App webhook → hoverstare serve（本服务）
  → HMAC 验签 → 事件路由（PR / @hoverstare 评论）
  → 取安装令牌 → 临时工作区 git clone → 复用审查编排 → 以 hoverstare[bot] 发布
```

## 准备：注册你自己的 GitHub App（一次性）

> 用自己的 App 部署时，身份就是"你的 App 名[bot]"；想共用官方 HoverStare 身份需等公开实例。

1. *Settings → Developer settings → GitHub Apps → New GitHub App*
2. 权限：Contents **Read**、Pull requests **Read & write**、Issues **Read & write**、
   Commit statuses **Read & write**
3. Webhook **Active 打开**：URL = `https://<你的域名>/webhook`，Secret 填一个随机串
4. 记录 **App ID**，生成 **private key**（.pem）
5. 把 App 安装到目标仓库

## 运行

### Docker（推荐）

```bash
docker build -t hoverstare .
docker run -d --name hoverstare \
  -p 8080:8080 \
  -e HOVERSTARE_APP_ID=4331106 \
  -e HOVERSTARE_APP_PRIVATE_KEY_PATH=/run/secrets/app.pem \
  -e HOVERSTARE_WEBHOOK_SECRET=<webhook secret> \
  -e OPENAI_API_KEY=<LLM key> \
  -e OPENAI_BASE_URL=https://api.kimi.com/coding/v1 \
  -e HOVERSTARE_MODEL=kimi-for-coding \
  -v $(pwd)/app.pem:/run/secrets/app.pem:ro \
  hoverstare
```

### 裸机

```bash
export HOVERSTARE_APP_ID=... \
       HOVERSTARE_APP_PRIVATE_KEY_PATH=/path/to/app.pem \
       HOVERSTARE_WEBHOOK_SECRET=... \
       OPENAI_API_KEY=... OPENAI_BASE_URL=... HOVERSTARE_MODEL=...
hoverstare serve --port 8080
```

### fly.io 示例

```toml
# fly.toml
app = "hoverstare-serve"
[build]
  dockerfile = "Dockerfile"
[http_service]
  internal_port = 8080
  force_https = true
[[vm]]
  size = "shared-cpu-1x"
  memory = "512mb"
```

```bash
fly secrets set HOVERSTARE_APP_ID=... HOVERSTARE_APP_PRIVATE_KEY_PATH=/run/secrets/app.pem \
  HOVERSTARE_WEBHOOK_SECRET=... OPENAI_API_KEY=... OPENAI_BASE_URL=... HOVERSTARE_MODEL=...
fly deploy
```

然后把 App 的 Webhook URL 配成 `https://<app>.fly.dev/webhook`。

## 环境变量总表

| env | 必填 | 说明 |
|---|---|---|
| `HOVERSTARE_APP_ID` | ✅ | GitHub App ID |
| `HOVERSTARE_APP_PRIVATE_KEY_PATH` | ✅ | App private key（.pem）文件路径 |
| `HOVERSTARE_WEBHOOK_SECRET` | ✅ | App webhook secret（验签用） |
| `OPENAI_API_KEY` 或 `ANTHROPIC_API_KEY` | ✅ | LLM 凭据 |
| `OPENAI_BASE_URL` | | OpenAI 兼容端点 |
| `HOVERSTARE_MODEL` | | 主审模型（OpenAI 兼容端点必填） |
| `HOVERSTARE_REFORMAT_MODEL` | | 输出修复模型 |
| `HOVERSTARE_THINKING` | | 思考模式开关（`enabled`/`disabled`，仅 OpenAI 兼容端点） |
| `HOVERSTARE_REASONING_EFFORT` | | 思考强度（`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`） |
| `HOVERSTARE_CONTEXT_TOKENS` | | 模型上下文窗口（token，如 DeepSeek `deepseek-flash` 为 `1000000`） |
| `HOVERSTARE_COMPACTION` | | 上下文压缩开关（`true`/`false`，spec 13） |
| `HOVERSTARE_COMPACTION_THRESHOLD_RATIO` | | 估算输入达到窗口的比例即压缩（默认 0.75） |
| `HOVERSTARE_COMPACTION_KEEP_RATIO` | | 压缩后保留最近这段比例的窗口（默认 0.25） |
| `HOVERSTARE_SUMMARY_MAX_CHARS` | | 模型摘要长度上限（默认 4000） |
| `HOVERSTARE_MAX_ROUNDS` | | 单次 run 模型调用轮数上限（0 = 由工具预算推导） |
| `PORT` | | 监听端口（默认 8080） |
| `HOVERSTARE_SERVE_MAX_JOBS` | | 并发任务上限（默认 4） |

## 端点

- `POST /webhook` — GitHub 事件入口（验签后异步处理）
- `GET /healthz` — `ok`（探活）

## 注意

- 仓库根目录放 `.github/hoverstare.toml` 可自定义审查配置（同 Action 模式）
- 服务本身无状态；审查状态全部存在 GitHub 侧（评论标记 + 元数据）
- 日志不会输出 token / private key / webhook secret
