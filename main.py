from longport.openapi import Config, QuoteContext, TradeContext, OrderType, OrderSide, TimeInForceType, OutsideRTH
import logging
from decimal import Decimal
from flask import Flask, request, jsonify
import datetime
from flask_cors import CORS
import time
import os

# ==================== Settings ====================
DO_LONG_SYMBOL = "TSLL.US"  # 做多的ETF
DO_SHORT_SYMBOL = "TSDD.US"  # 做空的ETF

# ==================== Init ====================
logging.basicConfig(level=logging.INFO,
                    format='%(asctime)s - %(levelname)s - %(message)s')
logger = logging.getLogger(__name__)

app = Flask(__name__)
CORS(app)

config = Config.from_env()
trade_ctx = TradeContext(config)
quote_ctx = QuoteContext(config)

class Action:
    BUY = "buy"
    SELL = "sell"

class Sentiment:
    LONG = "long"
    SHORT = "short"
    FLAT = "flat"

# ==================== Function ====================

def get_current_price(action: Action, symbol: str, max_retries: int = 3, retry_delay: float = 0.5):
    """获取当前盘口价格，带重试机制"""
    for attempt in range(1, max_retries + 1):
        try:
            resp = quote_ctx.depth(symbol)
            if resp.asks and resp.bids:
                if action == Action.BUY:
                    if resp.asks[0].price is not None:
                        logger.info(f"[{symbol}] 第{attempt}次获取盘口，买入参考价: {resp.asks[0].price}")
                        return resp.asks[0].price
                    else:
                        logger.warning(f"[{symbol}] 卖一价为空（可能夜盘）")
                elif action == Action.SELL:
                    if resp.bids[0].price is not None:
                        logger.info(f"[{symbol}] 第{attempt}次获取盘口，卖出参考价: {resp.bids[0].price}")
                        return resp.bids[0].price
                    else:
                        logger.warning(f"[{symbol}] 买一价为空（可能夜盘）")
            else:
                logger.warning(f"[{symbol}] 无盘口数据（第{attempt}次尝试）")
        except Exception as e:
            logger.error(f"[{symbol}] 获取盘口数据失败（第{attempt}次）：{e}")
        time.sleep(retry_delay)
    raise Exception(f"[{symbol}] 连续{max_retries}次获取盘口失败")

def get_current_buy_price(symbol: str):
    return get_current_price(Action.BUY, symbol)

def get_current_sell_price(symbol: str):
    return get_current_price(Action.SELL, symbol)

def buy(symbol: str):
    try:
        current_price = get_current_buy_price(symbol)
        max_buy_resp = trade_ctx.estimate_max_purchase_quantity(
            symbol=symbol,
            order_type=OrderType.LO,
            side=OrderSide.Buy,
            price=current_price
        )
        quantity = int(Decimal(max_buy_resp.cash_max_qty) * Decimal("0.9"))
        if quantity < 1:
            logger.warning(f"[{symbol}] 可买数量不足（计算后={quantity}），取消买入")
            return
        logger.info(f"[{symbol}] 最大买入数量: {max_buy_resp.cash_max_qty}，实际下单数量: {quantity}")
        trade_ctx.submit_order(
            symbol,
            OrderType.LO,
            OrderSide.Buy,
            Decimal(quantity),
            TimeInForceType.GoodTilCanceled,
            submitted_price=current_price,
            outside_rth=OutsideRTH.AnyTime,
            remark=f"{'多头' if DO_LONG_SYMBOL == symbol else '空头'}买入"
        )
        logger.info(f"[{symbol}] 买入订单已提交（数量={quantity}，价格={current_price}）")
    except Exception as e:
        logger.error(f"[{symbol}] 买入执行失败：{e}")

def sell(symbol: str, quantity: int):
    try:
        current_price = get_current_sell_price(symbol)
        logger.info(f"[{symbol}] 准备卖出，数量={quantity}，价格={current_price}")
        trade_ctx.submit_order(
            symbol,
            OrderType.LO,
            OrderSide.Sell,
            Decimal(quantity),
            TimeInForceType.GoodTilCanceled,
            submitted_price=current_price,
            outside_rth=OutsideRTH.AnyTime,
            remark=f"{'多头' if DO_LONG_SYMBOL == symbol else '空头'}卖出"
        )
        logger.info(f"[{symbol}] 卖出订单已提交")
    except Exception as e:
        logger.error(f"[{symbol}] 卖出执行失败：{e}")

def do_long():
    logger.info("执行做多操作")
    buy(DO_LONG_SYMBOL)

def do_short():
    logger.info("执行做空操作")
    buy(DO_SHORT_SYMBOL)

def do_close_position():
    logger.info("开始执行平仓流程")
    try:
        resp = trade_ctx.stock_positions()
        stock_positions = resp.channels
    except Exception as e:
        logger.error(f"获取持仓数量失败：{e}")
        return

    for channel in stock_positions:
        for position in channel.positions:
            if position.symbol in (DO_LONG_SYMBOL, DO_SHORT_SYMBOL):
                if position.quantity > 0 and position.quantity < 1:
                    logger.warning(f"[{position.symbol}] 剩余碎股，不平仓")
                elif position.quantity <= 0:
                    logger.info(f"[{position.symbol}] 无持仓")
                else:
                    logger.info(f"[{position.symbol}] 持仓数量={position.quantity}，准备卖出")
                    sell(position.symbol, position.quantity)
            else:
                logger.debug(f"[{position.symbol}] 非目标标的，忽略")

# ==================== Main ====================
@app.route('/webhook', methods=['POST'])
def webhook():
    try:
        webhook_data = request.json
        logger.info(f"收到TradingView信号: {webhook_data}")

        action = webhook_data.get('action', '').lower()
        sentiment = webhook_data.get('sentiment', '').lower()

        if not action or not sentiment:
            logger.error("信号数据不完整，缺少action或sentiment")
            return jsonify({'error': '信号数据不完整'}), 400

        logger.info(f"交易信号解析: action={action}, sentiment={sentiment}")

        if action == Action.BUY and sentiment == Sentiment.LONG:
            logger.info("信号=开多仓")
            do_long()
        elif action == Action.SELL and sentiment == Sentiment.SHORT:
            logger.info("信号=开空仓")
            do_short()
        elif (action == Action.SELL and sentiment == Sentiment.FLAT) or (action == Action.BUY and sentiment == Sentiment.FLAT):
            logger.info("信号=平仓")
            do_close_position()
        else:
            logger.warning(f"未识别的交易信号组合: action={action}, sentiment={sentiment}")

        return jsonify({'status': 'success'}), 200
    except Exception as e:
        logger.error(f"处理webhook时出错: {e}")
        return jsonify({'error': str(e)}), 500

@app.route('/webhook_test', methods=['POST'])
def webhook_test():
    webhook_data = request.json
    logger.info(f"收到测试信号: {webhook_data}")
    return jsonify({'status': 'success'}), 200

if __name__ == '__main__':
    logger.info("启动成功，北京时间：%s" % datetime.datetime.now().strftime("%Y-%m-%d %H:%M:%S"))
    app.run(host='0.0.0.0', port=80, debug=True)
