use axum::http;
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get, routing::post};
use longport::Decimal;
use longport::{
    Config, decimal,
    quote::QuoteContext,
    trade::{
        EstimateMaxPurchaseQuantityOptions, OrderSide, OrderStatus, OrderType, OutsideRTH,
        SubmitOrderOptions, TimeInForceType, TradeContext,
    },
};
use serde::{Deserialize, Serialize};
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};
use tracing_subscriber::FmtSubscriber;

/// ==================== Constants ====================
const DEFAULT_PURCHASE_RATIO: f64 = 0.5; // 每次都半仓买入
const DEFAULT_SELL_RATIO: f64 = 0.5; // 每次都半仓卖出
const RETRY_COUNT: usize = 5;
const RETRY_DELAY_SECS: u64 = 10;
const ORDER_WAIT_SECS: u64 = 30;

/// ==================== Enums ====================
#[derive(Debug, Clone, PartialEq)]
enum TradeAction {
    Buy,
    Sell,
}

#[derive(Debug, Clone, PartialEq)]
enum MarketSentiment {
    Long,
    Short,
    Flat,
}

/// ==================== Webhook Payload ====================
#[derive(Deserialize, Debug)]
struct WebhookRequest {
    action: String,
    sentiment: String,
    ticker: String,
}

#[derive(Serialize, Debug)]
struct WebApiResponse {
    status: &'static str,
    message: Option<String>,
}

/// ==================== Shared State ====================
struct AppState {
    quote_ctx: Arc<QuoteContext>,
    trade_ctx: Arc<TradeContext>,
}

/// ==================== Error Handling ====================
#[derive(thiserror::Error, Debug)]
enum TradingError {
    #[error("Signal parsing error: {0}")]
    ParseError(String),
    #[error("Quote retrieval error: {0}")]
    QuoteError(String),
    #[error("SDK or network error: {0}")]
    SdkError(String),
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
            Json(WebApiResponse {
                status: "error",
                message: Some(message.to_string()),
            }),
        )
            .into_response()
    }
}

/// ==================== Order Status Utilities ====================
fn is_order_active_and_cancellable(status: &OrderStatus) -> bool {
    matches!(
        status,
        OrderStatus::WaitToNew
            | OrderStatus::New
            | OrderStatus::WaitToReplace
            | OrderStatus::PendingReplace
            | OrderStatus::PartialFilled
    )
}

fn is_order_terminal(status: &OrderStatus) -> bool {
    matches!(
        status,
        OrderStatus::Filled
            | OrderStatus::Rejected
            | OrderStatus::Canceled
            | OrderStatus::Expired
            | OrderStatus::Replaced
            | OrderStatus::PartialWithdrawal
            | OrderStatus::WaitToCancel
            | OrderStatus::PendingCancel
    )
}

/// ==================== Order Builders ====================
fn build_limit_order(
    symbol: &str,
    side: OrderSide,
    quantity: Decimal,
    price: Decimal,
) -> SubmitOrderOptions {
    let order = SubmitOrderOptions::new(
        symbol,
        OrderType::LO,
        side,
        quantity,
        TimeInForceType::GoodTilCanceled,
    )
    .submitted_price(price)
    .outside_rth(OutsideRTH::AnyTime);
    order
}

/// ==================== Quote Utilities ====================
async fn get_ask_price(ctx: &QuoteContext, symbol: &str) -> Option<Decimal> {
    ctx.depth(symbol)
        .await
        .ok()
        .and_then(|depth| depth.asks.first()?.price)
}

async fn get_bid_price(ctx: &QuoteContext, symbol: &str) -> Option<Decimal> {
    ctx.depth(symbol)
        .await
        .ok()
        .and_then(|depth| depth.bids.first()?.price)
}

