use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    rc::Rc,
    time::{Duration, Instant},
};

use chrono::Utc;
use rand::Rng;
use thiserror::Error;
use tracing::{debug, error, info};

use crate::{
    depth::{L2MarketDepth, MarketDepth},
    live::{
        ipc::{IceoryxPubSubBot, LiveEventExt, PubSubList},
        Channel,
        Instrument,
    },
    types::{
        Bot,
        BuildError,
        Event,
        LiveError as ErrorEvent,
        LiveError,
        LiveEvent,
        OrdType,
        Order,
        OrderId,
        OrderRequest,
        Request,
        Side,
        StateValues,
        Status,
        TimeInForce,
        WaitOrderResponse,
        LOCAL_ASK_DEPTH_EVENT,
        LOCAL_BID_DEPTH_EVENT,
        LOCAL_BUY_TRADE_EVENT,
        LOCAL_SELL_TRADE_EVENT,
    },
};

#[derive(Error, Debug)]
pub enum BotError {
    #[error("OrderIdExist")]
    OrderIdExist,
    #[error("AssetNotFound")]
    InstrumentNotFound,
    #[error("OrderNotFound")]
    OrderNotFound,
    #[error("InvalidOrderStatus")]
    InvalidOrderStatus,
    #[error("Timeout")]
    Timeout,
    #[error("Interrupted")]
    Interrupted,
    #[error("Custom: {0}")]
    Custom(String),
}

pub type ErrorHandler = Box<dyn Fn(ErrorEvent) -> Result<(), BotError>>;
pub type OrderRecvHook = Box<dyn Fn(&Order, &Order) -> Result<(), BotError>>;

fn generate_random_id() -> u64 {
    // Initialize the random number generator
    let mut rng = rand::thread_rng();

    // Generate a random u64 value
    rng.gen::<u64>()
}

/// Live [`LiveBot`] builder.
pub struct LiveBotBuilder<MD> {
    id: u64,
    instruments: Vec<Instrument<MD>>,
    error_handler: Option<ErrorHandler>,
    order_hook: Option<OrderRecvHook>,
}

impl<MD> LiveBotBuilder<MD> {
    /// Adds an instrument.
    ///
    /// * `name` - Name of the [`Connector`], which is registered by
    ///            [`register()`](`LiveBotBuilder::register()`), through which this asset will be
    ///            traded.
    /// * `symbol` - Symbol of the asset. You need to check with the [`Connector`] which symbology
    ///              is used.
    /// * `tick_size` - The minimum price fluctuation.
    /// * `lot_size` -  The minimum trade size.
    /// * `depth` -  The market depth.
    pub fn add(self, instrument: Instrument<MD>) -> Self {
        Self {
            instruments: {
                let mut instruments = self.instruments;
                instruments.push(instrument);
                instruments
            },
            ..self
        }
    }

    /// Registers the error handler to deal with an error from connectors.
    pub fn error_handler<Handler>(self, handler: Handler) -> Self
    where
        Handler: Fn(LiveError) -> Result<(), BotError> + 'static,
    {
        Self {
            error_handler: Some(Box::new(handler)),
            ..self
        }
    }

    /// Registers the order response receive hook.
    pub fn order_recv_hook<Hook>(self, hook: Hook) -> Self
    where
        Hook: Fn(&Order, &Order) -> Result<(), BotError> + 'static,
    {
        Self {
            order_hook: Some(Box::new(hook)),
            ..self
        }
    }

    /// Sets the bot ID. It must be unique among all bots connected to the same `Connector`.
    pub fn id(self, id: u64) -> Self {
        Self { id, ..self }
    }

