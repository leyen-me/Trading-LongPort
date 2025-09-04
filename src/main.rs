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
use tracing::{debug, error, info, warn};
use tracing_subscriber::FmtSubscriber;

/// ==================== Constants ====================
const SYMBOL_LONG: &str = "TSLL.US";
const SYMBOL_SHORT: &str = "TSLQ.US";
const DEFAULT_PURCHASE_RATIO: f64 = 0.5; // 每次都半仓买入
const DEFAULT_SELL_RATIO: f64 = 0.5;     // 每次都半仓卖出
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
    remark: Option<&str>,
) -> SubmitOrderOptions {
    let mut order =
        SubmitOrderOptions::new(symbol, OrderType::LO, side, quantity, TimeInForceType::Day)
            .submitted_price(price)
            .outside_rth(OutsideRTH::AnyTime);

    if let Some(remark) = remark {
        order = order.remark(remark);
    }

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
        "Starting buy background task with max retries: {}",
        max_retries
    );

    while remaining >= decimal!(1) && attempt < max_retries {
        attempt += 1;
        info!(symbol, attempt, remaining = %remaining, "Attempt {} to buy", attempt);

        let price = match get_ask_price(&quote_ctx, &symbol).await {
            Some(p) => p,
            None => {
                warn!(symbol, "Failed to get ask price, retrying...");
                sleep(retry_delay).await;
                continue;
            }
        };

        let remark = if symbol == SYMBOL_LONG {
            "Open Long"
        } else {
            "Open Short"
        };

        let order = build_limit_order(&symbol, OrderSide::Buy, remaining, price, Some(remark));

        let order_id = match trade_ctx.submit_order(order).await {
            Ok(resp) => resp.order_id,
            Err(e) => {
                warn!(symbol, error = %e, "Submit buy order failed");
                sleep(retry_delay).await;
                continue;
            }
        };

        info!(order_id = %order_id, "Buy order submitted");

        sleep(Duration::from_secs(ORDER_WAIT_SECS)).await;

        let order_detail = match trade_ctx.order_detail(&order_id).await {
            Ok(detail) => detail,
            Err(e) => {
                warn!(order_id = %order_id, error = %e, "Failed to fetch order detail");
                sleep(retry_delay).await;
                continue;
            }
        };

        let filled = order_detail.executed_quantity;
        remaining = (remaining - filled).max(decimal!(0));

        match order_detail.status {
            OrderStatus::Filled => info!(order_id = %order_id, "Order fully filled"),
            OrderStatus::PartialFilled if remaining < decimal!(1) => {
                info!(order_id = %order_id, filled = %filled, "Partially filled, less than 1 unit, stop");
            }
            OrderStatus::PartialFilled => {
                info!(order_id = %order_id, filled = %filled, remaining = %remaining, "Partially filled, continue");
            }
            status if is_order_terminal(&status) => {
                warn!(order_id = %order_id, ?status, "Order in terminal state");
            }
            _ => {
                if is_order_active_and_cancellable(&order_detail.status) {
                    info!(order_id = %order_id, "Cancelling unfilled order");
                    let _ = trade_ctx.cancel_order(&order_id).await;
                }
            }
        }

        sleep(retry_delay).await;
    }

    if remaining >= decimal!(1) {
        error!(symbol, remaining = %remaining, "Buy task failed after retries");
    } else {
        info!(symbol, "Buy task completed successfully");
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
        "Starting sell background task with max retries: {}",
        max_retries
    );

    while remaining >= decimal!(1) && attempt < max_retries {
        attempt += 1;
        info!(symbol, attempt, remaining = %remaining, "Attempt {} to sell", attempt);

        let price = match get_bid_price(&quote_ctx, &symbol).await {
            Some(p) => p,
            None => {
                warn!(symbol, "Failed to get bid price, retrying...");
                sleep(retry_delay).await;
                continue;
            }
        };

        let remark = if symbol == SYMBOL_LONG {
            "Close Long"
        } else {
            "Close Short"
        };
        let order = build_limit_order(&symbol, OrderSide::Sell, remaining, price, Some(remark));

        let order_id = match trade_ctx.submit_order(order).await {
            Ok(resp) => resp.order_id,
            Err(e) => {
                warn!(symbol, error = %e, "Submit sell order failed");
                sleep(retry_delay).await;
                continue;
            }
        };

        info!(order_id = %order_id, "Sell order submitted");

        sleep(Duration::from_secs(ORDER_WAIT_SECS)).await;

        let order_detail = match trade_ctx.order_detail(&order_id).await {
            Ok(detail) => detail,
            Err(e) => {
                warn!(order_id = %order_id, error = %e, "Failed to fetch order detail");
                sleep(retry_delay).await;
                continue;
            }
        };

        let filled = order_detail.executed_quantity;
        remaining = (remaining - filled).max(decimal!(0));

        match order_detail.status {
            OrderStatus::Filled => info!(order_id = %order_id, "Order fully filled"),
            OrderStatus::PartialFilled if remaining < decimal!(1) => {
                info!(order_id = %order_id, filled = %filled, "Partially filled, less than 1 unit, stop");
            }
            OrderStatus::PartialFilled => {
                info!(order_id = %order_id, filled = %filled, remaining = %remaining, "Partially filled, continue");
            }
            status if is_order_terminal(&status) => {
                warn!(order_id = %order_id, ?status, "Order in terminal state");
            }
            _ => {
                if is_order_active_and_cancellable(&order_detail.status) {
                    info!(order_id = %order_id, "Cancelling unfilled order");
                    let _ = trade_ctx.cancel_order(&order_id).await;
                }
            }
        }

        sleep(retry_delay).await;
    }

    if remaining >= decimal!(1) {
        error!(symbol, remaining = %remaining, "Sell task failed after retries");
    } else {
        info!(symbol, "Sell task completed successfully");
    }
}

