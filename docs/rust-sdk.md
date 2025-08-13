# longport sdk rust

### Depth

```rust
use std::sync::Arc;
use longport::{Config, quote::QuoteContext};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(Config::from_env()?);
    let (quote_ctx, _) = QuoteContext::try_new(config).await?;

    let resp = quote_ctx.depth("700.HK").await?;
    println!("{:?}", resp); // SecurityDepth { asks: [Depth { position: 1, price: Some(561.000), volume: 34300, order_num: 2 }], bids: [Depth { position: 1, price: Some(560.500), volume: 30100, order_num: 3 }] }
    Ok(())
}
```


### estimate_max_purchase_quantity

```rust
use std::sync::Arc;
use longport::{
    Config,decimal,
    trade::{EstimateMaxPurchaseQuantityOptions, OrderSide, OrderType, TradeContext},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(Config::from_env()?);
    let (trade_ctx, _) = TradeContext::try_new(config).await?;

    let estimate_max_purchase_quantity_options = EstimateMaxPurchaseQuantityOptions::new(
            "700.HK",
            OrderType::LO,
            OrderSide::Buy,
    ).price(decimal!(200));
    let resp = trade_ctx
        .estimate_max_purchase_quantity(estimate_max_purchase_quantity_options)
        .await?;
    println!("{:?}", resp); // EstimateMaxPurchaseQuantityResponse { cash_max_qty: 800, margin_max_qty: 8000 }

    Ok(())
}
```

### submit_order

```rust
use std::sync::Arc;
use longport::{
    Config, decimal,
    trade::{OrderSide, OrderType, SubmitOrderOptions, TimeInForceType, TradeContext, OutsideRTH},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(Config::from_env()?);
    let (trade_ctx, _) = TradeContext::try_new(config).await?;

    let opts = SubmitOrderOptions::new(
        "700.HK",
        OrderType::LO,
        OrderSide::Buy,
        decimal!(200),
        TimeInForceType::GoodTilCanceled,
    )
    .submitted_price(decimal!(50i32))
    .outside_rth(OutsideRTH::AnyTime)
    .remark("多头买入")
    ;

    let resp = trade_ctx.submit_order(opts).await?;
    println!("{:?}", resp); // SubmitOrderResponse { order_id: "1138710758427742208" }

    Ok(())
}
```

### stock_positions

``` rust
use std::sync::Arc;
use longport::{Config, trade::TradeContext};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Arc::new(Config::from_env()?);
    let (trade_ctx, _) = TradeContext::try_new(config).await?;

    let resp = trade_ctx.stock_positions(None).await?;
    println!("{:?}", resp); // StockPositionsResponse { channels: [StockPositionChannel { account_channel: "lb_papertrading", positions: [] }] }

    Ok(())
}

```