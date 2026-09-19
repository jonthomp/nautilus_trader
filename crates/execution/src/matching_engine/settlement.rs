// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

use nautilus_core::{UUID4, UnixNanos};
use nautilus_model::{
    enums::{
        LiquiditySide, OptionKind, OrderSide, OrderStatus, OrderType, PositionSide, PriceType,
        TimeInForce,
    },
    events::{OrderEventAny, OrderFilled},
    identifiers::{ClientOrderId, InstrumentId, TradeId, VenueOrderId},
    instruments::{Instrument, InstrumentAny},
    orders::{MarketOrder, Order, OrderAny, OrderCore},
    position::Position,
    types::{Money, Price, Quantity},
};
use rust_decimal::Decimal;
use ustr::Ustr;
use uuid::Uuid;

use super::OrderMatchingEngine;

// FNV-1a 64-bit constants (see http://www.isthe.com/chongo/tech/comp/fnv/).
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

impl OrderMatchingEngine {
    pub(super) fn process_option_expiry(&mut self, ts_now: UnixNanos) -> anyhow::Result<bool> {
        let instrument_id = self.instrument.id();

        if !self.option_resume_pending_physical_deliveries(ts_now)? {
            return Ok(false);
        }

        let positions: Vec<Position> = {
            let cache = self.cache.borrow();
            cache
                .positions_open(None, Some(&instrument_id), None, None, None)
                .into_iter()
                .map(|p| p.cloned())
                .collect()
        };

        if positions.is_empty() {
            return Ok(true);
        }

        let underlying = match self.instrument.underlying() {
            Some(u) => u,
            None => {
                return Ok(self.option_settlement_retry(
                    "missing-underlying",
                    &format!("No underlying for option {instrument_id}"),
                ));
            }
        };
        let underlying_id = InstrumentId::from(format!("{underlying}.{}", self.venue).as_str());

        let underlying_instrument = {
            let cache = self.cache.borrow();
            cache.instrument(&underlying_id).cloned()
        };

        let underlying_instrument = match underlying_instrument {
            Some(u) => u,
            None => {
                return Ok(self.option_settlement_retry(
                    "missing-underlying-instrument",
                    &format!("No underlying instrument for option {instrument_id}"),
                ));
            }
        };

        // Resolve the underlying price by the underlying's instrument type. An index
        // is disseminated via `IndexPriceUpdate` (it does not trade), so its level is
        // held in the cache's index-price store rather than the trade/quote `price(...)`
        // store; a tradeable underlying (e.g. an equity) keeps the `Last` trade lookup.
        let underlying_price = {
            let cache = self.cache.borrow();
            if matches!(underlying_instrument, InstrumentAny::IndexInstrument(_)) {
                cache.index_price(&underlying_id).map(|ip| ip.value)
            } else {
                cache.price(&underlying_id, PriceType::Last)
            }
        };

        let underlying_price = match underlying_price {
            Some(p) => p,
            None => {
                return Ok(self.option_settlement_retry(
                    "missing-underlying-price",
                    &format!("No underlying price for option {instrument_id}"),
                ));
            }
        };

        let option_close_price = self
            .instrument_close
            .as_ref()
            .map(|close| close.close_price);
        let should_exercise = self.option_should_exercise(underlying_price);

        let plan = self.option_create_settlement_plan(
            &positions,
            &underlying_instrument,
            underlying_price,
            should_exercise,
            ts_now,
            option_close_price,
        );
        self.option_apply_settlement_plan(plan)?;
        Ok(true)
    }

