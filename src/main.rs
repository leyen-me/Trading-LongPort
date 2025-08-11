use axum::http;
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get, routing::post};
use longport::Decimal;
use longport::{
    Config, decimal,
    quote::QuoteContext,
    trade::{
        EstimateMaxPurchaseQuantityOptions, OrderSide, OrderStatus, OrderType, OutsideRTH,
        StockPosition, SubmitOrderOptions, TimeInForceType, TradeContext,
    },
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use tracing_subscriber::FmtSubscriber;

/// ==================== 配置常量 ====================
const DO_LONG_SYMBOL: &str = "TSLL.US";
const DO_SHORT_SYMBOL: &str = "TSDD.US";

/// ==================== 枚举定义 ====================
#[derive(Debug, Clone, PartialEq)]
enum Action {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq)]
enum Sentiment {
    Long,
    Short,
    Flat,
}

/// ==================== Webhook 请求结构 ====================
#[derive(Deserialize, Debug)]
struct WebhookPayload {
    action: String,
    sentiment: String,
}

#[derive(Serialize, Debug)]
struct ApiResponse {
    status: &'static str,
    message: Option<String>,
}

/// ==================== 共享状态（交易上下文）====================
struct AppState {
    quote_ctx: Arc<QuoteContext>,
    trade_ctx: Arc<TradeContext>,
}

/// ==================== 错误类型 ====================
#[derive(thiserror::Error, Debug)]
enum TradingError {
    #[error("解析信号失败: {0}")]
    ParseError(String),
    #[error("获取盘口失败: {0}")]
    QuoteError(String),
    #[error("交易执行失败: {0}")]
    TradeError(String),
    #[error("网络或SDK错误: {0}")]
    SDKError(String),
}

impl IntoResponse for TradingError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match &self {
            TradingError::ParseError(msg) => (StatusCode::BAD_REQUEST, msg),
            TradingError::QuoteError(msg) => (StatusCode::BAD_REQUEST, msg),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, &self.to_string()),
        };
        (
            status,
            Json(ApiResponse {
                status: "error",
                message: Some(message.to_string()),
            }),
        )
            .into_response()
    }
}

/// 判断订单是否仍有效且可撤单
fn is_order_active_and_cancellable(status: &OrderStatus) -> bool {
    matches!(
        status,
        OrderStatus::WaitToNew // 等待报单
            | OrderStatus::New // 已报单未成交
            | OrderStatus::WaitToReplace // 等待改单
            | OrderStatus::PendingReplace // 改单中
            | OrderStatus::PartialFilled // 部分成交
    )
}

/// 判断订单是否已进入终态（无需处理）
fn is_order_terminal(status: &OrderStatus) -> bool {
    matches!(
        status,
        OrderStatus::Filled // 完全成交
            | OrderStatus::Rejected // 被拒
            | OrderStatus::Canceled // 已取消
            | OrderStatus::Expired // 已过期
            | OrderStatus::Replaced // 已改单
            | OrderStatus::PartialWithdrawal // 部分撤回
            | OrderStatus::WaitToCancel // 撤销上报中
            | OrderStatus::PendingCancel // 撤销中
    )
}

/// ==================== 工具函数 ====================

async fn get_ask_price(quote_ctx: &QuoteContext, symbol: &str) -> Option<Decimal> {
    quote_ctx
        .depth(symbol)
        .await
        .ok()
        .and_then(|depth| depth.asks.first()?.price)
}

async fn get_bid_price(quote_ctx: &QuoteContext, symbol: &str) -> Option<Decimal> {
    quote_ctx
        .depth(symbol)
        .await
        .ok()
        .and_then(|depth| depth.bids.first()?.price)
}

/// 构建标准化的限价单
fn build_order(
    symbol: &str,
    side: OrderSide,
    quantity: Decimal,
    price: Decimal,
    time_in_force: TimeInForceType,
    remark: Option<&str>,
) -> SubmitOrderOptions {
    let mut order = SubmitOrderOptions::new(symbol, OrderType::LO, side, quantity, time_in_force)
        .submitted_price(price)
        .outside_rth(OutsideRTH::AnyTime);

    if let Some(remark) = remark {
        order = order.remark(remark);
    }

    order
}

