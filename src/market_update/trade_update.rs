use num::Zero;

use super::MarketUpdate;
use crate::{
    Result,
    market_update::market_update_trait::Exhausted,
    order_filters::{enforce_max_price, enforce_min_price, enforce_step_size},
    prelude::{Currency, LimitOrder, MarketState, Mon, Pending, PriceFilter, QuoteCurrency, Side},
    types::{TimestampNs, UserOrderId},
    utils::min,
};

// TODO: use `Getters`
/// A taker trade that consumes liquidity in the book.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Trade<I, const D: u8, BaseOrQuote>
where
    I: Mon<D>,
    BaseOrQuote: Currency<I, D>,
{
    /// The nanosecond timestamp at which this trade occurred at the exchange.
    pub timestamp_exchange_ns: TimestampNs,
    /// The price at which the trade executed at.
    pub price: QuoteCurrency<I, D>,
    /// The executed quantity.
    /// Generic denotation, e.g either Quote or Base currency denoted.
    pub quantity: BaseOrQuote,
    /// Either a buy or sell order.
    // TODO: remove field and derive from sign of `quantity` to save size of struct.
    pub side: Side,
}

impl<I, const D: u8, BaseOrQuote> Trade<I, D, BaseOrQuote>
where
    I: Mon<D>,
    BaseOrQuote: Currency<I, D>,
{
    /// If `true` then the `Trade` update fills the `order`.
    #[inline(always)]
    fn fills_order<UserOrderIdT: UserOrderId>(
        &self,
        order: &LimitOrder<I, D, BaseOrQuote, UserOrderIdT, Pending<I, D, BaseOrQuote>>,
    ) -> bool {
        match order.side() {
            Side::Buy => self.price < order.limit_price() && matches!(self.side, Side::Sell),
            Side::Sell => self.price > order.limit_price() && matches!(self.side, Side::Buy),
        }
    }
}

impl<I, const D: u8, BaseOrQuote> std::fmt::Display for Trade<I, D, BaseOrQuote>
where
    I: Mon<D>,
    BaseOrQuote: Currency<I, D>,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "price {}, quantity: {}, side: {}",
            self.price, self.quantity, self.side
        )
    }
}

