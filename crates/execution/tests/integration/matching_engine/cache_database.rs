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

use std::sync::Arc;

use ahash::AHashMap;
use bytes::Bytes;
use nautilus_common::{
    cache::database::{CacheDatabaseAdapter, CacheMap},
    signal::Signal,
};
use nautilus_core::UnixNanos;
use nautilus_model::{
    accounts::AccountAny,
    data::{
        Bar, CustomData, DataType, FundingRateUpdate, InstrumentClose, QuoteTick, TradeTick,
        greeks::{GreeksData, YieldCurveData},
    },
    events::{OrderEventAny, OrderSnapshot, position::snapshot::PositionSnapshot},
    identifiers::{
        AccountId, ActorId, ClientId, ClientOrderId, InstrumentId, PositionId, StrategyId,
        VenueOrderId,
    },
    instruments::{Instrument, InstrumentAny, SyntheticInstrument},
    orderbook::OrderBook,
    orders::{Order, OrderAny},
    position::Position,
    types::{Currency, Money},
};
use parking_lot::Mutex;
use ustr::Ustr;

#[derive(Debug, Default)]
struct FailNthAddOrderState {
    fail_add_order_on: Option<usize>,
    fail_index_order_position: bool,
    add_order_calls: usize,
    general: AHashMap<String, Bytes>,
    instruments: AHashMap<InstrumentId, InstrumentAny>,
    instrument_closes: AHashMap<InstrumentId, InstrumentClose>,
    synthetics: AHashMap<InstrumentId, SyntheticInstrument>,
    accounts: AHashMap<AccountId, AccountAny>,
    orders: AHashMap<ClientOrderId, OrderAny>,
    positions: AHashMap<PositionId, Position>,
    order_position: AHashMap<ClientOrderId, PositionId>,
    order_client: AHashMap<ClientOrderId, ClientId>,
    order_snapshots: Vec<OrderSnapshot>,
    position_snapshots: Vec<PositionSnapshot>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct FailNthAddOrderDatabaseControl {
    state: Arc<Mutex<FailNthAddOrderState>>,
}

impl FailNthAddOrderDatabaseControl {
    pub(super) fn set_fail_add_order_on(&self, call: Option<usize>) {
        let mut state = self.state.lock();
        state.fail_add_order_on = call;
        state.add_order_calls = 0;
    }

    pub(super) fn set_fail_index_order_position(&self, fail: bool) {
        self.state.lock().fail_index_order_position = fail;
    }

    pub(super) fn database(&self) -> FailNthAddOrderDatabase {
        FailNthAddOrderDatabase {
            control: self.clone(),
        }
    }

    #[allow(dead_code, reason = "used by the sibling exec_engine test module")]
    pub(super) fn order_snapshots(&self) -> Vec<OrderSnapshot> {
        self.state.lock().order_snapshots.clone()
    }

