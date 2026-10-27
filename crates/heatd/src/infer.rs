use crate::storage::AccessPoint;
use anyhow::{anyhow, Result};
use ort::session::Session;
use ort::value::Tensor;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Deserialize)]
pub struct Sidecar {
    pub version: u32,
    pub input_name: String,
    pub bssids: Vec<String>,
    pub labels: Vec<String>,
    pub rssi_floor: f32,
    pub rssi_span: f32,
}

impl Sidecar {
    pub fn features(&self, aps: &[AccessPoint]) -> Vec<f32> {
        let mut strongest: HashMap<String, i32> = HashMap::new();
        for ap in aps {
            let entry = strongest.entry(ap.bssid.to_lowercase()).or_insert(ap.rssi);
            *entry = (*entry).max(ap.rssi);
        }
        let n = self.bssids.len();
        let mut input = vec![0.0f32; 2 * n];
        for (i, bssid) in self.bssids.iter().enumerate() {
            if let Some(&rssi) = strongest.get(&bssid.to_lowercase()) {
                input[i] = ((rssi as f32 - self.rssi_floor) / self.rssi_span).clamp(0.0, 1.0);
                input[n + i] = 1.0;
            }
        }
        input
    }
}

pub struct RoomModel {
    sidecar: Sidecar,
    session: Mutex<Session>,
}

impl RoomModel {
    pub fn load(model_path: &Path) -> Result<Self> {
        let sidecar: Sidecar = serde_json::from_str(&std::fs::read_to_string(model_path.with_extension("json"))?)?;
        if sidecar.version != 1 {
            return Err(anyhow!("unsupported model sidecar version {}", sidecar.version));
        }
        let session = Session::builder()?.commit_from_file(model_path)?;
        Ok(Self { sidecar, session: Mutex::new(session) })
    }

    pub fn predict(&self, aps: &[AccessPoint]) -> Result<(String, f64)> {
        let input = self.sidecar.features(aps);
        let tensor = Tensor::from_array(([1usize, input.len()], input))?;
        let mut session = self.session.lock().unwrap();
        let outputs = session.run(ort::inputs![self.sidecar.input_name.as_str() => tensor])?;
        let (_, labels) = outputs["label"].try_extract_tensor::<i64>()?;
        let (_, probabilities) = outputs["probabilities"].try_extract_tensor::<f32>()?;
        let index = *labels.first().ok_or_else(|| anyhow!("empty label output"))? as usize;
        let label = self.sidecar.labels.get(index).ok_or_else(|| anyhow!("label index {index} out of range"))?;
        let probability = *probabilities.get(index).ok_or_else(|| anyhow!("probability index {index} out of range"))?;
        Ok((label.clone(), f64::from(probability)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::ScanBody;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    const CONTRACT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../contract");

    fn example_scan() -> ScanBody {
        serde_json::from_str(&std::fs::read_to_string(format!("{CONTRACT}/scan_example.json")).unwrap()).unwrap()
    }

    #[test]
    fn feature_vector_matches_contract() {
        let spec: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(format!("{CONTRACT}/features_example.json")).unwrap()).unwrap();
        let sidecar = Sidecar {
            version: 1,
            input_name: "input".into(),
            bssids: serde_json::from_value(spec["bssids"].clone()).unwrap(),
            labels: vec![],
            rssi_floor: spec["rssi_floor"].as_f64().unwrap() as f32,
            rssi_span: spec["rssi_span"].as_f64().unwrap() as f32,
        };
        let expected: Vec<f64> = serde_json::from_value(spec["expected_input"].clone()).unwrap();
        let got = sidecar.features(&example_scan().aps);
        assert_eq!(got.len(), expected.len());
        for (g, e) in got.iter().zip(&expected) {
            assert!((f64::from(*g) - e).abs() < 1e-6, "{g} vs {e}");
        }
    }

    #[test]
    fn fixture_model_predicts_wohnzimmer() {
        let model = RoomModel::load(&Path::new(FIXTURES).join("room_model.onnx")).unwrap();
        let (label, probability) = model.predict(&example_scan().aps).unwrap();
        assert_eq!(label, "wohnzimmer");
        assert!(probability > 0.9, "{probability}");
    }
}