/// 带重试机制的安全卖出（主交易接口）
async fn sell_with_retry(
    trade_ctx: &TradeContext,
    quote_ctx: &QuoteContext,
    symbol: &str,
    target_quantity: Decimal,
    max_retries: usize,
    retry_delay: Duration,
) -> Result<(), TradingError> {
    let mut remaining = target_quantity;
    let mut attempt = 0;

    info!(
        symbol,
        quantity = %target_quantity.to_string(),
        "开始重试卖出流程，最多 {} 次",
        max_retries
    );

    while remaining >= decimal!(1) && attempt < max_retries {
        attempt += 1;

        info!(
            symbol,
            attempt,
            remaining = %remaining.to_string(),
            "第 {} 次尝试卖出",
            attempt
        );

        // Step 1: 获取最新买一价
        let current_price = get_bid_price(quote_ctx, symbol)
            .await
            .ok_or(TradingError::QuoteError("无法获取买一价".to_string()))?;

        // Step 2: 构建订单
        let order_opts = build_order(
            symbol,
            OrderSide::Sell,
            remaining,
            current_price,
            TimeInForceType::Day,
            Some(if symbol == DO_LONG_SYMBOL {
                "多头平仓"
            } else {
                "空头平仓"
            }),
        );

        // Step 3: 提交订单
        let resp = match trade_ctx.submit_order(order_opts).await {
            Ok(resp) => {
                info!(order_id = %resp.order_id, "卖出订单已提交");
                resp
            }
            Err(e) => {
                warn!(symbol, error = %e, "提交订单失败，等待重试");
                sleep(retry_delay).await;
                continue;
            }
        };

        let order_id = resp.order_id;

        // Step 4: 等待 30 秒观察成交情况
        sleep(Duration::from_secs(30)).await;

        // Step 5: 查询订单状态
        let order = match trade_ctx.order_detail(&order_id).await {
            Ok(o) => o,
            Err(e) => {
                warn!(order_id = %order_id, error = %e, "查询订单状态失败");
                sleep(retry_delay).await;
                continue;
            }
        };

        let filled = order.executed_quantity;
        remaining = (remaining - filled).max(decimal!(0));

        match order.status {
            OrderStatus::Filled => {
                info!(order_id = %order_id, "订单完全成交");
            }
            OrderStatus::PartialFilled if remaining < decimal!(1) => {
                info!(order_id = %order_id, filled = %filled.to_string(), "部分成交，剩余不足1股，放弃");
            }
            OrderStatus::PartialFilled => {
                info!(
                    order_id = %order_id,
                    filled = %filled.to_string(),
                    remaining = %remaining.to_string(),
                    "部分成交，继续重试剩余数量"
                );
            }
            status if is_order_terminal(&status) => {
                warn!(order_id = %order_id, ?status, "订单终态但未完全成交");
            }
            _ => {
                // 未成交，尝试撤单
                if is_order_active_and_cancellable(&order.status) {
                    info!(order_id = %order_id, "撤单未成交订单");
                    let _ = trade_ctx.cancel_order(&order_id).await;
                }
            }
        }

        // 重试间隔
        sleep(retry_delay).await;
    }

    // 最终检查
    if remaining >= decimal!(1) {
        return Err(TradingError::TradeError(format!(
            "卖出失败：{} 次重试后仍剩余 {} 股未卖出",
            max_retries, remaining
        )));
    }

    info!(symbol, "卖出完成");
    Ok(())
}

