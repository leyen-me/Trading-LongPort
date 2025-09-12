# Trading LongPort 🚀

> 一个基于 Rust 的高性能自动化交易服务，通过 **TradingView Webhook** 驱动 **LongPort 券商账户**执行做多 / 做空策略。

本项目使用 **Rust + Axum + LongPort OpenAPI SDK** 构建，实现从 TradingView 发出信号到 LongPort 账户自动下单的完整链路。支持港美股市场，适用于趋势跟踪、均值回归等量化策略的实盘部署。

🔧 支持：  
✅ 开仓（做多 / 做空）  
✅ 平仓（多头 / 空头）  
✅ 智能仓位管理  
✅ 多轮重试与订单取消机制  
✅ 异步非阻塞架构 & 完整日志追踪

---

## 📊 核心功能

| 功能 | 说明 |
|------|------|
| ✅ Webhook 接收 | 接收来自 TradingView 的交易信号 |
| ✅ 自动交易 | 解析信号后自动在 LongPort 账户下单 |
| ✅ 支持多种信号组合 | `buy+long`, `sell+short`, `buy/sell+flat`（平仓） |
| ✅ 限价单交易 | 使用当前盘口买一/卖一价提交 `LO` 订单 |
| ✅ 智能仓位控制 | 可配置买入/卖出比例（默认半仓） |
| ✅ 融资融券支持 | 支持现金账户与融资账户模式 |
| ✅ 高可靠性 | 失败自动重试 + 未成交订单自动取消 |
| ✅ 异步后台任务 | 下单任务异步执行，不阻塞主请求 |
| ✅ 日志可观测性 | 使用 `tracing` 输出详细运行日志 |

---

## 📡 Webhook 信号协议

TradingView 发送的 JSON 示例：

```json
{
  "action": "buy",
  "sentiment": "long",
  "ticker": "AAPL"
}
```

### 字段说明

| 字段       | 可选值                     | 含义                         |
|------------|----------------------------|------------------------------|
| `action`   | `"buy"`, `"sell"`          | 交易动作                     |
| `sentiment`| `"long"`, `"short"`, `"flat"` | 市场观点（决定开仓或平仓方向） |
| `ticker`   | 如 `"AAPL"`                | 股票代码（自动转为 `.US`）     |

### 信号行为映射表

| action | sentiment | 行为描述               |
|--------|-----------|------------------------|
| buy    | long      | 做多（买入股票）         |
| sell   | short     | 做空（卖出股票）         |
| buy    | flat      | 平空仓（买回归还）       |
| sell   | flat      | 平多仓（卖出持仓）       |
| 其他   | —         | 忽略，返回成功但无操作   |

---

## ⚙️ 环境配置

### 1. LongPort API 凭据

前往 [LongPort OpenAPI 平台](https://open.longportapp.com/) 创建应用，获取以下密钥：

```env
LONGPORT_APP_KEY=your_app_key
LONGPORT_APP_SECRET=your_app_secret
LONGPORT_ACCESS_TOKEN=your_access_token
```

> 🔐 注意：这些是敏感信息，请勿提交至版本控制。

---

### 2. 可选环境变量

| 环境变量               | 默认值   | 说明                                     |
|------------------------|----------|------------------------------------------|
| `MAX_PURCHASE_RATIO`   | `0.5`    | 每次买入最多使用可用资金/额度的百分比     |
| `MAX_SELL_RATIO`       | `0.5`    | 每次平仓卖出当前持仓的百分比（支持分批）  |
| `TRADING_MODE`         | `cash`   | 交易模式：`cash`（现金）或 `margin`（融资） |
| `RETRY_COUNT`           | `5`      | 下单失败最大重试次数                     |
| `RETRY_DELAY_SECS`      | `10`     | 每次重试间隔时间（秒）                   |
| `ORDER_WAIT_SECS`       | `30`     | 提交订单后等待几秒再检查状态             |

---

## 🧰 技术栈

- **语言**: [Rust](https://www.rust-lang.org/) (async/await)
- **Web 框架**: [Axum](https://github.com/tokio-rs/axum)
- **运行时**: [Tokio](https://tokio.rs/)
- **券商接口**: [`longport`](https://crates.io/crates/longport) SDK
- **日志系统**: `tracing` + `tracing-subscriber`
- **错误处理**: `thiserror` + 自定义 `IntoResponse`
- **并发模型**: `Arc<Mutex<T>>` 共享状态 + `tokio::spawn` 异步任务
- **CORS**: 使用 `tower-http` 配置安全跨域策略

---

## 🚀 快速启动

### 1. 安装依赖

```bash
cargo build
```

### 2. 配置环境变量

创建 `.env` 文件或在 shell 中导出：

```bash
export LONGPORT_APP_KEY="xxx"
export LONGPORT_APP_SECRET="xxx"
export LONGPORT_ACCESS_TOKEN="xxx"

# 可选
export MAX_PURCHASE_RATIO="0.8"
export MAX_SELL_RATIO="1.0"
export TRADING_MODE="margin"
```

### 3. 运行服务

```bash
cargo run
```

服务将启动在：`http://0.0.0.0:8080`

---

## 🌐 API 接口

### `POST /webhook` – 接收交易信号

接收 TradingView Webhook 请求并触发交易逻辑。

```bash
curl -X POST http://localhost:8080/webhook \
  -H "Content-Type: application/json" \
  -d '{
    "action": "buy",
    "sentiment": "long",
    "ticker": "TSLA"
  }'
```

**响应示例**：
```json
{ "status": "success" }
```

> ⚠️ 若信号无效或处理失败，会返回对应错误码和消息。

---

### `POST /webhook_test` – 测试连通性

仅用于测试 Webhook 是否可达，**不会触发任何交易**。

```bash
curl -X POST http://localhost:8080/webhook_test \
  -H "Content-Type: application/json" \
  -d '{"test": "data"}'
```

---

### `GET /health` – 健康检查

用于负载均衡或监控探测。

```bash
curl http://localhost:8080/health
```

输出：
```
OK
```

---

## 🔐 安全部署建议

为防止未授权访问，请务必采取以下措施：

- ✅ 使用 **HTTPS**（推荐 Nginx / Caddy 反向代理）
- ✅ 添加 **Token 验证**（例如在 URL 中加入 `?token=xxx` 并校验）
- ✅ 设置 **IP 白名单**（[TradingView Webhook IP 列表](https://www.tradingview.com/support/solutions/43000529369-webhook-url/)）
- ✅ 使用 `tower-http` 的 `RequireAuthorizationLayer` 添加 Bearer Token 认证
- ✅ 将服务部署在私有网络内，避免公网暴露

---

## 🤝 贡献与反馈

欢迎提交 Issue 或 Pull Request！

如有疑问或合作意向，可通过 Issues 或邮件联系作者。

---

## 📄 许可证

MIT License

---

> Built with ❤️ by Rustaceans for Traders.