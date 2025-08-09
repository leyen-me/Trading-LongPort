// src/main.rs

use axum::http;
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get, routing::post};
use longport::Decimal;
use longport::{
    Config, decimal,
    quote::QuoteContext,
    trade::{
        EstimateMaxPurchaseQuantityOptions, OrderSide, OrderType, OutsideRTH, StockPosition,
        SubmitOrderOptions, TimeInForceType, TradeContext,
    },
};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{debug, error, error_span, info, warn};
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
    quote_ctx: QuoteContext,
    trade_ctx: TradeContext,
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

/// ==================== 工具函数 ====================

/// 获取盘口价格（带重试）
async fn get_current_price(
    quote_ctx: &QuoteContext,
    action: &Action,
    symbol: &str,
    max_retries: usize,
    retry_delay: Duration,
) -> Result<Decimal, TradingError> {
    for attempt in 1..=max_retries {
        let span = error_span!("get_price", symbol = symbol, attempt = attempt);
        let _enter = span.enter();

        match quote_ctx.depth(symbol).await {
            Ok(depth_resp) => {
                if let Some(ask) = depth_resp.asks.first() {
                    if action == &Action::Buy {
                        if let Some(price) = ask.price {
                            info!(price = %price.to_string(), "买入参考价获取成功");
                            return Ok(price);
                        } else {
                            warn!("卖一价为空（可能夜盘）");
                        }
                    }
                }
                if let Some(bid) = depth_resp.bids.first() {
                    if action == &Action::Sell {
                        if let Some(price) = bid.price {
                            info!(price = %price.to_string(), "卖出参考价获取成功");
                            return Ok(price);
                        } else {
                            warn!("买一价为空（可能夜盘）");
                        }
                    }
                }
                warn!("无有效盘口数据");
            }
            Err(e) => {
                error!(error = %e, "获取盘口失败");
            }
        }

        tokio::time::sleep(retry_delay).await;
    }

    Err(TradingError::QuoteError(format!(
        "连续 {} 次获取盘口失败",
        max_retries
    )))
}

/// 获取买入参考价
async fn get_current_buy_price(
    quote_ctx: &QuoteContext,
    symbol: &str,
) -> Result<Decimal, TradingError> {
    get_current_price(
        quote_ctx,
        &Action::Buy,
        symbol,
        3,
        Duration::from_millis(500),
    )
    .await
}

/// 获取卖出参考价
async fn get_current_sell_price(
    quote_ctx: &QuoteContext,
    symbol: &str,
) -> Result<Decimal, TradingError> {
    get_current_price(
        quote_ctx,
        &Action::Sell,
        symbol,
        3,
        Duration::from_millis(500),
    )
    .await
}

/// 执行买入
async fn buy(
    trade_ctx: &TradeContext,
    quote_ctx: &QuoteContext,
    symbol: &str,
) -> Result<(), TradingError> {
    let current_price = get_current_buy_price(quote_ctx, symbol).await?;
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

    let order_opts = SubmitOrderOptions::new(
        symbol,
        OrderType::LO,
        OrderSide::Buy,
        quantity,
        TimeInForceType::GoodTilCanceled,
    )
    .submitted_price(price_decimal)
    .outside_rth(OutsideRTH::AnyTime)
    .remark(if symbol == DO_LONG_SYMBOL {
        "多头买入"
    } else {
        "空头买入"
    });

    match trade_ctx.submit_order(order_opts).await {
        Ok(resp) => {
            info!(order_id = %resp.order_id, "买入订单已提交");
        }
        Err(e) => {
            return Err(TradingError::TradeError(format!("买入失败: {}", e)));
        }
    }

    Ok(())
}

/// 执行卖出
async fn sell(
    trade_ctx: &TradeContext,
    quote_ctx: &QuoteContext,
    symbol: &str,
    quantity: Decimal,
) -> Result<(), TradingError> {
    let current_price = get_current_sell_price(quote_ctx, symbol).await?;
    let price_decimal = current_price;

    info!(
        symbol,
        quantity = %quantity.to_string(),
        price = %current_price.to_string(),
        "准备卖出"
    );

    let order_opts = SubmitOrderOptions::new(
        symbol,
        OrderType::LO,
        OrderSide::Sell,
        quantity,
        TimeInForceType::GoodTilCanceled,
    )
    .submitted_price(price_decimal)
    .outside_rth(OutsideRTH::AnyTime)
    .remark(if symbol == DO_LONG_SYMBOL {
        "多头卖出"
    } else {
        "空头卖出"
    });

    match trade_ctx.submit_order(order_opts).await {
        Ok(resp) => {
            info!(order_id = %resp.order_id, "卖出订单已提交");
        }
        Err(e) => {
            return Err(TradingError::TradeError(format!("卖出失败: {}", e)));
        }
    }

    Ok(())
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

    match (action, sentiment) {
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
            // warn!(?action, ?sentiment, "未识别的交易信号组合");
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
        quote_ctx,
        trade_ctx,
    }));

    // 构建路由
    let app = Router::new()
        .route("/webhook", post(webhook_handler))
        .route("/webhook_test", post(webhook_test_handler))
        .route("/health", get(|| async { "OK" }))
        .with_state(app_state)
        .layer(
            tower_http::cors::CorsLayer::new()
                .allow_origin("*".parse::<http::HeaderValue>().unwrap())
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