/// ==================== Buy Task (Background) ====================
async fn buy_background_task(
    trade_ctx: Arc<TradeContext>,
    quote_ctx: Arc<QuoteContext>,
    symbol: String,
    target_quantity: Decimal,
    max_retries: usize,
    retry_delay: Duration,
) {
    let mut remaining = target_quantity;
    let mut attempt = 0;

    info!(
        symbol,
        quantity = %target_quantity,
        "启动买入后台任务，最大重试次数：{}",
        max_retries
    );

    while remaining >= decimal!(1) && attempt < max_retries {
        attempt += 1;
        info!(symbol, attempt, remaining = %remaining, "第 {} 次尝试买入", attempt);

        let price = match get_ask_price(&quote_ctx, &symbol).await {
            Some(p) => p,
            None => {
                warn!(symbol, "获取卖一价失败，正在重试...");
                sleep(retry_delay).await;
                continue;
            }
        };

        let order = build_limit_order(&symbol, OrderSide::Buy, remaining, price);

        let order_id = match trade_ctx.submit_order(order).await {
            Ok(resp) => resp.order_id,
            Err(e) => {
                warn!(symbol, error = %e, "提交买入订单失败");
                sleep(retry_delay).await;
                continue;
            }
        };

        info!(order_id = %order_id, "买入订单已提交");

        sleep(Duration::from_secs(ORDER_WAIT_SECS)).await;

        let order_detail = match trade_ctx.order_detail(&order_id).await {
            Ok(detail) => detail,
            Err(e) => {
                warn!(order_id = %order_id, error = %e, "获取订单详情失败");
                sleep(retry_delay).await;
                continue;
            }
        };

        let filled = order_detail.executed_quantity;
        remaining = (remaining - filled).max(decimal!(0));

        match order_detail.status {
            OrderStatus::Filled => info!(order_id = %order_id, "订单已全部成交"),
            OrderStatus::PartialFilled if remaining < decimal!(1) => {
                info!(order_id = %order_id, filled = %filled, "部分成交，剩余不足1单位，停止重试");
            }
            OrderStatus::PartialFilled => {
                info!(order_id = %order_id, filled = %filled, remaining = %remaining, "部分成交，继续尝试");
            }
            status if is_order_terminal(&status) => {
                warn!(order_id = %order_id, ?status, "订单已进入终态，不再重试");
            }
            _ => {
                if is_order_active_and_cancellable(&order_detail.status) {
                    info!(order_id = %order_id, "正在取消未成交订单");
                    let _ = trade_ctx.cancel_order(&order_id).await;
                }
            }
        }

        sleep(retry_delay).await;
    }

    if remaining >= decimal!(1) {
        error!(symbol, remaining = %remaining, "买入任务在多次重试后仍失败");
    } else {
        info!(symbol, "买入任务执行完成");
    }
}

/// ==================== Sell Task (Background) ====================
async fn sell_background_task(
    trade_ctx: Arc<TradeContext>,
    quote_ctx: Arc<QuoteContext>,
    symbol: String,
    target_quantity: Decimal,
    max_retries: usize,
    retry_delay: Duration,
) {
    let mut remaining = target_quantity;
    let mut attempt = 0;

    info!(
        symbol,
        quantity = %target_quantity,
        "启动卖出后台任务，最大重试次数：{}",
        max_retries
    );

    while remaining >= decimal!(1) && attempt < max_retries {
        attempt += 1;
        info!(symbol, attempt, remaining = %remaining, "第 {} 次尝试卖出", attempt);

        let price = match get_bid_price(&quote_ctx, &symbol).await {
            Some(p) => p,
            None => {
                warn!(symbol, "获取买一价失败，正在重试...");
                sleep(retry_delay).await;
                continue;
            }
        };

        let order = build_limit_order(&symbol, OrderSide::Sell, remaining, price);

        let order_id = match trade_ctx.submit_order(order).await {
            Ok(resp) => resp.order_id,
            Err(e) => {
                warn!(symbol, error = %e, "提交卖出订单失败");
                sleep(retry_delay).await;
                continue;
            }
        };

        info!(order_id = %order_id, "卖出订单已提交");

        sleep(Duration::from_secs(ORDER_WAIT_SECS)).await;

        let order_detail = match trade_ctx.order_detail(&order_id).await {
            Ok(detail) => detail,
            Err(e) => {
                warn!(order_id = %order_id, error = %e, "获取订单详情失败");
                sleep(retry_delay).await;
                continue;
            }
        };

        let filled = order_detail.executed_quantity;
        remaining = (remaining - filled).max(decimal!(0));

        match order_detail.status {
            OrderStatus::Filled => info!(order_id = %order_id, "订单已全部成交"),
            OrderStatus::PartialFilled if remaining < decimal!(1) => {
                info!(order_id = %order_id, filled = %filled, "部分成交，剩余不足1单位，停止重试");
            }
            OrderStatus::PartialFilled => {
                info!(order_id = %order_id, filled = %filled, remaining = %remaining, "部分成交，继续尝试");
            }
            status if is_order_terminal(&status) => {
                warn!(order_id = %order_id, ?status, "订单已进入终态，不再重试");
            }
            _ => {
                if is_order_active_and_cancellable(&order_detail.status) {
                    info!(order_id = %order_id, "正在取消未成交订单");
                    let _ = trade_ctx.cancel_order(&order_id).await;
                }
            }
        }

        sleep(retry_delay).await;
    }

    if remaining >= decimal!(1) {
        error!(symbol, remaining = %remaining, "卖出任务在多次重试后仍失败");
    } else {
        info!(symbol, "卖出任务执行完成");
    }
}