/// 执行买入
async fn buy(
    trade_ctx: &TradeContext,
    quote_ctx: &QuoteContext,
    symbol: &str,
) -> Result<(), TradingError> {
    let current_price = get_ask_price(quote_ctx, symbol)
        .await
        .ok_or(TradingError::QuoteError("无法获取卖一价".to_string()))?;

    let price_decimal = current_price;

    let estimate_opts =
        EstimateMaxPurchaseQuantityOptions::new(symbol, OrderType::LO, OrderSide::Buy)
            .price(price_decimal);

    let max_buy_resp = trade_ctx
        .estimate_max_purchase_quantity(estimate_opts)
        .await
        .map_err(|e| TradingError::SDKError(e.to_string()))?;

    let quantity = (max_buy_resp.cash_max_qty * decimal!(0.9)).trunc();

    if quantity < decimal!(1) {
        warn!(symbol, "可买数量不足（{}），取消买入", quantity);
        return Ok(());
    }

    info!(
        symbol,
        price = %current_price.to_string(),
        quantity = %quantity.to_string(),
        "估算最大买入量，准备下单"
    );

    let order_opts = build_order(
        symbol,
        OrderSide::Buy,
        quantity,
        current_price,
        TimeInForceType::Day,
        Some(if symbol == DO_LONG_SYMBOL {
            "多头买入"
        } else {
            "空头买入"
        }),
    );

    let resp = trade_ctx
        .submit_order(order_opts)
        .await
        .map_err(|e| TradingError::TradeError(format!("买入失败: {}", e)))?;

    let order_id = resp.order_id.clone();
    info!(order_id = %order_id, "买入订单已提交");

    // 🔥 启动后台任务：30 秒后检查是否成交，未成交则撤单
    let trade_ctx_for_task: TradeContext = trade_ctx.clone();
    tokio::spawn(async move {
        // 等待 30 秒
        sleep(Duration::from_secs(30)).await;
        // 查询订单最新状态
        match trade_ctx_for_task.order_detail(&order_id).await {
            Ok(order) => {
                if is_order_terminal(&order.status) {
                    debug!(order_id = %order_id, status = ?order.status, "订单已是终态，跳过撤单");
                    return;
                }
                if is_order_active_and_cancellable(&order.status) {
                    info!(order_id = %order_id, status = ?order.status, "订单未完成，正在撤单");
                    if let Err(e) = trade_ctx_for_task.cancel_order(&order_id).await {
                        error!(order_id = %order_id, "撤单失败: {}", e);
                    } else {
                        info!(order_id = %order_id, "撤单成功");
                    }
                } else {
                    warn!(order_id = %order_id, status = ?order.status, "订单状态异常，无法处理");
                }
            }
            Err(e) => {
                error!(order_id = %order_id, "查询订单状态失败: {}", e);
            }
        }
    });

    Ok(())
}

/// 执行卖出
async fn sell(
    trade_ctx: &TradeContext,
    quote_ctx: &QuoteContext,
    symbol: &str,
    quantity: Decimal,
) -> Result<(), TradingError> {
    sell_with_retry(
        trade_ctx,
        quote_ctx,
        symbol,
        quantity,
        5, // 最多重试 5 次
        Duration::from_secs(10),
    )
    .await
}

/// 执行做多
async fn do_long(state: &Arc<Mutex<AppState>>) -> Result<(), TradingError> {
    let state = state.lock().await;
    info!("执行做多操作");
    buy(&state.trade_ctx, &state.quote_ctx, DO_LONG_SYMBOL).await
}

/// 执行做空
async fn do_short(state: &Arc<Mutex<AppState>>) -> Result<(), TradingError> {
    let state = state.lock().await;
    info!("执行做空操作");
    buy(&state.trade_ctx, &state.quote_ctx, DO_SHORT_SYMBOL).await
}

/// 执行平仓
async fn do_close_position(state: &Arc<Mutex<AppState>>) -> Result<(), TradingError> {
    let state = state.lock().await;
    info!("开始执行平仓流程");

    let positions_resp = state
        .trade_ctx
        .stock_positions(None)
        .await
        .map_err(|e| TradingError::SDKError(e.to_string()))?;

    for channel in positions_resp.channels {
        for position in channel.positions {
            if ![DO_LONG_SYMBOL, DO_SHORT_SYMBOL].contains(&position.symbol.as_str()) {
                debug!(symbol = %position.symbol, "非目标标的，忽略");
                continue;
            }

            match handle_position(&state.trade_ctx, &state.quote_ctx, position).await {
                Ok(_) => {}
                Err(e) => error!(error = %e, "平仓单执行失败"),
            }
        }
    }

    Ok(())
}

