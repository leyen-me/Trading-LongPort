# Trading LongPort 🚀

> 一个基于 Rust 的高性能自动化交易服务，通过 TradingView Webhook 驱动 LongPort 账户执行做多 / 做空 ETF 策略。

本项目使用 **Rust + Axum + LongPort OpenAPI SDK** 构建，实现从 TradingView 发出信号到 LongPort 券商自动下单的完整链路。支持对冲式 ETF 交易（如 TSLL.US 做多，TSLQ.US 做空），适用于趋势跟踪、均值回归等量化策略的实盘部署。

---

## 🔍 核心功能

✅ 接收 TradingView Webhook 信号并解析  
✅ 自动在 LongPort 账户中开仓（买入 TSLL.US 或 TSLQ.US）或平仓  
✅ 支持“做多”、“做空”、“平仓”三种信号组合  
✅ 使用限价单（LO）基于实时盘口价格下单（ask/bid）  
✅ 智能仓位管理：按账户可买数量的百分比下单（可配置）  
✅ 卖出任务支持多轮重试与自动取消未成交订单  
✅ 异步非阻塞架构，高可靠性与可观测性（tracing 日志）  
✅ CORS 安全防护 + 健康检查端点  

---

## 📡 信号协议（Webhook Payload）

TradingView 发送的 JSON 示例：

```json
{
  "action": "buy",
  "sentiment": "long"
}

| 字段       | 可选值                     | 含义                         |
|------------|----------------------------|------------------------------|
| `action`   | `"buy"`, `"sell"`          | 动作类型                     |
| `sentiment`| `"long"`, `"short"`, `"flat"` | 市场观点（决定交易方向）     |

### 信号组合解释：

| action | sentiment | 行为                             |
|--------|-----------|----------------------------------|
| buy    | long      | 买入 TSLL.US（做多）             |
| sell   | short     | 买入 TSLQ.US（等效做空）         |
| *      | flat      | 平掉 TSLL.US 或 TSLQ.US 所有持仓 |
| 其他   | —         | 忽略                             |

## ⚙️ 环境依赖与配置

### 1. LongPort API 凭据

你需要在 [LongPort OpenAPI 平台](https://open.longportapp.com/) 创建应用，获取以下信息：

```env
LONGPORT_APP_KEY=your_app_key
LONGPORT_APP_SECRET=your_app_secret
LONGPORT_ACCESS_TOKEN=your_access_token
```
---

### 2. 可选配置项

| 环境变量               | 默认值   | 说明                                     |
|------------------------|----------|------------------------------------------|
| `MAX_PURCHASE_RATIO`   | `0.5`    | 每次买入最多使用可用资金的百分比         |
| `MAX_SELL_RATIO`       | `0.5`    | 每次平仓卖出当前持仓的百分比（用于分批） |
| `RETRY_COUNT`           | `5`      | 卖出失败最大重试次数                     |
| `RETRY_DELAY_SECS`      | `10`     | 重试间隔（秒）                           |
| `ORDER_WAIT_SECS`       | `30`     | 下单后等待几秒再检查状态或取消           |

---

## 🧰 技术栈

- **语言**: Rust (async/await)
- **Web 框架**: [Axum](https://github.com/tokio-rs/axum)
- **HTTP Server**: Tokio + TcpListener
- **券商接口**: [longport](https://crates.io/crates/longport) SDK
- **日志系统**: `tracing` + `tracing-subscriber`
- **错误处理**: `thiserror`, 自定义 `IntoResponse`
- **并发模型**: `Arc<Mutex<T>>` 共享状态 + `tokio::spawn` 异步任务

---

## 🚀 快速启动

### 1. 安装依赖

```bash
cargo build
```

### 2. 配置环境变量

```env
LONGPORT_APP_KEY=xxx
LONGPORT_APP_SECRET=xxx
LONGPORT_ACCESS_TOKEN=xxx

# 可选
MAX_PURCHASE_RATIO=0.5
MAX_SELL_RATIO=0.5
```

### 3. 运行服务

```bash
cargo run
```

默认监听：`http://0.0.0.0:8080`

---

## 🌐 API 接口

### POST `/webhook`
接收 TradingView Webhook 请求。

示例请求：
```bash
curl -X POST http://localhost:8080/webhook \
  -H "Content-Type: application/json" \
  -d '{"action": "buy", "sentiment": "long"}'
```

响应：
```json
{ "status": "success" }
```

---

### POST `/webhook_test`
用于测试 Webhook 是否可达（不触发交易）。

---

### GET `/health`
健康检查接口，返回 `"OK"`。

```bash
curl http://localhost:8080/health
```

---

## 🛡 安全部署建议

- 使用 **HTTPS + Nginx / Caddy** 反向代理
- 添加 **Token 验证**（如在 query 中加 `?token=xxx`，并在 handler 中校验）
- 限制 IP（TradingView IP 白名单）
- 使用 `tower-http` 的 `RequireAuthorizationLayer` 增强安全性

---

## 📊 当前支持的 ETF

| Symbol     | 名称                     | 类型     |
|------------|--------------------------|----------|
| `TSLL.US`  | Direxion Tesla Bull 2X | 做多杠杆 |
| `TSLQ.US`  | Direxion Tesla Bear 2X | 做空杠杆 |

> 🔁 你可以在代码中修改 `SYMBOL_LONG` / `SYMBOL_SHORT` 替换为其他标的（如 SQQQ/TQQQ、SPXU/SPXL 等）