/// ==================== Trade Actions ====================
async fn do_long(state: &Arc<Mutex<AppState>>, symbol: &str) -> Result<(), TradingError> {
    let state = state.lock().await;

    let ratio: f64 = env::var("MAX_PURCHASE_RATIO")
        .unwrap_or_else(|_| DEFAULT_PURCHASE_RATIO.to_string())
        .parse()
        .unwrap_or(DEFAULT_PURCHASE_RATIO);

    let price = get_ask_price(&state.quote_ctx, symbol)
        .await
        .ok_or_else(|| TradingError::QuoteError("获取卖一价失败".to_string()))?;

    let opts =
        EstimateMaxPurchaseQuantityOptions::new(symbol, OrderType::LO, OrderSide::Buy).price(price);

    let estimate = &state
        .trade_ctx
        .estimate_max_purchase_quantity(opts)
        .await
        .map_err(|e| TradingError::SdkError(e.to_string()))?;

    let quantity = (estimate.cash_max_qty * decimal!(ratio)).trunc();

    if quantity < decimal!(1) {
        warn!(symbol, "买入数量不足");
        return Ok(());
    }

    let trade_ctx = Arc::clone(&state.trade_ctx);
    let quote_ctx = Arc::clone(&state.quote_ctx);
    // 启动后台买入任务
    tokio::spawn(buy_background_task(
        trade_ctx,
        quote_ctx,
        symbol.to_string(),
        quantity,
        RETRY_COUNT,
        Duration::from_secs(RETRY_DELAY_SECS),
    ));

    info!(symbol, quantity = %quantity, "做多任务已启动（后台运行）");
    Ok(())
}

async fn do_short(state: &Arc<Mutex<AppState>>, symbol: &str) -> Result<(), TradingError> {
    let state = state.lock().await;

    let ratio: f64 = env::var("MAX_PURCHASE_RATIO")
        .unwrap_or_else(|_| DEFAULT_PURCHASE_RATIO.to_string())
        .parse()
        .unwrap_or(DEFAULT_PURCHASE_RATIO);

    let price = get_bid_price(&state.quote_ctx, symbol)
        .await
        .ok_or_else(|| TradingError::QuoteError("获取买一价失败".to_string()))?;

    let opts = EstimateMaxPurchaseQuantityOptions::new(symbol, OrderType::LO, OrderSide::Sell)
        .price(price);

    let estimate = &state
        .trade_ctx
        .estimate_max_purchase_quantity(opts)
        .await
        .map_err(|e| TradingError::SdkError(e.to_string()))?;

    let quantity = (estimate.cash_max_qty * decimal!(ratio)).trunc();

    if quantity < decimal!(1) {
        warn!(symbol, "卖出数量不足");
        return Ok(());
    }

    let trade_ctx = Arc::clone(&state.trade_ctx);
    let quote_ctx = Arc::clone(&state.quote_ctx);
    // 启动后台买入任务
    tokio::spawn(sell_background_task(
        trade_ctx,
        quote_ctx,
        symbol.to_string(),
        quantity,
        RETRY_COUNT,
        Duration::from_secs(RETRY_DELAY_SECS),
    ));

    info!(symbol, quantity = %quantity, "做空任务已启动（后台运行）");
    Ok(())
}