    /// Builds a live [`LiveBot`] based on the registered connectors and assets.
    pub fn build(self) -> Result<LiveBot<MD>, BuildError> {
        let mut dup = HashSet::new();
        let mut tmp_pubsub: HashMap<String, Rc<IceoryxPubSubBot<Request, LiveEventExt>>> =
            HashMap::new();
        let mut pubsub = Vec::new();
        for instrument in self.instruments.iter() {
            if !dup.insert(format!(
                "{}/{}",
                instrument.connector_name, instrument.symbol
            )) {
                Err(BuildError::Duplicate(
                    instrument.connector_name.clone(),
                    instrument.symbol.clone(),
                ))?;
            }

            match tmp_pubsub.entry(instrument.connector_name.clone()) {
                Entry::Occupied(entry) => {
                    let ps = entry.get().clone();
                    pubsub.push(ps);
                }
                Entry::Vacant(entry) => {
                    let ps = Rc::new(IceoryxPubSubBot::new(&instrument.connector_name)?);
                    entry.insert(ps.clone());
                    pubsub.push(ps);
                }
            }
        }

        let asset_name_to_no: HashMap<_, _> = self
            .instruments
            .iter()
            .enumerate()
            .map(|(asset_no, instrument)| (instrument.symbol.clone(), asset_no))
            .collect();

        let id = self.id;

        // Requests to prepare a given asset for trading.
        // The Connector will send the current orders on this asset.
        for instrument in &self.instruments {
            info!(
                connector_name = instrument.connector_name,
                symbol = instrument.symbol,
                "Prepares the instrument."
            );
            let asset_no = asset_name_to_no.get(&instrument.symbol).unwrap();
            let ps = pubsub.get(*asset_no).unwrap();
            ps.send(
                id,
                &Request::AddInstrument {
                    symbol: instrument.symbol.clone(),
                    tick_size: instrument.tick_size,
                },
            )
            .map_err(|error| BuildError::Error(anyhow::Error::from(error)))?;
        }

        let pubsub = PubSubList::new(pubsub)
            .map_err(|error| BuildError::Error(anyhow::Error::from(error)))?;

        Ok(LiveBot {
            id,
            pubsub,
            instruments: self.instruments,
            symbol_to_inst_no: asset_name_to_no,
            error_handler: self.error_handler,
            order_hook: self.order_hook,
        })
    }
}

/// A live trading bot.
///
/// Provides the same interface as the backtesters in [`backtest`](`crate::backtest`).
///
/// ```
/// use hftbacktest::{live::{Instrument, LiveBot}, prelude::HashMapMarketDepth};
///
/// let tick_size = 0.1;
/// let lot_size = 1.0;
///
/// let mut hbt = LiveBot::builder()
///     .add(Instrument::new(
///         "connector_name",
///         "symbol",
///         tick_size,
///         lot_size,
///         HashMapMarketDepth::new(tick_size, lot_size),
///         0
///     ))
///     .build()
///     .unwrap();
/// ```
pub struct LiveBot<MD> {
    id: u64,
    pubsub: PubSubList,
    instruments: Vec<Instrument<MD>>,
    symbol_to_inst_no: HashMap<String, usize>,
    error_handler: Option<ErrorHandler>,
    order_hook: Option<OrderRecvHook>,
}

