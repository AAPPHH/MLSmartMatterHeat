use crate::{
    centi_degrees, AttributeReport, Error, MatterController, NodeId, Result, ThermostatState, LOCAL_TEMPERATURE,
    OCCUPIED_HEATING_SETPOINT, SYSTEM_MODE,
};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Mutex;
use tokio::sync::mpsc::{channel, Receiver, Sender};

const DRIFT_PER_TICK_C: f64 = 0.1;
const MODE_HEAT: f64 = 4.0;

struct MockNode {
    temperature: f64,
    setpoint: f64,
    mode: f64,
}

#[derive(Default)]
struct Inner {
    nodes: BTreeMap<NodeId, MockNode>,
    subscribers: Vec<(NodeId, Sender<AttributeReport>)>,
    setpoint_writes: Vec<(NodeId, f64)>,
}

pub struct MockController {
    endpoint: u16,
    inner: Mutex<Inner>,
}

impl MockController {
    pub fn new(endpoint: u16) -> Self {
        Self { endpoint, inner: Mutex::new(Inner::default()) }
    }

    pub fn add_node(&self, node: NodeId, temperature: f64, setpoint: f64) {
        self.inner.lock().unwrap().nodes.insert(node, MockNode { temperature, setpoint, mode: MODE_HEAT });
    }

    pub fn setpoint_writes(&self) -> Vec<(NodeId, f64)> {
        self.inner.lock().unwrap().setpoint_writes.clone()
    }

    pub fn tick(&self) {
        let mut inner = self.inner.lock().unwrap();
        let mut reports = Vec::new();
        for (node, state) in inner.nodes.iter_mut() {
            let diff = state.setpoint - state.temperature;
            let step = diff.clamp(-DRIFT_PER_TICK_C, DRIFT_PER_TICK_C);
            if step.abs() > 1e-9 {
                state.temperature = ((state.temperature + step) * 100.0).round() / 100.0;
                reports.push((*node, LOCAL_TEMPERATURE.snake, state.temperature));
            }
        }
        for (node, attribute, value) in reports {
            self.publish(&mut inner, node, attribute, value);
        }
    }

    fn publish(&self, inner: &mut Inner, node: NodeId, attribute: &str, value: f64) {
        let report = AttributeReport { node, endpoint: self.endpoint, attribute: attribute.to_string(), value };
        inner.subscribers.retain(|(n, tx)| {
            if *n == node {
                let _ = tx.try_send(report.clone());
            }
            !tx.is_closed()
        });
    }
}

#[async_trait]
impl MatterController for MockController {
    async fn commission(&self, node: NodeId, _pairing_code: &str, _thread_dataset_hex: &str) -> Result<()> {
        self.inner.lock().unwrap().nodes.entry(node).or_insert(MockNode { temperature: 18.0, setpoint: 17.0, mode: MODE_HEAT });
        Ok(())
    }

    async fn read_thermostat(&self, node: NodeId) -> Result<ThermostatState> {
        let inner = self.inner.lock().unwrap();
        let state = inner.nodes.get(&node).ok_or(Error::UnknownNode(node))?;
        Ok(ThermostatState {
            local_temperature_c: Some(state.temperature),
            occupied_heating_setpoint_c: Some(state.setpoint),
            system_mode: Some(state.mode as u8),
            extra: BTreeMap::new(),
        })
    }

    async fn write_heating_setpoint(&self, node: NodeId, celsius: f64) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let value = f64::from(centi_degrees(celsius)) / 100.0;
        inner.nodes.get_mut(&node).ok_or(Error::UnknownNode(node))?.setpoint = value;
        inner.setpoint_writes.push((node, value));
        self.publish(&mut inner, node, OCCUPIED_HEATING_SETPOINT.snake, value);
        Ok(())
    }

    async fn subscribe(&self, node: NodeId) -> Result<Receiver<AttributeReport>> {
        let mut inner = self.inner.lock().unwrap();
        let state = inner.nodes.get(&node).ok_or(Error::UnknownNode(node))?;
        let initial = [
            (LOCAL_TEMPERATURE.snake, state.temperature),
            (OCCUPIED_HEATING_SETPOINT.snake, state.setpoint),
            (SYSTEM_MODE.snake, state.mode),
        ];
        let (tx, rx) = channel(64);
        for (attribute, value) in initial {
            let report = AttributeReport { node, endpoint: self.endpoint, attribute: attribute.to_string(), value };
            let _ = tx.try_send(report);
        }
        inner.subscribers.push((node, tx));
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drifts_toward_setpoint_deterministically() {
        let mock = MockController::new(1);
        mock.add_node(5, 18.0, 18.0);
        mock.write_heating_setpoint(5, 18.25).await.unwrap();
        let mut rx = mock.subscribe(5).await.unwrap();
        for _ in 0..3 {
            mock.tick();
        }
        let state = mock.read_thermostat(5).await.unwrap();
        assert_eq!(state.local_temperature_c, Some(18.25));
        assert_eq!(state.occupied_heating_setpoint_c, Some(18.25));
        let mut temps = Vec::new();
        while let Ok(report) = rx.try_recv() {
            if report.attribute == "local_temperature" {
                temps.push(report.value);
            }
        }
        assert_eq!(temps, vec![18.0, 18.1, 18.2, 18.25]);
    }

    #[tokio::test]
    async fn cools_down_and_rejects_unknown_nodes() {
        let mock = MockController::new(1);
        mock.commission(7, "34970112332", "00").await.unwrap();
        mock.write_heating_setpoint(7, 17.0).await.unwrap();
        mock.tick();
        assert_eq!(mock.read_thermostat(7).await.unwrap().local_temperature_c, Some(17.9));
        assert!(matches!(mock.read_thermostat(9).await, Err(Error::UnknownNode(9))));
        assert_eq!(mock.setpoint_writes(), vec![(7, 17.0)]);
    }
}
