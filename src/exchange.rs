use std::cmp::Ordering;

use assert2::assert;
use getset::{Getters, MutGetters};
use num_traits::Zero;
use tracing::{debug, info, trace, warn};

use crate::{
    config::Config,
    market_state::MarketState,
    order_margin::OrderMargin,
    order_rate_limiter::OrderRateLimiter,
    prelude::{
        ActiveLimitOrders, Currency, MarketUpdate, Mon, OrderError, Position, QuoteCurrency,
        RePricing,
    },
    risk_engine::{IsolatedMarginRiskEngine, RiskEngine},
    types::{
        Balances, Error, ExchangeOrderMeta, Filled, LimitOrder, LimitOrderFill, MarginCurrency,
        MarketOrder, NewOrder, OrderId, Pending, Result, RiskError, Side, TimestampNs, UserOrderId,
    },
};

/// Whether to cancel a limit order by its `OrderId` or the `UserOrderId`.
#[allow(missing_docs)]
#[derive(Debug, Clone, Copy)]
pub enum CancelBy<UserOrderIdT: UserOrderId> {
    OrderId(OrderId),
    UserOrderId(UserOrderIdT),
}

/// Relevant information about the traders account.
///
/// Generics:
/// - `I`: The numeric data type of currencies.
/// - `D`: The constant decimal precision of the currencies.
/// - `BaseOrQuote`: Either `BaseCurrency` or `QuoteCurrency` depending on the futures type.
/// - `UserOrderId`: The type of user order id to use. Set to `()` if you don't need one.
#[derive(Debug)]
pub struct Account<'a, I, const D: u8, BaseOrQuote, UserOrderIdT>
where
    I: Mon<D>,
    BaseOrQuote: Currency<I, D>,
    BaseOrQuote::PairedCurrency: MarginCurrency<I, D>,
    UserOrderIdT: UserOrderId,
{
    /// The active limit orders of the account.
    pub active_limit_orders: &'a ActiveLimitOrders<I, D, BaseOrQuote, UserOrderIdT>,
    /// The current position of the account.
    pub position: &'a Position<I, D, BaseOrQuote>,
    /// The TAccount balances of the account.
    pub balances: &'a Balances<I, D, BaseOrQuote::PairedCurrency>,
}

/// The main leveraged futures exchange for simulated trading
#[derive(Debug, Clone, Getters, MutGetters)]
pub struct Exchange<I, const D: u8, BaseOrQuote, UserOrderIdT>
where
    I: Mon<D>,
    BaseOrQuote: Currency<I, D>,
    BaseOrQuote::PairedCurrency: MarginCurrency<I, D>,
    UserOrderIdT: UserOrderId,
{
    /// The exchange configuration.
    #[getset(get = "pub")]
    config: Config<I, D, BaseOrQuote::PairedCurrency>,

    /// The current state of the simulated market.
    #[getset(get = "pub")]
    market_state: MarketState<I, D>,

    risk_engine: IsolatedMarginRiskEngine<I, D, BaseOrQuote>,

    next_order_id: OrderId,

    /// The balances of the user including margin amounts.
    #[getset(get = "pub")]
    #[cfg_attr(test, getset(get_mut = "pub(crate)"))]
    balances: Balances<I, D, BaseOrQuote::PairedCurrency>,

    /// Get the current position of the user.
    #[getset(get = "pub")]
    #[cfg_attr(test, getset(get_mut = "pub(crate)"))]
    position: Position<I, D, BaseOrQuote>,

    /// The order margin state.
    #[getset(get = "pub")]
    order_margin: OrderMargin<I, D, BaseOrQuote, UserOrderIdT>,

    // To avoid allocations in hot-paths
    limit_order_updates: Vec<LimitOrderFill<I, D, BaseOrQuote, UserOrderIdT>>,

    order_rate_limiter: OrderRateLimiter,
}