impl<I, const D: u8, BaseOrQuote> MarketUpdate<I, D, BaseOrQuote> for Trade<I, D, BaseOrQuote>
where
    I: Mon<D>,
    BaseOrQuote: Currency<I, D>,
{
    const CAN_FILL_LIMIT_ORDERS: bool = true;

    #[inline]
    fn limit_order_filled<UserOrderIdT: UserOrderId>(
        &mut self,
        order: &LimitOrder<I, D, BaseOrQuote, UserOrderIdT, Pending<I, D, BaseOrQuote>>,
    ) -> Option<(BaseOrQuote, Exhausted)> {
        debug_assert!(
            self.quantity > BaseOrQuote::zero(),
            "The trade quantity must be greater than zero."
        );
        debug_assert!(order.remaining_quantity() > BaseOrQuote::zero());

        // Notice that the limit order price must be strictly lower or higher than the limit order price,
        // because we assume the limit order has the worst possible queue position in the book.
        if self.fills_order(order) {
            // Execute up to the quantity of the incoming `Trade`.
            let filled_qty = min(self.quantity, order.remaining_quantity());
            self.quantity -= filled_qty;
            debug_assert!(self.quantity >= Zero::zero());
            Some((filled_qty, self.quantity <= Zero::zero()))
        } else {
            None
        }
    }

    fn validate_market_update(&self, price_filter: &PriceFilter<I, D>) -> Result<()> {
        debug_assert!(self.price > QuoteCurrency::zero());
        enforce_min_price(price_filter.min_price(), self.price)?;
        enforce_max_price(price_filter.max_price(), self.price)?;
        enforce_step_size(price_filter.tick_size(), self.price)?;
        Ok(())
    }

    #[inline(always)]
    fn update_market_state(&self, market_state: &mut MarketState<I, D>) {
        market_state.set_last_trade_price(self.price);
    }

    #[inline(always)]
    fn timestamp_exchange_ns(&self) -> TimestampNs {
        self.timestamp_exchange_ns
    }

    #[inline(always)]
    fn can_fill_bids(&self) -> bool {
        match self.side {
            Side::Buy => false,
            Side::Sell => true,
        }
    }

    #[inline(always)]
    fn can_fill_asks(&self) -> bool {
        match self.side {
            Side::Buy => true,
            Side::Sell => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prelude::*;

    #[test_case::test_case(Side::Buy, false, true)]
    fn trade_update_can_fill_bids_asks(side: Side, can_fill_bid: bool, can_fill_ask: bool) {
        let trade = Trade {
            price: QuoteCurrency::<i64, 1>::new(100, 0),
            quantity: BaseCurrency::new(5, 0),
            side,
            timestamp_exchange_ns: 0.into(),
        };
        assert_eq!(trade.can_fill_bids(), can_fill_bid);
        assert_eq!(trade.can_fill_asks(), can_fill_ask);
    }

    // A buy trade can never fill a buy limit order.
    #[test_case::test_matrix([90, 95, 100, 105, 110])]
    fn trade_update_fills_buy_order_never(price: i64) {
        let new_order = LimitOrder::new(
            Side::Buy,
            QuoteCurrency::new(100, 0),
            BaseCurrency::new(5, 0),
        )
        .unwrap();
        let meta = ExchangeOrderMeta::default();
        let order = new_order.into_pending(meta);

        let trade = Trade {
            price: QuoteCurrency::<i64, 1>::new(price, 0),
            quantity: BaseCurrency::new(5, 0),
            side: Side::Buy,
            timestamp_exchange_ns: 0.into(),
        };
        assert!(!trade.fills_order(&order));
    }

    // A sell trade can fill a buy limit order at certain prices.
    #[test_case::test_matrix([90, 95, 99])]
    fn trade_update_fills_buy_order(price: i64) {
        let new_order = LimitOrder::new(
            Side::Buy,
            QuoteCurrency::new(100, 0),
            BaseCurrency::new(5, 0),
        )
        .unwrap();
        let meta = ExchangeOrderMeta::default();
        let order = new_order.into_pending(meta);

        let trade = Trade {
            price: QuoteCurrency::<i64, 1>::new(price, 0),
            quantity: BaseCurrency::new(5, 0),
            side: Side::Sell,
            timestamp_exchange_ns: 0.into(),
        };
        assert!(trade.fills_order(&order));
    }

    // A sell trade can fill a buy limit order at certain prices.
    #[test_case::test_matrix([100, 101, 105, 110])]
    fn trade_update_fills_buy_order_not(price: i64) {
        let new_order = LimitOrder::new(
            Side::Buy,
            QuoteCurrency::new(100, 0),
            BaseCurrency::new(5, 0),
        )
        .unwrap();
        let meta = ExchangeOrderMeta::default();
        let order = new_order.into_pending(meta);

        let trade = Trade {
            price: QuoteCurrency::<i64, 1>::new(price, 0),
            quantity: BaseCurrency::new(5, 0),
            side: Side::Sell,
            timestamp_exchange_ns: 0.into(),
        };
        assert!(!trade.fills_order(&order));
    }

    // A buy trade can never fill a buy limit order.
    #[test_case::test_matrix([90, 95, 100, 105, 110])]
    fn trade_update_fills_sell_order_never(price: i64) {
        let new_order = LimitOrder::new(
            Side::Sell,
            QuoteCurrency::new(100, 0),
            BaseCurrency::new(5, 0),
        )
        .unwrap();
        let meta = ExchangeOrderMeta::default();
        let order = new_order.into_pending(meta);

        let trade = Trade {
            price: QuoteCurrency::<i64, 1>::new(price, 0),
            quantity: BaseCurrency::new(5, 0),
            side: Side::Sell,
            timestamp_exchange_ns: 0.into(),
        };
        assert!(!trade.fills_order(&order));
    }

    // A sell trade can fill a buy limit order at certain prices.
    #[test_case::test_matrix([101, 105, 110])]
    fn trade_update_fills_sell_order(price: i64) {
        let new_order = LimitOrder::new(
            Side::Sell,
            QuoteCurrency::new(100, 0),
            BaseCurrency::new(5, 0),
        )
        .unwrap();
        let meta = ExchangeOrderMeta::default();
        let order = new_order.into_pending(meta);

        let trade = Trade {
            price: QuoteCurrency::<i64, 1>::new(price, 0),
            quantity: BaseCurrency::new(5, 0),
            side: Side::Buy,
            timestamp_exchange_ns: 0.into(),
        };
        assert!(trade.fills_order(&order));
    }

    // A sell trade can fill a buy limit order at certain prices.
    #[test_case::test_matrix([90, 95, 99])]
    fn trade_update_fills_sell_order_not(price: i64) {
        let new_order = LimitOrder::new(
            Side::Sell,
            QuoteCurrency::new(100, 0),
            BaseCurrency::new(5, 0),
        )
        .unwrap();
        let meta = ExchangeOrderMeta::default();
        let order = new_order.into_pending(meta);

        let trade = Trade {
            price: QuoteCurrency::<i64, 1>::new(price, 0),
            quantity: BaseCurrency::new(5, 0),
            side: Side::Buy,
            timestamp_exchange_ns: 0.into(),
        };
        assert!(!trade.fills_order(&order));
    }

    #[test]
    fn trade_update_market_state() {
        let trade = Trade {
            price: QuoteCurrency::<i64, 5>::new(100, 0),
            quantity: BaseCurrency::new(5, 0),
            side: Side::Buy,
            timestamp_exchange_ns: 0.into(),
        };
        let mut state = MarketState::default();
        trade.update_market_state(&mut state);
        assert_eq!(state.last_trade_price(), QuoteCurrency::new(100, 0));
    }

    #[test]
    fn trade_update_display() {
        let trade = Trade {
            price: QuoteCurrency::<i64, 5>::new(100, 0),
            quantity: BaseCurrency::new(5, 0),
            side: Side::Buy,
            timestamp_exchange_ns: 0.into(),
        };
        assert_eq!(
            &trade.to_string(),
            "price 100.00000 Quote, quantity: 5.00000 Base, side: Buy"
        );
    }

    #[test_case::test_matrix(
        [100, 110, 120],
        [1, 2, 3],
        [Side::Buy, Side::Sell]
    )]
    fn trade_limit_order_filled_some(price: i32, qty: i32, side: Side) {
        let price = QuoteCurrency::<i32, 2>::new(price, 0);
        let quantity = BaseCurrency::new(qty, 0);
        let mut trade = Trade {
            price,
            quantity,
            side,
            timestamp_exchange_ns: 0.into(),
        };

        let offset = match side {
            Side::Buy => QuoteCurrency::new(-1, 0),
            Side::Sell => QuoteCurrency::new(1, 0),
        };
        let limit_order = LimitOrder::new(side.inverted(), price + offset, quantity).unwrap();
        let meta = ExchangeOrderMeta::new(0.into(), 0.into());
        let limit_order = limit_order.into_pending(meta);
        assert_eq!(
            trade.limit_order_filled(&limit_order).unwrap(),
            (quantity, true)
        );
        assert_eq!(
            trade.quantity,
            Zero::zero(),
            "Trade quantity is reduced as well"
        );
    }

    #[test_case::test_matrix(
        [100, 110, 120],
        [1, 2, 3],
        [Side::Buy, Side::Sell]
    )]
    fn trade_limit_order_filled_none(price: i32, qty: i32, side: Side) {
        let price = QuoteCurrency::<i32, 2>::new(price, 0);
        let quantity = BaseCurrency::new(qty, 0);
        let mut trade = Trade {
            price,
            quantity,
            side,
            timestamp_exchange_ns: 0.into(),
        };
        let offset = match side {
            Side::Buy => QuoteCurrency::new(-1, 0),
            Side::Sell => QuoteCurrency::new(1, 0),
        };
        let limit_order = LimitOrder::new(
            side.inverted(),
            price + offset,
            quantity / BaseCurrency::new(2, 0),
        )
        .unwrap();
        let meta = ExchangeOrderMeta::new(0.into(), 0.into());
        let limit_order = limit_order.into_pending(meta);
        assert_eq!(
            trade.limit_order_filled(&limit_order).unwrap(),
            (quantity / BaseCurrency::new(2, 0), false)
        );
        assert_eq!(
            trade.quantity,
            quantity / BaseCurrency::new(2, 0),
            "Trade quantity is reduced as well"
        );
    }

    #[test]
    fn size_of_trade() {
        assert_eq!(
            std::mem::size_of::<Trade<i32, 2, BaseCurrency<i32, 2>>>(),
            24
        );
        assert_eq!(
            std::mem::size_of::<Trade<i64, 2, BaseCurrency<i64, 2>>>(),
            32
        );
    }
}
