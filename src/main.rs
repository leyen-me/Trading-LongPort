use axum::http;
use axum::{Json, Router, http::StatusCode, response::IntoResponse, routing::get, routing::post};
use chrono;
use lettre::message::Mailbox;
use longport::Decimal;
use longport::{
    Config, decimal,
    quote::QuoteContext,
    trade::{
        EstimateMaxPurchaseQuantityOptions, OrderSide, OrderStatus, OrderType, OutsideRTH,
        SubmitOrderOptions, TimeInForceType, TradeContext,
    },
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use tracing_subscriber::FmtSubscriber;

use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};

/// ==================== Constants ====================
const DEFAULT_PURCHASE_RATIO: f64 = 0.5; // 每次都半仓买入
const DEFAULT_SELL_RATIO: f64 = 0.5; // 每次都半仓卖出
const RETRY_COUNT: usize = 5;
const RETRY_DELAY_SECS: u64 = 10;
const ORDER_WAIT_SECS: u64 = 30;

const WALLSTREET_API: &str = "https://api-prod.wallstreetcn.com/apiv1/content/lives"; // 华尔街见闻 api 地址
const CHANNEL: &str = "us-stock-channel"; // 美股快讯
const MAX_NEWS_COUNT: usize = 20;

const AI_BASE_API: &str = "https://api-inference.modelscope.cn/v1/chat/completions";

/// Symbol mapping config
/// 股票代码映射，例如你的Webhook监听的 TSLA 发出的信号，需要对 TSLA 做多或者做空（靠ETF实现）
fn symbol_mapping() -> HashMap<&'static str, (&'static str, &'static str)> {
    let mut m = HashMap::new();
    m.insert("TSLA", ("TSLL.US", "TSLQ.US"));
    m.insert("TSLL", ("TSLL.US", "TSLQ.US"));
    m.insert("NVDA", ("NVDL.US", "NVDS.US"));
    m
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RawNewsItem {
    id: i64,
    content_text: String,
    display_time: i64,
}