/// 处理单个持仓
async fn handle_position(
    trade_ctx: &TradeContext,
    quote_ctx: &QuoteContext,
    position: StockPosition,
) -> Result<(), TradingError> {
    let symbol = &position.symbol;
    let qty = position.quantity;

    if qty < decimal!(1) {
        warn!(%symbol, quantity = %qty.to_string(), "剩余碎股，不平仓");
        return Ok(());
    }

    if qty <= decimal!(0) {
        info!(%symbol, "无持仓");
        return Ok(());
    }

    info!(%symbol, quantity = %qty.to_string(), "持仓数量 > 0，准备卖出");
    sell(trade_ctx, quote_ctx, symbol, qty).await
}

/// ==================== Webhook 路由 ====================

/// 主要 Webhook 处理器
async fn webhook_handler(
    state: axum::extract::State<Arc<Mutex<AppState>>>,
    Json(payload): Json<WebhookPayload>,
) -> Result<Json<ApiResponse>, TradingError> {
    info!("收到 TradingView 信号: {:?}", payload);

    let action = parse_action(&payload.action)?;
    let sentiment = parse_sentiment(&payload.sentiment)?;

    info!(?action, ?sentiment, "交易信号解析完成");

    match (&action, &sentiment) {
        (Action::Buy, Sentiment::Long) => {
            info!("信号=开多仓");
            do_long(&state).await?;
        }
        (Action::Sell, Sentiment::Short) => {
            info!("信号=开空仓");
            do_short(&state).await?;
        }
        (_, Sentiment::Flat) => {
            info!("信号=平仓");
            do_close_position(&state).await?;
        }
        _ => {
            warn!(?action, ?sentiment, "未识别的交易信号组合");
            return Ok(Json(ApiResponse {
                status: "success",
                message: Some("unknown signal, ignored".to_string()),
            }));
        }
    }

    Ok(Json(ApiResponse {
        status: "success",
        message: None,
    }))
}

/// 测试用端点
async fn webhook_test_handler(Json(payload): Json<serde_json::Value>) -> impl IntoResponse {
    info!("收到测试信号: {:?}", payload);
    (
        StatusCode::OK,
        Json(ApiResponse {
            status: "success",
            message: None,
        }),
    )
}

/// 解析 Action
fn parse_action(s: &str) -> Result<Action, TradingError> {
    match s.to_lowercase().as_str() {
        "buy" => Ok(Action::Buy),
        "sell" => Ok(Action::Sell),
        _ => Err(TradingError::ParseError(format!("无效 action: {}", s))),
    }
}

/// 解析 Sentiment
fn parse_sentiment(s: &str) -> Result<Sentiment, TradingError> {
    match s.to_lowercase().as_str() {
        "long" => Ok(Sentiment::Long),
        "short" => Ok(Sentiment::Short),
        "flat" => Ok(Sentiment::Flat),
        _ => Err(TradingError::ParseError(format!("无效 sentiment: {}", s))),
    }
}

/// ==================== 主函数 ====================
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 初始化日志
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("启动中...");

    // 加载配置
    let config = Arc::new(Config::from_env()?);

    let (quote_ctx, _) = QuoteContext::try_new(config.clone()).await?;
    let (trade_ctx, _) = TradeContext::try_new(config).await?;

    let app_state = Arc::new(Mutex::new(AppState {
        quote_ctx: Arc::new(quote_ctx),
        trade_ctx: Arc::new(trade_ctx),
    }));

    // 构建路由
    let app = Router::new()
        .route("/webhook", post(webhook_handler))
        .route("/webhook_test", post(webhook_test_handler))
        .route("/health", get(|| async { "OK" }))
        .with_state(app_state)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_methods([http::Method::POST])
                .allow_headers([http::header::CONTENT_TYPE]),
        );

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    info!("服务启动成功，监听地址: {}", addr);

    // Create TcpListener
    let listener = TcpListener::bind(&addr).await.unwrap();
    // Start server
    axum::serve(listener, app).await.unwrap();

    Ok(())
}