    fn option_resume_pending_physical_deliveries(
        &mut self,
        ts_now: UnixNanos,
    ) -> anyhow::Result<bool> {
        if self.config.use_random_ids {
            return Ok(true);
        }

        let instrument_id = self.instrument.id();
        let positions: Vec<Position> = {
            let cache = self.cache.borrow();
            cache
                .positions(None, Some(&instrument_id), None, None, None)
                .into_iter()
                .map(|position| position.cloned())
                .collect()
        };

        for position in positions {
            // Physical settlement closes the option before opening the underlying,
            // so restart recovery must also inspect closed source positions.
            if !position.is_closed() {
                continue;
            }

            let (close_client_order_id, _, _) =
                self.option_settlement_ids(&position, "physical-close");
            let (open_client_order_id, generated_venue_order_id, trade_id) =
                self.option_settlement_ids(&position, "physical-open");
            let (close_complete, open_order) = {
                let cache = self.cache.borrow();
                let close_complete = cache
                    .order(&close_client_order_id)
                    .is_some_and(|order| order.status() == OrderStatus::Filled);
                let open_order = cache
                    .order(&open_client_order_id)
                    .map(|order| order.clone());
                (close_complete, open_order)
            };

            if !close_complete {
                continue;
            }

            let Some(open_order) = open_order else {
                continue;
            };

            if open_order.status() == OrderStatus::Filled {
                continue;
            }

            let underlying_instrument = {
                let cache = self.cache.borrow();
                cache.instrument(&open_order.instrument_id()).cloned()
            };
            let Some(underlying_instrument) = underlying_instrument else {
                return Ok(self.option_settlement_retry(
                    "missing-underlying-instrument",
                    &format!(
                        "No underlying instrument {} for pending physical option settlement {}",
                        open_order.instrument_id(),
                        open_order.client_order_id(),
                    ),
                ));
            };

            self.account_ids
                .insert(position.trader_id, position.account_id);
            let venue_order_id = open_order
                .venue_order_id()
                .unwrap_or(generated_venue_order_id);

            match open_order.status() {
                OrderStatus::Initialized => {
                    self.generate_order_accepted(&open_order, venue_order_id);
                }
                OrderStatus::Accepted => {}
                status => {
                    anyhow::bail!(
                        "cannot resume physical option settlement order {} from status {status}",
                        open_order.client_order_id(),
                    );
                }
            }

            let fill = OrderFilled::new(
                open_order.trader_id(),
                open_order.strategy_id(),
                open_order.instrument_id(),
                open_order.client_order_id(),
                venue_order_id,
                position.account_id,
                trade_id,
                open_order.order_side(),
                open_order.order_type(),
                open_order.quantity(),
                self.instrument
                    .strike_price()
                    .expect("physical option settlement requires a strike price"),
                underlying_instrument.quote_currency(),
                LiquiditySide::Taker,
                UUID4::new(),
                ts_now,
                ts_now,
                false,
                None,
                Some(Money::zero(underlying_instrument.quote_currency())),
                None,
            );
            self.dispatch_order_event(OrderEventAny::Filled(fill));
        }

        Ok(true)
    }

    fn option_settlement_retry(&mut self, reason: &'static str, message: &str) -> bool {
        if self.option_settlement_warning != Some(reason) {
            log::warn!("{message}; settlement will retry");
            self.option_settlement_warning = Some(reason);
        }
        false
    }

    fn option_create_settlement_plan(
        &self,
        positions: &[Position],
        underlying_instrument: &InstrumentAny,
        underlying_price: Price,
        should_exercise: bool,
        ts_now: UnixNanos,
        option_close_price: Option<Price>,
    ) -> OptionSettlementPlan {
        let mut legs = Vec::new();

        for position in positions {
            if should_exercise {
                self.option_plan_exercise_position(
                    &mut legs,
                    position,
                    underlying_instrument,
                    underlying_price,
                    ts_now,
                    option_close_price,
                );
            } else {
                legs.push(self.option_plan_otm_expiry(position, ts_now, option_close_price));
            }
        }
        OptionSettlementPlan { legs }
    }

    fn option_apply_settlement_plan(&mut self, plan: OptionSettlementPlan) -> anyhow::Result<()> {
        self.option_register_settlement_plan(&plan)?;

        for leg in &plan.legs {
            self.account_ids
                .insert(leg.fill.trader_id, leg.fill.account_id);
        }

        for leg in &plan.legs {
            let order = self.option_cached_settlement_order(leg)?;
            if order.status() == OrderStatus::Initialized {
                self.publish_order_initialized(&order);
            }
        }

        for leg in &plan.legs {
            let order = self.option_cached_settlement_order(leg)?;
            if order.status() == OrderStatus::Initialized {
                self.generate_order_accepted(&order, leg.fill.venue_order_id);
            }
        }

        for leg in plan.legs {
            let order = self.option_cached_settlement_order(&leg)?;
            if order.status() != OrderStatus::Filled {
                self.dispatch_order_event(OrderEventAny::Filled(leg.fill));
            }
        }

        Ok(())
    }