impl<I, const D: u8, BaseOrQuote, UserOrderIdT> Exchange<I, D, BaseOrQuote, UserOrderIdT>
where
    I: Mon<D>,
    BaseOrQuote: Currency<I, D>,
    BaseOrQuote::PairedCurrency: MarginCurrency<I, D>,
    UserOrderIdT: UserOrderId,
{
    /// Create a new Exchange with the desired config and whether to use candles
    /// as information source
    pub fn new(config: Config<I, D, BaseOrQuote::PairedCurrency>) -> Self {
        let market_state = MarketState::default();
        let risk_engine = IsolatedMarginRiskEngine::new(config.contract_spec().clone());

        let max_active_orders = config.max_num_open_orders();
        let order_rate_limiter =
            OrderRateLimiter::new(config.order_rate_limits().orders_per_second());
        let balances = Balances::new(config.starting_wallet_balance());
        Self {
            config,
            market_state,
            risk_engine,
            next_order_id: OrderId::default(),
            balances,
            position: Position::default(),
            order_margin: OrderMargin::new(max_active_orders),
            limit_order_updates: Vec::with_capacity(max_active_orders.get()),
            order_rate_limiter,
        }
    }

    /// The the users currently active limit orders.
    #[inline]
    pub fn active_limit_orders(&self) -> &ActiveLimitOrders<I, D, BaseOrQuote, UserOrderIdT> {
        self.order_margin.active_limit_orders()
    }

    /// Get information about the `Account`
    pub fn account(&self) -> Account<I, D, BaseOrQuote, UserOrderIdT> {
        Account {
            active_limit_orders: self.active_limit_orders(),
            position: &self.position,
            balances: self.balances(),
        }
    }

    /// Update the exchange state with new information
    /// Returns a reference to order updates vector for performance reasons.
    ///
    /// ### Parameters:
    /// `timestamp_ns`: Is used in the AccountTracker `A`
    ///     and if setting order timestamps is enabled in the config.
    /// `market_update`: Newest market information
    ///
    /// ### Returns:
    /// If Ok, returns updates regarding limit orders, wether partially filled or fully.
    pub fn update_state<U>(
        &mut self,
        market_update: &U,
    ) -> std::result::Result<&Vec<LimitOrderFill<I, D, BaseOrQuote, UserOrderIdT>>, RiskError>
    where
        U: MarketUpdate<I, D, BaseOrQuote>,
    {
        trace!("update_state: market_update: {market_update}");

        self.market_state
            .update_state(market_update, self.config.contract_spec().price_filter());

        if let Err(e) = <IsolatedMarginRiskEngine<I, D, BaseOrQuote> as RiskEngine<
            I,
            D,
            BaseOrQuote,
            UserOrderIdT,
        >>::check_maintenance_margin(
            &self.risk_engine, &self.market_state, &self.position
        ) {
            self.liquidate();
            return Err(e);
        };

        self.check_active_orders(market_update.clone());
        Ok(&self.limit_order_updates)
    }

    /// Set the best bid and ask, alternatively a `Bba` `MarketUpdate` can be passed into `update_state`
    #[inline]
    pub fn set_best_bid_and_ask(&mut self, bid: QuoteCurrency<I, D>, ask: QuoteCurrency<I, D>) {
        debug_assert!(bid < ask);
        self.market_state.set_bid(bid);
        self.market_state.set_ask(ask);
    }

    // Liquidate the position by closing it with a market order.
    fn liquidate(&mut self) {
        warn!("liquidating position {}", self.position);
        debug_assert!(self.market_state.ask() > QuoteCurrency::zero());
        debug_assert!(self.market_state.bid() > QuoteCurrency::zero());
        let order = match &self.position {
            Position::Long(pos) => {
                MarketOrder::new(Side::Sell, pos.quantity()).expect("Can create market order.")
            }
            Position::Short(pos) => {
                MarketOrder::new(Side::Buy, pos.quantity()).expect("Can create market order.")
            }
            Position::Neutral => panic!("A neutral position can not be liquidated"),
        };
        self.submit_market_order(order)
            .expect("Must be able to submit liquidation order");
        info!("balances after liquidation: {:?}", self.balances());
    }

    /// Submit a new `MarketOrder` to the exchange.
    ///
    /// # Arguments:
    /// `order`: The order that is being submitted.
    ///
    /// # Returns:
    /// If Ok, the order with timestamp and id filled in.
    /// Else its an error.
    pub fn submit_market_order(
        &mut self,
        order: MarketOrder<I, D, BaseOrQuote, UserOrderIdT, NewOrder>,
    ) -> Result<MarketOrder<I, D, BaseOrQuote, UserOrderIdT, Filled<I, D, BaseOrQuote>>> {
        self.order_rate_limiter
            .aquire(self.market_state.current_ts_ns())?;
        // Basic checks
        self.config
            .contract_spec()
            .quantity_filter()
            .validate_order_quantity(order.quantity())?;

        let meta = ExchangeOrderMeta::new(
            self.next_order_id(),
            self.market_state.current_timestamp_ns(),
        );
        let order = order.into_pending(meta);

        assert2::debug_assert!(self.market_state.ask() > QuoteCurrency::zero());
        assert2::debug_assert!(self.market_state.bid() > QuoteCurrency::zero());
        let fill_price = match order.side() {
            Side::Buy => self.market_state.ask(),
            Side::Sell => self.market_state.bid(),
        };
        self.risk_engine
            .check_market_order(&self.position, &order, fill_price, &self.balances)?;

        let filled_order = order.into_filled(fill_price, self.market_state.current_timestamp_ns());
        self.settle_filled_market_order(filled_order.clone());

        Ok(filled_order)
    }

    fn settle_filled_market_order(
        &mut self,
        order: MarketOrder<I, D, BaseOrQuote, UserOrderIdT, Filled<I, D, BaseOrQuote>>,
    ) {
        let filled_qty = order.quantity();
        assert2::debug_assert!(filled_qty > BaseOrQuote::zero());
        let fill_price = order.state().avg_fill_price();
        assert2::debug_assert!(fill_price > QuoteCurrency::zero());

        let notional = BaseOrQuote::PairedCurrency::convert_from(filled_qty, fill_price);
        let fee = notional * *self.config.contract_spec().fee_taker().as_ref();

        self.position.change(
            filled_qty,
            fill_price,
            order.side(),
            &mut self.balances,
            self.config.contract_spec().init_margin_req(),
        );
        self.balances.account_for_fee(fee);
    }

    #[inline]
    fn next_order_id(&mut self) -> OrderId {
        let oid = self.next_order_id;
        self.next_order_id.incr();
        oid
    }

    /// # Arguments:
    /// `order`: The order that is being submitted.
    ///
    /// # Returns:
    /// If Ok, the order with timestamp and id filled in.
    /// Else its an error.
    pub fn submit_limit_order(
        &mut self,
        order: LimitOrder<I, D, BaseOrQuote, UserOrderIdT, NewOrder>,
    ) -> Result<LimitOrder<I, D, BaseOrQuote, UserOrderIdT, Pending<I, D, BaseOrQuote>>> {
        trace!("submit_order: {}", order);

        self.order_rate_limiter
            .aquire(self.market_state.current_ts_ns())?;
        // Basic checks
        self.config
            .contract_spec()
            .quantity_filter()
            .validate_order_quantity(order.remaining_quantity())?;
        self.config
            .contract_spec()
            .price_filter()
            .validate_limit_price(order.limit_price(), self.market_state.mid_price())?;

        let meta = ExchangeOrderMeta::new(
            self.next_order_id(),
            self.market_state.current_timestamp_ns(),
        );
        let order = order.into_pending(meta);

        self.risk_engine.check_limit_order(
            &self.position,
            &order,
            self.balances.available(),
            &self.order_margin,
        )?;

        // If a limit order is marketable, it will take liquidity from the book at the `limit_price` price level and pay the taker fee,
        let marketable = match order.side() {
            Side::Buy => order.limit_price() >= self.market_state.ask(),
            Side::Sell => order.limit_price() <= self.market_state.bid(),
        };
        match order.re_pricing() {
            RePricing::GoodTilCrossing => {
                if marketable {
                    return Err(Error::OrderError(
                        OrderError::GoodTillCrossingRejectedOrder {
                            limit_price: order.limit_price().to_string(),
                            away_market_quotation_price: match order.side() {
                                Side::Buy => self.market_state.ask().to_string(),
                                Side::Sell => self.market_state.bid().to_string(),
                            },
                        },
                    ));
                }
            }
        }

        self.append_limit_order(order.clone(), marketable)?;

        Ok(order)
    }

    /// Amend an existing limit order.
    ///
    /// The amend message will only be accepted if the original order can be successfully removed.
    /// Requests which cannot be processed will be rejected with an error.
    ///
    /// The new order get a new `OrderId` as well.
    pub fn amend_limit_order(
        &mut self,
        existing_order_id: OrderId,
        mut new_order: LimitOrder<I, D, BaseOrQuote, UserOrderIdT, NewOrder>,
    ) -> Result<LimitOrder<I, D, BaseOrQuote, UserOrderIdT, Pending<I, D, BaseOrQuote>>> {
        self.order_rate_limiter
            .aquire(self.market_state.current_ts_ns())?;
        let existing_order = self
            .active_limit_orders()
            .get_by_id(existing_order_id, new_order.side()) // Its assumed that `new_order` has the same side as existing order.
            .ok_or_else(|| {
                if existing_order_id < self.next_order_id {
                    Error::OrderNoLongerActive
                } else {
                    Error::OrderIdNotFound {
                        order_id: existing_order_id,
                    }
                }
            })?;
        // When the order is in partially filled status and the new quantity <= `filled_quantity`, as per `binance` docs.
        //
        // As per cboe: "Changes in OrderQty result in an adjustment of the current order’s OrderQty. The new OrderQty does
        // not directly replace the current order’s LeavesQty. Rather, a delta is computed from the current
        // OrderQty and the replacement OrderQty. This delta is then applied to the current LeavesQty. If the
        // resulting LeavesQty is less than or equal to zero, the order is cancelled. This results in safer behavior
        // when the modification request overlaps partial fills for the current order, leaving the Member in total
        // control of the share exposure of the order"
        let qty_delta = new_order.total_quantity() - existing_order.total_quantity();
        trace!("qty_delta: {qty_delta}");
        let new_leaves_qty = existing_order.remaining_quantity() + qty_delta;
        if new_leaves_qty <= BaseOrQuote::zero() {
            self.cancel_limit_order(CancelBy::OrderId(existing_order_id))
                .expect("Can cancel this order");
            return Err(Error::AmendQtyAlreadyFilled);
        }

        new_order.set_remaining_quantity(new_leaves_qty);

        self.cancel_limit_order(CancelBy::OrderId(existing_order_id))?;
        self.submit_limit_order(new_order)
    }

    /// Append a new limit order as active order.
    /// If limit order is `marketable`, the order will take liquidity from the book at the `limit_price` price level.
    /// Then it pays the taker fee for the quantity that was taken from the book, the rest of the quantity (if any)
    /// will be placed into the book as a passive order.
    fn append_limit_order(
        &mut self,
        order: LimitOrder<I, D, BaseOrQuote, UserOrderIdT, Pending<I, D, BaseOrQuote>>,
        marketable: bool,
    ) -> Result<()> {
        trace!("append_limit_order: order: {order}, marketable: {marketable}");
        trace!(
            "active_limit_orders: {}, market_state: {}, position: {}",
            self.active_limit_orders(),
            self.market_state,
            self.position,
        );

        let init_margin_req = self.config().contract_spec().init_margin_req();
        self.order_margin
            .try_insert(order, &mut self.balances, &self.position, init_margin_req)?;
        debug_assert!(if self.active_limit_orders().is_empty() {
            self.balances.order_margin().is_zero()
        } else {
            true
        });
        self.balances.debug_assert_state();

        Ok(())
    }

    /// Cancel an active limit order.
    /// returns Some order if successful with given order_id
    pub fn cancel_limit_order(
        &mut self,
        cancel_by: CancelBy<UserOrderIdT>,
    ) -> Result<LimitOrder<I, D, BaseOrQuote, UserOrderIdT, Pending<I, D, BaseOrQuote>>> {
        trace!("cancel_order: by {:?}", cancel_by);
        self.order_rate_limiter
            .aquire(self.market_state.current_ts_ns())?;
        debug_assert_eq!(
            self.balances.order_margin(),
            self.order_margin.order_margin(
                self.config.contract_spec().init_margin_req(),
                &self.position,
            )
        );

        let init_margin_req = self.config().contract_spec().init_margin_req();
        let removed_order = self.order_margin.remove(
            cancel_by,
            &mut self.balances,
            &self.position,
            init_margin_req,
        )?;

        assert!(if self.active_limit_orders().is_empty() {
            self.balances.order_margin().is_zero()
        } else {
            true
        });

        Ok(removed_order)
    }

    /// Checks for the execution of active limit orders in the account.
    /// NOTE: only public for benchmarking purposes.
    pub fn check_active_orders<U>(&mut self, mut market_update: U)
    where
        U: MarketUpdate<I, D, BaseOrQuote>,
    {
        // Clear any potential order updates from the previous iteration.
        self.limit_order_updates.clear();

        if !U::CAN_FILL_LIMIT_ORDERS {
            return;
        }

        if market_update.can_fill_bids() {
            // peek at the best bid order.
            while let Some(order) = self.active_limit_orders().peek_best_bid() {
                // TODO: if some quantity was filled, mutate `market_update` to reflect the reduced liquidity so it does not fill more orders than possible.
                if let Some((filled_qty, exhausted)) = market_update.limit_order_filled(order) {
                    self.fill_limit_order(
                        order.clone(),
                        filled_qty,
                        market_update.timestamp_exchange_ns(),
                    );
                    if exhausted {
                        return;
                    }
                } else {
                    // We can be sure that no other bid can be filled if this one could not be filled.
                    break;
                }
            }
        }

        if market_update.can_fill_asks() {
            while let Some(order) = self.active_limit_orders().peek_best_ask() {
                // TODO: if some quantity was filled, mutate `market_update` to reflect the reduced liquidity so it does not fill more orders than possible.
                if let Some((filled_qty, exhausted)) = market_update.limit_order_filled(order) {
                    self.fill_limit_order(
                        order.clone(),
                        filled_qty,
                        market_update.timestamp_exchange_ns(),
                    );
                    if exhausted {
                        return;
                    }
                } else {
                    // We can be sure that no other ask can be filled if this one could not be filled.
                    break;
                }
            }
        }

        assert2::debug_assert!(if self.active_limit_orders().is_empty() {
            self.balances.order_margin().is_zero()
        } else {
            true
        });
        debug_assert_eq!(
            self.balances.order_margin(),
            self.order_margin.order_margin(
                self.config.contract_spec().init_margin_req(),
                &self.position
            )
        );
        self.balances.debug_assert_state();
    }

    fn fill_limit_order(
        &mut self,
        mut order: LimitOrder<I, D, BaseOrQuote, UserOrderIdT, Pending<I, D, BaseOrQuote>>,
        filled_qty: BaseOrQuote,
        ts_ns: TimestampNs,
    ) {
        debug!(
            "filled limit {} order {}: {filled_qty}/{} @ {}",
            order.side(),
            order.id(),
            order.remaining_quantity(),
            order.limit_price()
        );
        assert2::debug_assert!(
            filled_qty > BaseOrQuote::zero(),
            "The filled_qty must be greater than zero"
        );
        debug_assert_eq!(
            self.balances.order_margin(),
            self.order_margin.order_margin(
                self.config.contract_spec().init_margin_req(),
                &self.position
            )
        );

        let side = order.side();
        let limit_price = order.limit_price();
        let notional = BaseOrQuote::PairedCurrency::convert_from(filled_qty, limit_price);
        let fee = notional * *self.config.contract_spec().fee_maker().as_ref();
        self.balances.account_for_fee(fee);

        let limit_order_update = order.fill(filled_qty, fee, ts_ns);
        let init_margin_req = self.config().contract_spec().init_margin_req();
        if let LimitOrderFill::FullyFilled { .. } = limit_order_update {
            self.order_margin
                .remove(
                    CancelBy::OrderId(order.id()),
                    &mut self.balances,
                    &self.position,
                    init_margin_req,
                )
                .expect("Can remove order as its an internal call");
        } else {
            assert2::debug_assert!(order.remaining_quantity() > BaseOrQuote::zero());
            self.order_margin
                .fill_order(order, &mut self.balances, &self.position, init_margin_req)
        }
        self.limit_order_updates.push(limit_order_update);

        self.position.change(
            filled_qty,
            limit_price,
            side,
            &mut self.balances,
            self.config.contract_spec().init_margin_req(),
        );

        let new_order_margin = self.order_margin.order_margin(
            self.config.contract_spec().init_margin_req(),
            &self.position,
        );
        match new_order_margin.cmp(&self.balances.order_margin()) {
            Ordering::Less => {
                let margin_delta = self.balances.order_margin() - new_order_margin;
                assert2::debug_assert!(margin_delta > Zero::zero());
                self.balances.free_order_margin(margin_delta);
            }
            Ordering::Equal => {}
            Ordering::Greater => {
                let margin_delta = new_order_margin - self.balances.order_margin();
                assert2::debug_assert!(margin_delta > Zero::zero());
                let success = self.balances.try_reserve_order_margin(margin_delta);
                assert!(success, "Can reserve order margin");
            }
        }
    }
}