    #[allow(dead_code, reason = "used by the sibling exec_engine test module")]
    pub(super) fn position_snapshots(&self) -> Vec<PositionSnapshot> {
        self.state.lock().position_snapshots.clone()
    }
}

#[derive(Debug)]
pub(super) struct FailNthAddOrderDatabase {
    control: FailNthAddOrderDatabaseControl,
}

impl FailNthAddOrderDatabase {
    pub(super) fn create() -> (Self, FailNthAddOrderDatabaseControl) {
        let control = FailNthAddOrderDatabaseControl::default();
        (
            Self {
                control: control.clone(),
            },
            control,
        )
    }
}

#[async_trait::async_trait]
impl CacheDatabaseAdapter for FailNthAddOrderDatabase {
    fn close(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn flush(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn load_all(&self) -> anyhow::Result<CacheMap> {
        let state = self.control.state.lock();
        Ok(CacheMap {
            currencies: AHashMap::new(),
            instruments: state.instruments.clone(),
            instrument_closes: state.instrument_closes.clone(),
            synthetics: state.synthetics.clone(),
            accounts: state.accounts.clone(),
            orders: state.orders.clone(),
            positions: state.positions.clone(),
            greeks: AHashMap::new(),
            yield_curves: AHashMap::new(),
        })
    }

    fn load(&self) -> anyhow::Result<AHashMap<String, Bytes>> {
        Ok(self.control.state.lock().general.clone())
    }

    async fn load_currencies(&self) -> anyhow::Result<AHashMap<Ustr, Currency>> {
        Ok(AHashMap::new())
    }

    async fn load_instruments(&self) -> anyhow::Result<AHashMap<InstrumentId, InstrumentAny>> {
        Ok(self.control.state.lock().instruments.clone())
    }

    async fn load_instrument_closes(
        &self,
    ) -> anyhow::Result<AHashMap<InstrumentId, InstrumentClose>> {
        Ok(self.control.state.lock().instrument_closes.clone())
    }

    async fn load_synthetics(&self) -> anyhow::Result<AHashMap<InstrumentId, SyntheticInstrument>> {
        Ok(self.control.state.lock().synthetics.clone())
    }

    async fn load_accounts(&self) -> anyhow::Result<AHashMap<AccountId, AccountAny>> {
        Ok(self.control.state.lock().accounts.clone())
    }

    async fn load_orders(&self) -> anyhow::Result<AHashMap<ClientOrderId, OrderAny>> {
        Ok(self.control.state.lock().orders.clone())
    }

    async fn load_positions(&self) -> anyhow::Result<AHashMap<PositionId, Position>> {
        Ok(self.control.state.lock().positions.clone())
    }

    fn load_index_order_position(&self) -> anyhow::Result<AHashMap<ClientOrderId, PositionId>> {
        Ok(self.control.state.lock().order_position.clone())
    }

    fn load_index_order_client(&self) -> anyhow::Result<AHashMap<ClientOrderId, ClientId>> {
        Ok(self.control.state.lock().order_client.clone())
    }

    async fn load_currency(&self, _code: &Ustr) -> anyhow::Result<Option<Currency>> {
        Ok(None)
    }

    async fn load_instrument(
        &self,
        instrument_id: &InstrumentId,
    ) -> anyhow::Result<Option<InstrumentAny>> {
        Ok(self
            .control
            .state
            .lock()
            .instruments
            .get(instrument_id)
            .cloned())
    }

    async fn load_synthetic(
        &self,
        instrument_id: &InstrumentId,
    ) -> anyhow::Result<Option<SyntheticInstrument>> {
        Ok(self
            .control
            .state
            .lock()
            .synthetics
            .get(instrument_id)
            .cloned())
    }

    async fn load_account(&self, account_id: &AccountId) -> anyhow::Result<Option<AccountAny>> {
        Ok(self.control.state.lock().accounts.get(account_id).cloned())
    }

    async fn load_order(
        &self,
        client_order_id: &ClientOrderId,
    ) -> anyhow::Result<Option<OrderAny>> {
        Ok(self
            .control
            .state
            .lock()
            .orders
            .get(client_order_id)
            .cloned())
    }

    async fn load_position(&self, position_id: &PositionId) -> anyhow::Result<Option<Position>> {
        Ok(self
            .control
            .state
            .lock()
            .positions
            .get(position_id)
            .cloned())
    }

    fn load_actor(&self, _actor_id: &ActorId) -> anyhow::Result<AHashMap<String, Bytes>> {
        Ok(AHashMap::new())
    }

    fn load_strategy(&self, _strategy_id: &StrategyId) -> anyhow::Result<AHashMap<String, Bytes>> {
        Ok(AHashMap::new())
    }

    fn load_signals(&self, _name: &str) -> anyhow::Result<Vec<Signal>> {
        Ok(Vec::new())
    }

    fn load_custom_data(&self, _data_type: &DataType) -> anyhow::Result<Vec<CustomData>> {
        Ok(Vec::new())
    }

    fn load_order_snapshot(
        &self,
        _client_order_id: &ClientOrderId,
    ) -> anyhow::Result<Option<OrderSnapshot>> {
        Ok(None)
    }

    fn load_position_snapshot(
        &self,
        _position_id: &PositionId,
    ) -> anyhow::Result<Option<PositionSnapshot>> {
        Ok(None)
    }

    fn load_quotes(&self, _instrument_id: &InstrumentId) -> anyhow::Result<Vec<QuoteTick>> {
        Ok(Vec::new())
    }

    fn load_trades(&self, _instrument_id: &InstrumentId) -> anyhow::Result<Vec<TradeTick>> {
        Ok(Vec::new())
    }

    fn load_funding_rates(
        &self,
        _instrument_id: &InstrumentId,
    ) -> anyhow::Result<Vec<FundingRateUpdate>> {
        Ok(Vec::new())
    }

    fn load_bars(&self, _instrument_id: &InstrumentId) -> anyhow::Result<Vec<Bar>> {
        Ok(Vec::new())
    }

    fn add(&self, key: String, value: Bytes) -> anyhow::Result<()> {
        self.control.state.lock().general.insert(key, value);
        Ok(())
    }

    fn add_currency(&self, _currency: &Currency) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_instrument(&self, instrument: &InstrumentAny) -> anyhow::Result<()> {
        self.control
            .state
            .lock()
            .instruments
            .insert(instrument.id(), instrument.clone());
        Ok(())
    }

    fn add_instrument_close(&self, close: &InstrumentClose) -> anyhow::Result<()> {
        self.control
            .state
            .lock()
            .instrument_closes
            .insert(close.instrument_id, *close);
        Ok(())
    }

    fn add_synthetic(&self, synthetic: &SyntheticInstrument) -> anyhow::Result<()> {
        self.control
            .state
            .lock()
            .synthetics
            .insert(synthetic.id, synthetic.clone());
        Ok(())
    }

    fn add_account(&self, account: &AccountAny) -> anyhow::Result<()> {
        self.control
            .state
            .lock()
            .accounts
            .insert(account.id(), account.clone());
        Ok(())
    }

    fn add_order(&self, order: &OrderAny, client_id: Option<ClientId>) -> anyhow::Result<()> {
        let mut state = self.control.state.lock();
        state.add_order_calls += 1;
        if state.fail_add_order_on == Some(state.add_order_calls) {
            anyhow::bail!("test add order failure");
        }
        state.orders.insert(order.client_order_id(), order.clone());
        if let Some(client_id) = client_id {
            state
                .order_client
                .insert(order.client_order_id(), client_id);
        }
        Ok(())
    }

    fn add_order_snapshot(&self, _snapshot: &OrderSnapshot) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_position(&self, position: &Position) -> anyhow::Result<()> {
        self.control
            .state
            .lock()
            .positions
            .insert(position.id, position.clone());
        Ok(())
    }

    fn add_position_snapshot(&self, _snapshot: &PositionSnapshot) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_order_book(&self, _order_book: &OrderBook) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_signal(&self, _signal: &Signal) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_custom_data(&self, _data: &CustomData) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_quote(&self, _quote: &QuoteTick) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_trade(&self, _trade: &TradeTick) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_funding_rate(&self, _funding_rate: &FundingRateUpdate) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_bar(&self, _bar: &Bar) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_greeks(&self, _greeks: &GreeksData) -> anyhow::Result<()> {
        Ok(())
    }

    fn add_yield_curve(&self, _yield_curve: &YieldCurveData) -> anyhow::Result<()> {
        Ok(())
    }

    fn delete_actor(&self, _actor_id: &ActorId) -> anyhow::Result<()> {
        Ok(())
    }

    fn delete_strategy(&self, _component_id: &StrategyId) -> anyhow::Result<()> {
        Ok(())
    }

    fn delete_order(&self, client_order_id: &ClientOrderId) -> anyhow::Result<()> {
        let mut state = self.control.state.lock();
        state.orders.remove(client_order_id);
        state.order_position.remove(client_order_id);
        state.order_client.remove(client_order_id);
        Ok(())
    }

    fn delete_position(&self, position_id: &PositionId) -> anyhow::Result<()> {
        self.control.state.lock().positions.remove(position_id);
        Ok(())
    }

    fn delete_account_event(&self, _account_id: &AccountId, _event_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    fn index_venue_order_id(
        &self,
        _client_order_id: ClientOrderId,
        _venue_order_id: VenueOrderId,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn index_order_position(
        &self,
        client_order_id: ClientOrderId,
        position_id: PositionId,
    ) -> anyhow::Result<()> {
        let mut state = self.control.state.lock();
        if state.fail_index_order_position {
            anyhow::bail!("index order position failed");
        }

        state.order_position.insert(client_order_id, position_id);

        Ok(())
    }

    fn update_actor(
        &self,
        _actor_id: &ActorId,
        _state: &AHashMap<String, Bytes>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn update_strategy(
        &self,
        _strategy_id: &StrategyId,
        _state: &AHashMap<String, Bytes>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn update_account(&self, account: &AccountAny) -> anyhow::Result<()> {
        self.control
            .state
            .lock()
            .accounts
            .insert(account.id(), account.clone());
        Ok(())
    }

    fn update_order(&self, order_event: &OrderEventAny) -> anyhow::Result<()> {
        let client_order_id = order_event.client_order_id();
        let mut state = self.control.state.lock();
        let order = state
            .orders
            .get_mut(&client_order_id)
            .ok_or_else(|| anyhow::anyhow!("order {client_order_id} not found"))?;
        order.apply(order_event.clone())?;
        Ok(())
    }

    fn update_position(&self, position: &Position) -> anyhow::Result<()> {
        self.control
            .state
            .lock()
            .positions
            .insert(position.id, position.clone());
        Ok(())
    }

    fn snapshot_order_state(&self, order: &OrderAny) -> anyhow::Result<()> {
        self.control
            .state
            .lock()
            .order_snapshots
            .push(OrderSnapshot::from(order.clone()));
        Ok(())
    }

    fn snapshot_position_state(
        &self,
        position: &Position,
        ts_snapshot: UnixNanos,
        unrealized_pnl: Option<Money>,
    ) -> anyhow::Result<()> {
        let mut snapshot = PositionSnapshot::from(position, unrealized_pnl);
        snapshot.ts_init = ts_snapshot;

        self.control.state.lock().position_snapshots.push(snapshot);
        Ok(())
    }

    fn heartbeat(&self, _timestamp: UnixNanos) -> anyhow::Result<()> {
        Ok(())
    }
}