/// ==================== Buy Logic ====================
async fn buy_position(
    trade_ctx: Arc<TradeContext>,
    quote_ctx: Arc<QuoteContext>,
    symbol: &str,
) -> Result<(), TradingError> {
    let ratio: f64 = env::var("MAX_PURCHASE_RATIO")
        .unwrap_or_else(|_| DEFAULT_PURCHASE_RATIO.to_string())
        .parse()
        .unwrap_or(DEFAULT_PURCHASE_RATIO);

    let price = get_ask_price(&quote_ctx, symbol)
        .await
        .ok_or_else(|| TradingError::QuoteError("Failed to get ask price".to_string()))?;

    let opts =
        EstimateMaxPurchaseQuantityOptions::new(symbol, OrderType::LO, OrderSide::Buy).price(price);

    let estimate = trade_ctx
        .estimate_max_purchase_quantity(opts)
        .await
        .map_err(|e| TradingError::SdkError(e.to_string()))?;

    let quantity = (estimate.cash_max_qty * decimal!(ratio)).trunc();

    if quantity < decimal!(1) {
        warn!(symbol, "Insufficient quantity to buy");
        return Ok(());
    }

    // 启动后台买入任务
    tokio::spawn(buy_background_task(
        trade_ctx,
        quote_ctx,
        symbol.to_string(),
        quantity,
        RETRY_COUNT,
        Duration::from_secs(RETRY_DELAY_SECS),
    ));

    info!(symbol, quantity = %quantity, "Buy task started in background");
    Ok(())
}

/// ==================== Sell Logic ====================
async fn sell_position(
    trade_ctx: Arc<TradeContext>,
    quote_ctx: Arc<QuoteContext>,
    symbol: &str,
    quantity: Decimal,
) -> Result<(), TradingError> {
    let ratio: f64 = env::var("MAX_SELL_RATIO")
        .unwrap_or_else(|_| DEFAULT_SELL_RATIO.to_string())
        .parse()
        .unwrap_or(DEFAULT_SELL_RATIO);

    let target = (quantity * decimal!(ratio)).trunc();

    tokio::spawn(sell_background_task(
        trade_ctx,
        quote_ctx,
        symbol.to_string(),
        target,
        RETRY_COUNT,
        Duration::from_secs(RETRY_DELAY_SECS),
    ));

    info!(symbol, quantity = %target, "Sell task started in background");
    Ok(())
}