async fn do_close(
    state: &Arc<Mutex<AppState>>,
    symbol: &str,
    action: &TradeAction,
) -> Result<(), TradingError> {
    let state = state.lock().await;

    let ratio: f64 = env::var("MAX_SELL_RATIO")
        .unwrap_or_else(|_| DEFAULT_SELL_RATIO.to_string())
        .parse()
        .unwrap_or(DEFAULT_SELL_RATIO);

    let resp = state
        .trade_ctx
        .stock_positions(None)
        .await
        .map_err(|e| TradingError::SdkError(e.to_string()))?;

    for channel in resp.channels {
        for pos in channel.positions {
            if ![symbol].contains(&pos.symbol.as_str()) {
                continue;
            }
            let target_quantity = (pos.quantity.abs() * decimal!(ratio)).trunc();
            let trade_ctx = Arc::clone(&state.trade_ctx);
            let quote_ctx = Arc::clone(&state.quote_ctx);
            match action {
                TradeAction::Buy => {
                    tokio::spawn(buy_background_task(
                        trade_ctx,
                        quote_ctx,
                        pos.symbol,
                        target_quantity,
                        RETRY_COUNT,
                        Duration::from_secs(RETRY_DELAY_SECS),
                    ));
                    info!(symbol = %symbol, quantity = %target_quantity, "平仓空头任务已启动（后台运行）");
                }
                TradeAction::Sell => {
                    tokio::spawn(sell_background_task(
                        trade_ctx,
                        quote_ctx,
                        pos.symbol,
                        target_quantity,
                        RETRY_COUNT,
                        Duration::from_secs(RETRY_DELAY_SECS),
                    ));
                    info!(symbol = %symbol, quantity = %target_quantity, "平仓多头任务已启动（后台运行）");
                }
            }
            break;
        }
    }

    Ok(())
}

/// ==================== Webhook Handlers ====================
///
/// {
///    "ticker": "{{ticker}}",
///    "time": "{{time}}",
///    "action": "{{strategy.order.action}}",
///    "sentiment": "{{strategy.market_position}}",
///    "price": "{{strategy.order.price}}"
/// }
///
async fn webhook_handler(
    state: axum::extract::State<Arc<Mutex<AppState>>>,
    Json(payload): Json<WebhookRequest>,
) -> Result<Json<WebApiResponse>, TradingError> {
    info!("收到来自 TradingView 的信号: {:?}", payload);

    let action = match payload.action.to_lowercase().as_str() {
        "buy" => TradeAction::Buy,
        "sell" => TradeAction::Sell,
        _ => return Err(TradingError::ParseError("无效的参数 action".to_string())),
    };

    let sentiment = match payload.sentiment.to_lowercase().as_str() {
        "long" => MarketSentiment::Long,
        "short" => MarketSentiment::Short,
        "flat" => MarketSentiment::Flat,
        _ => return Err(TradingError::ParseError("无效的参数 sentiment".to_string())),
    };

    let symbol = payload.ticker + ".US";

    match (&action, &sentiment) {
        (TradeAction::Buy, MarketSentiment::Long) => do_long(&state, &symbol).await?,
        (TradeAction::Sell, MarketSentiment::Short) => do_short(&state, &symbol).await?,
        (TradeAction::Buy, MarketSentiment::Flat) => do_close(&state, &symbol, &action).await?, // 做空平仓，需要买回
        (TradeAction::Sell, MarketSentiment::Flat) => do_close(&state, &symbol, &action).await?, // 做多平仓，需要卖出
        _ => {
            warn!(?action, ?sentiment, "未知的信号组合");
            return Ok(Json(WebApiResponse {
                status: "success",
                message: Some("未知的信号组合".to_string()),
            }));
        }
    }

    Ok(Json(WebApiResponse {
        status: "success",
        message: None,
    }))
}

async fn webhook_test_handler(Json(payload): Json<serde_json::Value>) -> impl IntoResponse {
    info!("Test webhook received: {:?}", payload);
    (
        StatusCode::OK,
        Json(WebApiResponse {
            status: "success",
            message: None,
        }),
    )
}

/// ==================== Main ====================
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("服务启动中...");

    let config = Arc::new(Config::from_env()?);
    let (quote_ctx, _) = QuoteContext::try_new(config.clone()).await?;
    let (trade_ctx, _) = TradeContext::try_new(config).await?;

    let state = Arc::new(Mutex::new(AppState {
        quote_ctx: Arc::new(quote_ctx),
        trade_ctx: Arc::new(trade_ctx),
    }));

    let app = Router::new()
        .route("/webhook", post(webhook_handler))
        .route("/webhook_test", post(webhook_test_handler))
        .route("/health", get(|| async { "OK" }))
        .with_state(state)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_methods([http::Method::POST])
                .allow_headers([http::header::CONTENT_TYPE]),
        );

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    info!("服务启动后地址: {}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