impl<MD> LiveBot<MD>
where
    MD: MarketDepth + L2MarketDepth,
{
    /// Builder to construct [`LiveBot`] instances.
    pub fn builder() -> LiveBotBuilder<MD> {
        LiveBotBuilder {
            id: generate_random_id(),
            instruments: Vec::new(),
            error_handler: None,
            order_hook: None,
        }
    }

    fn process_event<const WAIT_NEXT_FEED: bool>(
        &mut self,
        ev: LiveEvent,
        wait_order_response: WaitOrderResponse,
    ) -> Result<bool, BotError> {
        match ev {
            LiveEvent::Feed { symbol, event } => {
                let Some(&asset_no) = self.symbol_to_inst_no.get(&symbol) else {
                    return Ok(false);
                };

                let instrument = unsafe { self.instruments.get_unchecked_mut(asset_no) };
                instrument.last_feed_latency = Some((event.exch_ts, event.local_ts));
                if event.is(LOCAL_BID_DEPTH_EVENT) {
                    instrument
                        .depth
                        .update_bid_depth(event.px, event.qty, event.exch_ts);
                } else if event.is(LOCAL_ASK_DEPTH_EVENT) {
                    instrument
                        .depth
                        .update_ask_depth(event.px, event.qty, event.exch_ts);
                } else if event.is(LOCAL_BUY_TRADE_EVENT) || event.is(LOCAL_SELL_TRADE_EVENT) {
                    if instrument.last_trades.capacity() > 0 {
                        instrument.last_trades.push(event);
                    }
                }
            }
            LiveEvent::Order { symbol, order } => {
                let Some(&asset_no) = self.symbol_to_inst_no.get(&symbol) else {
                    return Ok(false);
                };

                debug!(%asset_no, ?order, "Event::Order");
                let received_order_resp = match wait_order_response {
                    WaitOrderResponse::Any => true,
                    WaitOrderResponse::Specified {
                        asset_no: wait_order_asset_no,
                        order_id: wait_order_id,
                    } if wait_order_id == order.order_id && wait_order_asset_no == asset_no => true,
                    _ => false,
                };
                let instrument = unsafe { self.instruments.get_unchecked_mut(asset_no) };
                instrument.last_order_latency = Some((
                    order.local_timestamp,
                    order.exch_timestamp,
                    Utc::now().timestamp_nanos_opt().unwrap(),
                ));
                match instrument.orders.entry(order.order_id) {
                    Entry::Occupied(mut entry) => {
                        let ex_order = entry.get_mut();
                        if let Some(hook) = self.order_hook.as_mut() {
                            hook(ex_order, &order)?;
                        }
                        if order.exch_timestamp >= ex_order.exch_timestamp {
                            if ex_order.status == Status::Canceled
                                || ex_order.status == Status::Expired
                                || ex_order.status == Status::Filled
                            {
                                // Ignores the update since the current status is the final status.
                            } else {
                                ex_order.update(&order);
                            }
                        }
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(order);
                    }
                }
                if received_order_resp {
                    return Ok(true);
                }
            }
            LiveEvent::Position { symbol, qty } => {
                let Some(&asset_no) = self.symbol_to_inst_no.get(&symbol) else {
                    return Ok(false);
                };

                unsafe { self.instruments.get_unchecked_mut(asset_no) }
                    .state
                    .position = qty;
            }
            LiveEvent::Error(error) => {
                if let Some(handler) = self.error_handler.as_mut() {
                    handler(error)?;
                }
            }
        }
        Ok(false)
    }

    fn elapse_<const WAIT_NEXT_FEED: bool>(
        &mut self,
        duration: i64,
        wait_order_response: WaitOrderResponse,
    ) -> Result<bool, BotError> {
        let instant = Instant::now();
        let duration = Duration::from_nanos(duration as u64);
        let mut remaining_duration = duration;
        let mut in_batch = false;
        let mut receive_wait_resp = false;

        loop {
            match self.pubsub.recv_timeout(self.id, remaining_duration) {
                Ok(LiveEventExt::Normal(ev)) => {
                    if self.process_event::<WAIT_NEXT_FEED>(ev, wait_order_response)? {
                        receive_wait_resp = true;
                        if !in_batch {
                            return Ok(true);
                        }
                    }
                }
                Ok(LiveEventExt::Batch(ev)) => {
                    in_batch = true;
                    if self.process_event::<WAIT_NEXT_FEED>(ev, wait_order_response)? {
                        receive_wait_resp = true;
                    }
                }
                Ok(LiveEventExt::EndOfBatch) => {
                    in_batch = false;
                    if receive_wait_resp {
                        return Ok(true);
                    }
                }
                Err(BotError::Timeout) => {
                    return Ok(true);
                }
                Err(BotError::Interrupted) => {
                    return Ok(false);
                }
                Err(error) => {
                    return Err(error);
                }
            }
            if !in_batch {
                let elapsed = instant.elapsed();
                if elapsed > duration {
                    return Ok(true);
                }
                remaining_duration = duration - elapsed;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_order(
        &mut self,
        asset_no: usize,
        order_id: u64,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
        side: Side,
    ) -> Result<bool, BotError> {
        let instrument = self
            .instruments
            .get_mut(asset_no)
            .ok_or(BotError::InstrumentNotFound)?;
        if instrument.orders.contains_key(&order_id) {
            return Err(BotError::OrderIdExist);
        }
        let symbol = instrument.symbol.clone();
        let tick_size = instrument.tick_size;
        let order = Order {
            order_id,
            price_tick: (price / tick_size).round() as i64,
            qty,
            leaves_qty: qty,
            tick_size,
            side,
            time_in_force,
            order_type,
            status: Status::New,
            local_timestamp: Utc::now().timestamp_nanos_opt().unwrap(),
            req: Status::New,
            exec_price_tick: 0,
            exch_timestamp: 0,
            exec_qty: 0.0,
            // Invalid information
            q: Box::new(()),
            maker: false,
        };
        let order_id = order.order_id;
        instrument.orders.insert(order_id, order.clone());

        self.pubsub
            .send(asset_no, Request::Order { symbol, order })?;

        if wait {
            // fixme: timeout should be specified by the argument.
            return self.wait_order_response(asset_no, order_id, 60_000_000_000);
        }
        Ok(true)
    }
}

impl<MD> Bot<MD> for LiveBot<MD>
where
    MD: MarketDepth + L2MarketDepth,
{
    type Error = BotError;

    #[inline]
    fn current_timestamp(&self) -> i64 {
        Utc::now().timestamp_nanos_opt().unwrap()
    }

    #[inline]
    fn num_assets(&self) -> usize {
        self.instruments.len()
    }

    #[inline]
    fn position(&self, asset_no: usize) -> f64 {
        self.state_values(asset_no).position
    }

    #[inline]
    fn state_values(&self, asset_no: usize) -> &StateValues {
        // todo: implement the missing fields. Trade values need to be changed to a rolling manner,
        //       unlike the current Python implementation, to support live trading.
        &self.instruments.get(asset_no).unwrap().state
    }

    #[inline]
    fn depth(&self, asset_no: usize) -> &MD {
        &self.instruments.get(asset_no).unwrap().depth
    }

    #[inline]
    fn last_trades(&self, asset_no: usize) -> &[Event] {
        self.instruments
            .get(asset_no)
            .unwrap()
            .last_trades
            .as_slice()
    }

    fn clear_last_trades(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(asset_no) => {
                self.instruments
                    .get_mut(asset_no)
                    .unwrap()
                    .last_trades
                    .clear();
            }
            None => {
                for asset_no in 0..self.instruments.len() {
                    self.instruments
                        .get_mut(asset_no)
                        .unwrap()
                        .last_trades
                        .clear();
                }
            }
        }
    }

    #[inline]
    fn orders(&self, asset_no: usize) -> &HashMap<OrderId, Order> {
        &self.instruments.get(asset_no).unwrap().orders
    }

    #[inline]
    fn submit_buy_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        self.submit_order(
            asset_no,
            order_id,
            price,
            qty,
            time_in_force,
            order_type,
            wait,
            Side::Buy,
        )
    }

    #[inline]
    fn submit_sell_order(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        price: f64,
        qty: f64,
        time_in_force: TimeInForce,
        order_type: OrdType,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        self.submit_order(
            asset_no,
            order_id,
            price,
            qty,
            time_in_force,
            order_type,
            wait,
            Side::Sell,
        )
    }

    fn submit_order(
        &mut self,
        asset_no: usize,
        order: OrderRequest,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        self.submit_order(
            asset_no,
            order.order_id,
            order.price,
            order.qty,
            order.time_in_force,
            order.order_type,
            wait,
            order.side,
        )
    }

    #[inline]
    fn cancel(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        wait: bool,
    ) -> Result<bool, Self::Error> {
        let instrument = self
            .instruments
            .get_mut(asset_no)
            .ok_or(BotError::InstrumentNotFound)?;
        let symbol = instrument.symbol.clone();
        let order = instrument
            .orders
            .get_mut(&order_id)
            .ok_or(BotError::OrderNotFound)?;
        if !order.cancellable() {
            return Err(BotError::InvalidOrderStatus);
        }
        order.req = Status::Canceled;
        order.local_timestamp = Utc::now().timestamp_nanos_opt().unwrap();

        self.pubsub.send(
            asset_no,
            Request::Order {
                symbol,
                order: order.clone(),
            },
        )?;

        if wait {
            // fixme: timeout should be specified by the argument.
            return self.wait_order_response(asset_no, order_id, 60_000_000_000);
        }
        Ok(true)
    }

    #[inline]
    fn clear_inactive_orders(&mut self, asset_no: Option<usize>) {
        match asset_no {
            Some(inst_no) => {
                if let Some(instrument) = self.instruments.get_mut(inst_no) {
                    instrument.orders.retain(|_, order| order.active());
                }
            }
            None => {
                for instrument in self.instruments.iter_mut() {
                    instrument.orders.retain(|_, order| order.active());
                }
            }
        }
    }

    #[inline]
    fn wait_order_response(
        &mut self,
        asset_no: usize,
        order_id: OrderId,
        timeout: i64,
    ) -> Result<bool, Self::Error> {
        self.elapse_::<false>(timeout, WaitOrderResponse::Specified { asset_no, order_id })
    }

    #[inline]
    fn wait_next_feed(
        &mut self,
        include_order_resp: bool,
        timeout: i64,
    ) -> Result<bool, Self::Error> {
        if include_order_resp {
            self.elapse_::<true>(timeout, WaitOrderResponse::Any)
        } else {
            self.elapse_::<true>(timeout, WaitOrderResponse::None)
        }
    }

    #[inline]
    fn elapse(&mut self, duration: i64) -> Result<bool, Self::Error> {
        self.elapse_::<false>(duration, WaitOrderResponse::None)
    }

    #[inline]
    fn elapse_bt(&mut self, _duration: i64) -> Result<bool, Self::Error> {
        Ok(true)
    }

    fn close(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn feed_latency(&self, asset_no: usize) -> Option<(i64, i64)> {
        self.instruments.get(asset_no).unwrap().last_feed_latency
    }

    fn order_latency(&self, asset_no: usize) -> Option<(i64, i64, i64)> {
        self.instruments.get(asset_no).unwrap().last_order_latency
    }
}