    fn option_register_settlement_plan(&self, plan: &OptionSettlementPlan) -> anyhow::Result<()> {
        for leg in &plan.legs {
            let client_order_id = leg.order.client_order_id();
            let mut cache = self.cache.borrow_mut();
            if let Some(existing) = cache.order(&client_order_id) {
                anyhow::ensure!(
                    existing.instrument_id() == leg.order.instrument_id()
                        && existing.order_side() == leg.order.order_side()
                        && existing.quantity() == leg.order.quantity()
                        && existing.is_reduce_only() == leg.order.is_reduce_only(),
                    "existing settlement order {client_order_id} conflicts with the recovered plan",
                );
            } else {
                cache
                    .add_order(leg.order.clone(), leg.fill.position_id, None, false)
                    .map_err(|e| {
                        anyhow::anyhow!("cannot add settlement order {client_order_id}: {e}")
                    })?;
            }
            cache
                .add_venue_order_id(&client_order_id, &leg.fill.venue_order_id, false)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "cannot claim venue order ID {} for settlement order {client_order_id}: {e}",
                        leg.fill.venue_order_id
                    )
                })?;
        }
        Ok(())
    }

    fn option_cached_settlement_order(
        &self,
        leg: &OptionSettlementLeg,
    ) -> anyhow::Result<OrderAny> {
        let client_order_id = leg.order.client_order_id();
        self.cache
            .borrow()
            .order(&client_order_id)
            .map(|order| order.clone())
            .ok_or_else(|| anyhow::anyhow!("settlement order {client_order_id} is not cached"))
    }

    fn option_should_exercise(&self, underlying_price: Price) -> bool {
        let strike = match self.instrument.strike_price() {
            Some(p) => p.as_decimal(),
            None => return false,
        };
        let spot = underlying_price.as_decimal();
        match self.instrument.option_kind() {
            Some(OptionKind::Call) => spot > strike,
            Some(OptionKind::Put) => strike > spot,
            None => false,
        }
    }

    fn option_settlement_price(&self, underlying_price: Price, cash_settled: bool) -> Price {
        let strike = self
            .instrument
            .strike_price()
            .expect("option must have strike");
        if !cash_settled {
            return strike;
        }

        let spot = underlying_price.as_decimal();
        let strike_value = strike.as_decimal();
        let value = match self.instrument.option_kind() {
            Some(OptionKind::Call) => (spot - strike_value).max(Decimal::ZERO),
            _ => (strike_value - spot).max(Decimal::ZERO),
        };
        Price::from_decimal_dp(value, strike.precision).expect("Invalid option settlement price")
    }

    fn option_plan_exercise_position(
        &self,
        legs: &mut Vec<OptionSettlementLeg>,
        position: &Position,
        underlying_instrument: &InstrumentAny,
        underlying_price: Price,
        ts_now: UnixNanos,
        option_close_price: Option<Price>,
    ) {
        if matches!(underlying_instrument, InstrumentAny::IndexInstrument(_)) {
            legs.push(self.option_plan_cash_settlement(
                position,
                underlying_price,
                ts_now,
                option_close_price,
            ));
        } else {
            legs.extend(self.option_plan_physical_settlement(
                position,
                underlying_instrument,
                underlying_price,
                ts_now,
                option_close_price,
            ));
        }
    }

    fn option_plan_cash_settlement(
        &self,
        position: &Position,
        underlying_price: Price,
        ts_now: UnixNanos,
        option_close_price: Option<Price>,
    ) -> OptionSettlementLeg {
        let venue = self.venue;
        let (client_order_id, venue_order_id, trade_id) =
            self.option_settlement_ids(position, "cash");
        let close_px = option_close_price
            .unwrap_or_else(|| self.option_settlement_price(underlying_price, true));
        let close_side = OrderCore::closing_side(position.side)
            .expect("Settlement position must be Long or Short");
        let order = self.option_create_settlement_order(
            position,
            self.instrument.id(),
            close_side,
            position.quantity,
            client_order_id,
            true,
            &format!("EXPIRATION_{venue}_CASH"),
        );
        let fill = self.option_create_close_fill(
            position,
            close_px,
            client_order_id,
            venue_order_id,
            trade_id,
            ts_now,
        );
        OptionSettlementLeg { order, fill }
    }

    fn option_plan_physical_settlement(
        &self,
        position: &Position,
        underlying_instrument: &InstrumentAny,
        underlying_price: Price,
        ts_now: UnixNanos,
        option_close_price: Option<Price>,
    ) -> [OptionSettlementLeg; 2] {
        let multiplier = self.instrument.multiplier();
        let underlying_qty = Quantity::from_decimal_dp(
            position.quantity.as_decimal() * multiplier.as_decimal(),
            underlying_instrument.size_precision(),
        )
        .expect("Invalid underlying settlement quantity");

        let underlying_side = if self.instrument.option_kind() == Some(OptionKind::Call) {
            position.side
        } else {
            match position.side {
                PositionSide::Long => PositionSide::Short,
                PositionSide::Short => PositionSide::Long,
                other => other,
            }
        };

        let venue = self.venue;
        let (close_client_order_id, close_venue_order_id, close_trade_id) =
            self.option_settlement_ids(position, "physical-close");
        let (open_client_order_id, open_venue_order_id, open_trade_id) =
            self.option_settlement_ids(position, "physical-open");
        let settlement_px = self.option_settlement_price(underlying_price, false);
        let option_close_px =
            option_close_price.unwrap_or_else(|| Price::zero(self.instrument.price_precision()));
        let close_side = OrderCore::closing_side(position.side)
            .expect("Settlement position must be Long or Short");
        let underlying_order_side = match underlying_side {
            PositionSide::Long => OrderSide::Buy,
            _ => OrderSide::Sell,
        };

        let close_order = self.option_create_settlement_order(
            position,
            self.instrument.id(),
            close_side,
            position.quantity,
            close_client_order_id,
            true,
            &format!("EXPIRATION_{venue}_PHYSICAL_CLOSE"),
        );
        let open_order = self.option_create_settlement_order(
            position,
            underlying_instrument.id(),
            underlying_order_side,
            underlying_qty,
            open_client_order_id,
            false,
            &format!("EXPIRATION_{venue}_PHYSICAL_OPEN"),
        );

        let option_fill = self.option_create_close_fill(
            position,
            option_close_px,
            close_client_order_id,
            close_venue_order_id,
            close_trade_id,
            ts_now,
        );
        let underlying_fill = self.option_create_underlying_fill(
            position,
            underlying_instrument,
            underlying_qty,
            underlying_side,
            settlement_px,
            open_client_order_id,
            open_venue_order_id,
            open_trade_id,
            ts_now,
        );
        [
            OptionSettlementLeg {
                order: close_order,
                fill: option_fill,
            },
            OptionSettlementLeg {
                order: open_order,
                fill: underlying_fill,
            },
        ]
    }

    fn option_plan_otm_expiry(
        &self,
        position: &Position,
        ts_now: UnixNanos,
        option_close_price: Option<Price>,
    ) -> OptionSettlementLeg {
        let venue = self.venue;
        let (client_order_id, venue_order_id, trade_id) =
            self.option_settlement_ids(position, "otm");
        let close_px =
            option_close_price.unwrap_or_else(|| Price::zero(self.instrument.price_precision()));
        let close_side = OrderCore::closing_side(position.side)
            .expect("Settlement position must be Long or Short");
        let order = self.option_create_settlement_order(
            position,
            self.instrument.id(),
            close_side,
            position.quantity,
            client_order_id,
            true,
            &format!("EXPIRATION_{venue}_OTM"),
        );
        let fill = self.option_create_close_fill(
            position,
            close_px,
            client_order_id,
            venue_order_id,
            trade_id,
            ts_now,
        );
        OptionSettlementLeg { order, fill }
    }

    fn option_settlement_ids(
        &self,
        position: &Position,
        leg_role: &str,
    ) -> (ClientOrderId, VenueOrderId, TradeId) {
        let venue = self.venue;

        if self.config.use_random_ids {
            return (
                ClientOrderId::from(format!("EXPIRATION-{venue}-{}", UUID4::new())),
                VenueOrderId::from(format!("EXPIRATION-{venue}-{}", UUID4::new())),
                TradeId::from(UUID4::new().to_string()),
            );
        }

        let client_uuid = self.option_settlement_uuid(position, leg_role, "client-order");
        let venue_uuid = self.option_settlement_uuid(position, leg_role, "venue-order");
        let trade_uuid = self.option_settlement_uuid(position, leg_role, "trade");
        (
            ClientOrderId::from(format!("EXPIRATION-{venue}-{client_uuid}")),
            VenueOrderId::from(format!("EXPIRATION-{venue}-{venue_uuid}")),
            TradeId::from(trade_uuid),
        )
    }

    fn option_settlement_uuid(&self, position: &Position, leg_role: &str, id_role: &str) -> String {
        let instrument_id = self.instrument.id().to_string();
        let expiration_ns = self
            .instrument
            .expiration_ns()
            .expect("option settlement requires an expiration timestamp")
            .as_u64()
            .to_le_bytes();
        let ts_opened = position.ts_opened.as_u64().to_le_bytes();

        // Side, quantity and status can change when the close fill is applied,
        // so the restart key contains only immutable settlement identity fields.
        let parts = [
            position.account_id.as_str().as_bytes(),
            position.trader_id.as_str().as_bytes(),
            position.strategy_id.as_str().as_bytes(),
            instrument_id.as_bytes(),
            position.id.as_str().as_bytes(),
            position.opening_order_id.as_str().as_bytes(),
            &ts_opened,
            &expiration_ns,
            leg_role.as_bytes(),
            id_role.as_bytes(),
        ];
        deterministic_option_settlement_uuid(&parts)
    }

    #[expect(clippy::too_many_arguments)]
    fn option_create_settlement_order(
        &self,
        position: &Position,
        instrument_id: InstrumentId,
        order_side: OrderSide,
        quantity: Quantity,
        client_order_id: ClientOrderId,
        reduce_only: bool,
        tag: &str,
    ) -> OrderAny {
        let ts_now = self.clock.borrow().timestamp_ns();
        OrderAny::Market(MarketOrder::new(
            position.trader_id,
            position.strategy_id,
            instrument_id,
            client_order_id,
            order_side,
            quantity,
            TimeInForce::Gtc,
            UUID4::new(),
            ts_now,
            reduce_only,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(vec![Ustr::from(tag)]),
        ))
    }

    fn option_create_close_fill(
        &self,
        position: &Position,
        price: Price,
        client_order_id: ClientOrderId,
        venue_order_id: VenueOrderId,
        trade_id: TradeId,
        ts_now: UnixNanos,
    ) -> OrderFilled {
        let close_side = OrderCore::closing_side(position.side)
            .expect("Settlement position must be Long or Short");
        OrderFilled::new(
            position.trader_id,
            position.strategy_id,
            self.instrument.id(),
            client_order_id,
            venue_order_id,
            position.account_id,
            trade_id,
            close_side,
            OrderType::Market,
            position.quantity,
            price,
            self.instrument.quote_currency(),
            LiquiditySide::Taker,
            UUID4::new(),
            ts_now,
            ts_now,
            false,
            Some(position.id),
            Some(Money::zero(self.instrument.quote_currency())),
            None,
        )
    }

    #[expect(clippy::too_many_arguments)]
    fn option_create_underlying_fill(
        &self,
        position: &Position,
        underlying_instrument: &InstrumentAny,
        quantity: Quantity,
        side: PositionSide,
        price: Price,
        client_order_id: ClientOrderId,
        venue_order_id: VenueOrderId,
        trade_id: TradeId,
        ts_now: UnixNanos,
    ) -> OrderFilled {
        let order_side = match side {
            PositionSide::Long => OrderSide::Buy,
            PositionSide::Short => OrderSide::Sell,
            PositionSide::Flat => {
                unreachable!("flat position cannot create an underlying settlement fill")
            }
        };
        OrderFilled::new(
            position.trader_id,
            position.strategy_id,
            underlying_instrument.id(),
            client_order_id,
            venue_order_id,
            position.account_id,
            trade_id,
            order_side,
            OrderType::Market,
            quantity,
            price,
            underlying_instrument.quote_currency(),
            LiquiditySide::Taker,
            UUID4::new(),
            ts_now,
            ts_now,
            false,
            None,
            Some(Money::zero(underlying_instrument.quote_currency())),
            None,
        )
    }
}

struct OptionSettlementLeg {
    order: OrderAny,
    fill: OrderFilled,
}

struct OptionSettlementPlan {
    legs: Vec<OptionSettlementLeg>,
}

fn deterministic_option_settlement_uuid(parts: &[&[u8]]) -> String {
    let primary = option_settlement_hash(b"option-settlement-v1", parts);
    let secondary = option_settlement_hash(b"option-settlement-v1-alt", parts);
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&primary.to_be_bytes());
    bytes[8..].copy_from_slice(&secondary.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    Uuid::from_bytes(bytes).to_string()
}

fn option_settlement_hash(namespace: &[u8], parts: &[&[u8]]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    update_option_settlement_hash(&mut hash, namespace);

    for part in parts {
        update_option_settlement_hash(&mut hash, part);
    }

    hash
}

fn update_option_settlement_hash(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }

    *hash ^= 0xff;
    *hash = hash.wrapping_mul(FNV_PRIME);
}
