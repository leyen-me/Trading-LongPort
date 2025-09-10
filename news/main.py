import os
import time
import json
import requests
from openai import OpenAI

# ========================
# 配置区
# ========================

MODEL_ID = "Qwen/Qwen3-235B-A22B-Instruct-2507"
BASE_URL = "https://api-inference.modelscope.cn/v1"
API_KEY = os.getenv("MODELSCOPE_API_KEY")
if not API_KEY:
    raise ValueError("❌ 环境变量 MODELSCOPE_API_KEY 未设置！请先配置。")

MAX_STORED_NEWS = 100 # 最多存储100条新闻
MAX_NEWS_TO_ANALYZE = 20 # 最多给20条新闻给ai
CHECK_INTERVAL = 300  # 每5分钟检查一次
recent_news = [] # 存储最近新闻（用于去重和时间窗口）

client = OpenAI(
    base_url=BASE_URL,
    api_key=API_KEY,
)

def get_system_prompt_with_time():
    current_time = time.strftime("%Y年%m月%d日 %H:%M", time.localtime())
    return f"""
    现在是北京时间 {current_time}。
    
    你是一名专业的美股分析师。请基于以下提供的新闻内容，对特斯拉（Tesla, Inc., 股票代码：TSLA）进行综合分析。分析应包括以下维度：

    新闻摘要与关键点提炼：简要总结所提供的新闻内容，突出与 TSLA 相关的核心信息（如财报、CEO 言论、供应链、政策影响、竞争动态等）。
    基本面影响分析：评估新闻对 TSLA 当前及未来盈利能力、营收增长、毛利率、产能扩张等方面的影响。
    市场情绪与投资者反应：分析新闻可能引发的市场情绪变化（乐观/悲观），是否可能导致机构或散户行为变化。
    风险与机会评估：指出该新闻带来的潜在风险（如监管、竞争、执行风险）和投资机会（如低估、增长催化剂）。
    请使用专业、客观的语言，避免过度推测。
    
    请你基于新闻进行分析并输出输出一个 JSON 对象，对象包含三个字段：

    - "volatility"：数值型分数 0–100，衡量未来1-5个交易日的预期波动强度（>50 表示波动加剧，适合开仓）
    - "direction"：字符串，取值为 "up"、"down" 或 "sideways"
    - "reason"：一句话（不超过30字）概括核心逻辑，聚焦关键驱动（如马斯克言论、政策、数据、宏观等）

    输出必须是合法 JSON，仅包含上述英文字段。不要任何额外内容。
    """

WALLSTREET_API = 'https://api-prod.wallstreetcn.com/apiv1/content/lives'
HEADERS = {
    'User-Agent': 'Mozilla/5.0',
    'Accept': 'application/json'
}
PARAMS = {'channel': 'us-stock-channel', 'client': 'pc', 'cursor': 0, 'limit': 10}





def get_news():
    try:
        resp = requests.get(WALLSTREET_API, headers=HEADERS, params=PARAMS, verify=False)
        if resp.status_code != 200:
            print("网络错误:", resp.status_code)
            return []

        data = resp.json().get('data', {})
        items = data.get('items', [])
        PARAMS['cursor'] = data.get('next_cursor', PARAMS['cursor'])

        news_list = []
        for item in items:
            content = item['content_text'].strip()
            pub_time = item['display_time']
            if not content or len(content) < 10:
                continue
            news_list.append({
                'id': item['id'],
                'time': time.strftime('%Y-%m-%d %H:%M:%S', time.localtime(pub_time)),
                'timestamp': pub_time,
                'content': content
            })
        return news_list
    except Exception as e:
        print("抓取失败:", str(e))
        return []

def filter_relevant_news(news_list):
    """按时间倒序取最多 MAX_NEWS_TO_ANALYZE 条"""
    relevant = []
    for news in news_list:
        relevant.append(news)
    relevant.sort(key=lambda x: x['timestamp'], reverse=True)
    return relevant[:MAX_NEWS_TO_ANALYZE]

def call_openai_for_volatility_score(news_content):
    try:
        system_prompt = get_system_prompt_with_time()
        response = client.chat.completions.create(
            model=MODEL_ID,
            messages=[
                {
                    'role': 'system',
                    'content': system_prompt
                },
                {
                    'role': 'user',
                    'content': news_content
                }
            ]
        )
        response_message = response.choices[0].message.content
        return response_message
    except Exception as e:
        print("OpenAI 调用失败:", str(e))
        return None

def main_loop():
    global recent_news
    print("🚀 开始监控美股新闻波动率（基于 OpenAI）...")

    while True:
        print(f"\n🔄 正在获取最新新闻...")
        new_news = get_news()
        # 去重
        new_ids = {n['id'] for n in new_news}
        recent_news = [n for n in recent_news if n['id'] not in new_ids]
        recent_news.extend(new_news)

        recent_news = sorted(recent_news, key=lambda x: x['timestamp'], reverse=True)[:MAX_STORED_NEWS]
        relevant_news = filter_relevant_news(recent_news)
        
        if not relevant_news:
            print(f"🟡 [{time.strftime('%H:%M:%S')}] 未发现特斯拉相关新闻。")
            time.sleep(CHECK_INTERVAL)
            continue
        
        news_input = "\n\n".join([
                f"[{n['time']}] {n['content']}" for n in relevant_news
            ])

        raw_response = call_openai_for_volatility_score(news_input)
        if not raw_response:
            time.sleep(CHECK_INTERVAL)
            continue
        # 尝试解析 JSON
        try:
            parsed = json.loads(raw_response.strip())
            volatility = parsed.get("volatility", "N/A")
            direction = parsed.get("direction", "N/A")
            reason = parsed.get("reason", "N/A")
            print(f"📊 [{time.strftime('%H:%M:%S')}] 分析结果:")
            print(f"   Volatility: {volatility} | Direction: {direction} | Reason: {reason}")

            # 🔔 可选：触发开仓信号
            # if isinstance(volatility, (int, float)) and volatility > 50:
            #     print(f"✅✅✅ 【开仓信号】波动率超标！评分: {volatility}")

        except json.JSONDecodeError:
            print(f"❌ 返回内容不是合法 JSON: {raw_response}")
            print(f"Raw response:\n{raw_response}")

        time.sleep(CHECK_INTERVAL)

if __name__ == "__main__":
    main_loop()