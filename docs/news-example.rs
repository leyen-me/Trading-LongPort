use reqwest::Client;
use chrono;
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport};


const WALLSTREET_API: &str = "https://api-prod.wallstreetcn.com/apiv1/content/lives"; // 华尔街见闻 api 地址
const CHANNEL: &str = "us-stock-channel"; // 美股快讯
const MAX_NEWS_COUNT: usize = 20;

const AI_BASE_API: &str = "https://api-inference.modelscope.cn/v1/chat/completions";


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

/// ==================== Email Utilities ====================
async fn send_email(
    subject: String,
    body: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let smtp_username = "672228275@qq.com";
    let smtp_password = env::var("SMTP_PASSWORD").map_err(|_| "环境变量 SMTP_PASSWORD 没有设置")?;

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
        Ok(_) => info!("📧 邮件发送成功"),
        Err(e) => error!("📧 邮件发送失败: {}", e),
    }

    Ok(())
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

    debug!("AI 分析: {}", raw_content);

    let analysis: AiAnalysis = serde_json::from_str(raw_content).map_err(|e| {
        error!("JSON 解析失败: {}", e);
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
    info!("开始分析新闻");

    let http_client = Client::new();

    let all_news = match fetch_wallstreet_news(&http_client, MAX_NEWS_COUNT).await {
        Ok(news) => news,
        Err(e) => {
            warn!("无法获取新闻: {}", e);
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
            error!("AI 分析失败: {}", e);

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


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    info!("服务启动中...");

    let app = Router::new()
        .route("/analyze_news", post(analyze_news_handler))
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