/// ==================== Trade Actions ====================
async fn do_long(state: &Arc<Mutex<AppState>>) -> Result<(), TradingError> {
    let state = state.lock().await;
    buy_position(
        Arc::clone(&state.trade_ctx),
        Arc::clone(&state.quote_ctx),
        SYMBOL_LONG,
    )
    .await
}

async fn do_short(state: &Arc<Mutex<AppState>>) -> Result<(), TradingError> {
    let state = state.lock().await;
    buy_position(
        Arc::clone(&state.trade_ctx),
        Arc::clone(&state.quote_ctx),
        SYMBOL_SHORT,
    )
    .await
}

async fn do_close_long(state: &Arc<Mutex<AppState>>) -> Result<(), TradingError> {
    let state = state.lock().await;
    let resp = state
        .trade_ctx
        .stock_positions(None)
        .await
        .map_err(|e| TradingError::SdkError(e.to_string()))?;

    for channel in resp.channels {
        for pos in channel.positions {
            if ![SYMBOL_LONG].contains(&pos.symbol.as_str()) {
                debug!(symbol = %pos.symbol, "Ignored non-target symbol");
                continue;
            }

            let trade_ctx = Arc::clone(&state.trade_ctx);
            let quote_ctx = Arc::clone(&state.quote_ctx);

            tokio::spawn(async move {
                if let Err(e) = sell_position(trade_ctx, quote_ctx, &pos.symbol, pos.quantity).await
                {
                    error!(error = %e, "Close position task failed");
                }
            });
        }
    }

    Ok(())
}

async fn do_close_short(state: &Arc<Mutex<AppState>>) -> Result<(), TradingError> {
    let state = state.lock().await;
    let resp = state
        .trade_ctx
        .stock_positions(None)
        .await
        .map_err(|e| TradingError::SdkError(e.to_string()))?;

    for channel in resp.channels {
        for pos in channel.positions {
            if ![SYMBOL_SHORT].contains(&pos.symbol.as_str()) {
                debug!(symbol = %pos.symbol, "Ignored non-target symbol");
                continue;
            }

            let trade_ctx = Arc::clone(&state.trade_ctx);
            let quote_ctx = Arc::clone(&state.quote_ctx);

            tokio::spawn(async move {
                if let Err(e) = sell_position(trade_ctx, quote_ctx, &pos.symbol, pos.quantity).await
                {
                    error!(error = %e, "Close position task failed");
                }
            });
        }
    }

    Ok(())
}

/// ==================== Webhook Handlers ====================
async fn webhook_handler(
    state: axum::extract::State<Arc<Mutex<AppState>>>,
    Json(payload): Json<WebhookRequest>,
) -> Result<Json<WebApiResponse>, TradingError> {
    info!("Received webhook: {:?}", payload);

    let action = match payload.action.to_lowercase().as_str() {
        "buy" => TradeAction::Buy,
        "sell" => TradeAction::Sell,
        _ => return Err(TradingError::ParseError("Invalid action".to_string())),
    };

    let sentiment = match payload.sentiment.to_lowercase().as_str() {
        "long" => MarketSentiment::Long,
        "short" => MarketSentiment::Short,
        "flat" => MarketSentiment::Flat,
        _ => return Err(TradingError::ParseError("Invalid sentiment".to_string())),
    };

    info!(?action, ?sentiment, "Parsed signal");

    match (&action, &sentiment) {
        (TradeAction::Buy, MarketSentiment::Long) => do_long(&state).await?,
        (TradeAction::Sell, MarketSentiment::Short) => do_short(&state).await?,
        (TradeAction::Buy, MarketSentiment::Flat) => do_close_long(&state).await?,
        (TradeAction::Sell, MarketSentiment::Flat) => do_close_short(&state).await?,
        _ => {
            warn!(?action, ?sentiment, "Unknown signal combination");
            return Ok(Json(WebApiResponse {
                status: "success",
                message: Some("Unknown signal combination".to_string()),
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

    info!("Starting server...");

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
    info!("Listening on: {}", addr);

    let listener = TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