#[derive(Debug, Clone, Serialize)]
struct NewsItem {
    id: i64,
    time: String,
    timestamp: i64,
    content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WallStreetResponse {
    data: WallStreetData,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WallStreetData {
    items: Vec<RawNewsItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AiAnalysis {
    volatility: u8,
    direction: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize)]
struct AnalysisResponse {
    volatility: u8,
    direction: String,
    reason: String,
}

#[derive(Deserialize)]
struct ModelScopeResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: String,
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

fn get_symbols_for_ticker(ticker: &str) -> Option<(&'static str, &'static str)> {
    symbol_mapping().get(ticker).copied()
}

/// ==================== Order Builders ====================
fn build_limit_order(
    symbol: &str,
    side: OrderSide,
    quantity: Decimal,
    price: Decimal,
) -> SubmitOrderOptions {
    let order =
        SubmitOrderOptions::new(symbol, OrderType::LO, side, quantity, TimeInForceType::Day)
            .submitted_price(price)
            .outside_rth(OutsideRTH::AnyTime);
    order
}

/// ==================== Email Utilities ====================
async fn send_email(
    subject: String,
    body: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let smtp_username = "672228275@qq.com";
    let smtp_password = env::var("SMTP_PASSWORD").map_err(|_| "SMTP_PASSWORD not set")?;

    let email = Message::builder()
        .from(Mailbox::new(
            Some(smtp_username.to_owned()),
            smtp_username.parse().unwrap(),
        ))
        .to(Mailbox::new(
            Some(smtp_username.to_owned()),
            smtp_username.parse().unwrap(),
        ))
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body)
        .unwrap();

    let creds = Credentials::new(smtp_username.to_owned(), smtp_password);

    let mailer = SmtpTransport::relay("smtp.qq.com")
        .unwrap()
        .credentials(creds)
        .build();

    match mailer.send(&email) {
        Ok(_) => info!("📧 Email sent successfully"),
        Err(e) => error!("📧 Failed to send email: {}", e),
    }

    Ok(())
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

        let order = build_limit_order(&symbol, OrderSide::Buy, remaining, price);

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

        let order = build_limit_order(&symbol, OrderSide::Sell, remaining, price);

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

    // 启动新闻分析
    tokio::spawn(analyze_news_handler());

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
async fn do_long(state: &Arc<Mutex<AppState>>, symbol: &str) -> Result<(), TradingError> {
    let state = state.lock().await;
    buy_position(
        Arc::clone(&state.trade_ctx),
        Arc::clone(&state.quote_ctx),
        symbol,
    )
    .await
}

async fn do_short(state: &Arc<Mutex<AppState>>, symbol: &str) -> Result<(), TradingError> {
    let state = state.lock().await;
    buy_position(
        Arc::clone(&state.trade_ctx),
        Arc::clone(&state.quote_ctx),
        symbol,
    )
    .await
}

async fn do_close_all(
    state: &Arc<Mutex<AppState>>,
    long_symbol: &str,
    short_symbol: &str,
) -> Result<(), TradingError> {
    let state = state.lock().await;
    let resp = state
        .trade_ctx
        .stock_positions(None)
        .await
        .map_err(|e| TradingError::SdkError(e.to_string()))?;

    for channel in resp.channels {
        for pos in channel.positions {
            if ![long_symbol, short_symbol].contains(&pos.symbol.as_str()) {
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

    let ticker = payload.ticker;
    let (long_symbol, short_symbol) = get_symbols_for_ticker(&ticker)
        .ok_or_else(|| TradingError::ParseError(format!("Unknown ticker: {}", ticker)))?;

    info!(?action, ?sentiment, ticker, "Parsed signal");

    match (&action, &sentiment) {
        (TradeAction::Buy, MarketSentiment::Long) => do_long(&state, long_symbol).await?,
        (TradeAction::Sell, MarketSentiment::Short) => do_short(&state, short_symbol).await?,
        (_, MarketSentiment::Flat) => do_close_all(&state, long_symbol, short_symbol).await?,
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

async fn call_ai_analyze(
    client: &Client,
    news_content: &str,
) -> Result<AiAnalysis, Box<dyn std::error::Error + Send + Sync>> {
    let api_key = std::env::var("MODELSCOPE_API_KEY")
        .map_err(|_| "❌ 环境变量 MODELSCOPE_API_KEY 未设置！")?;

    let model_id = std::env::var("MODEL_ID")
        .unwrap_or_else(|_| "Qwen/Qwen3-235B-A22B-Instruct-2507".to_string());

    let system_prompt = format!(
        "现在是北京时间 {}。\n\n你是一名专业的美股分析师。请基于以下提供的新闻内容，对特斯拉（Tesla, Inc., 股票代码：TSLA）进行综合分析。\n\n分析应包括：\n- 新闻摘要与关键点提炼\n- 基本面影响\n- 市场情绪\n- 风险与机会\n\n请输出一个 JSON 对象，包含：\n- \"volatility\": 0–100 分数（>50 表示波动加剧）\n- \"direction\": \"up\"、\"down\" 或 \"sideways\"\n- \"reason\": 一句话（不超过30字）\n\n只返回合法 JSON，无额外内容。",
        chrono::Local::now().format("%Y年%m月%d日 %H:%M")
    );

    let payload = serde_json::json!({
        "model": model_id,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": news_content}
        ]
    });

    let response = client
        .post(AI_BASE_API)
        .bearer_auth(api_key)
        .json(&payload)
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(format!(
            "HTTP {}: {}",
            response.status(),
            response.text().await.unwrap_or_default()
        )
        .into());
    }

    let ai_resp: ModelScopeResponse = response.json().await?;
    let raw_content = ai_resp.choices[0].message.content.trim();

    debug!("Raw AI response: {}", raw_content);

    let analysis: AiAnalysis = serde_json::from_str(raw_content).map_err(|e| {
        error!("JSON parse error: {}", e);
        error!("Raw content: {}", raw_content);
        "AI 返回内容不是合法 JSON"
    })?;

    // 校验方向
    if !["up", "down", "sideways"].contains(&analysis.direction.as_str()) {
        return Err("AI 返回的 direction 不合法".into());
    }

    Ok(analysis)
}

async fn fetch_wallstreet_news(
    client: &Client,
    limit: usize,
) -> Result<Vec<NewsItem>, Box<dyn std::error::Error + Send + Sync>> {
    let params = [
        ("channel", CHANNEL),
        ("client", "pc"),
        ("cursor", "0"),
        ("limit", &limit.to_string()),
    ];
    let response = client.get(WALLSTREET_API).query(&params).send().await?;

    if !response.status().is_success() {
        return Err(format!(
            "HTTP {}: {}",
            response.status(),
            response.text().await.unwrap_or_default()
        )
        .into());
    }

    let json: WallStreetResponse = response.json().await?;
    let mut news_list = Vec::new();

    for item in json.data.items {
        if item.content_text.trim().len() < 10 {
            continue;
        }

        let time_str = chrono::DateTime::from_timestamp(item.display_time, 0)
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "Unknown".to_string());

        news_list.push(NewsItem {
            id: item.id,
            content: item.content_text,
            time: time_str,
            timestamp: item.display_time,
        });
    }

    Ok(news_list)
}

async fn analyze_news_handler() -> Result<Json<AnalysisResponse>, impl IntoResponse> {
    info!("Received request to analyze news");

    let http_client = Client::new();

    let all_news = match fetch_wallstreet_news(&http_client, MAX_NEWS_COUNT).await {
        Ok(news) => news,
        Err(e) => {
            warn!("Failed to fetch news: {}", e);
            return Err((
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "无法获取新闻",
                    "detail": e.to_string()
                })),
            ));
        }
    };

    if all_news.is_empty() {
        return Ok(Json(AnalysisResponse {
            volatility: 30,
            direction: "sideways".to_string(),
            reason: "暂无相关新闻".to_string(),
        }));
    }

    // 3. 拼接内容
    let news_input = all_news
        .iter()
        .map(|n| format!("[{}] {}", n.time, n.content))
        .collect::<Vec<_>>()
        .join("\n\n");

    // 4. 调用 AI
    match call_ai_analyze(&http_client, &news_input).await {
        Ok(analysis) => {
            info!(
                volatility = analysis.volatility,
                direction = %analysis.direction,
                reason = %analysis.reason,
                "AI 分析完成"
            );

            let body: String = format!("{:?}", analysis);
            let _ = send_email("盘整突破交易系统".to_string(), body).await;
            Ok(Json(AnalysisResponse {
                volatility: analysis.volatility,
                direction: analysis.direction,
                reason: analysis.reason,
            }))
        }
        Err(e) => {
            error!("AI analysis failed: {}", e);

            let body = "新闻分析失败，请检查配置";
            let _ = send_email("盘整突破交易系统".to_string(), body.to_string()).await;
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "AI 分析失败",
                    "detail": e.to_string()
                })),
            ))
        }
    }
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
        .route("/analyze_news", post(analyze_news_handler))
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