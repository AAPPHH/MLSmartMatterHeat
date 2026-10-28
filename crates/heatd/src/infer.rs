use crate::storage::AccessPoint;
use anyhow::{anyhow, Result};
use ort::session::Session;
use ort::value::Tensor;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Part {
    pub bssid: String,
    pub share: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RoomShare {
    pub room: String,
    pub p: f64,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Explanation {
    pub rooms: Vec<RoomShare>,
    pub aps: Vec<AccessPoint>,
}

impl Explanation {
    pub fn probability(&self, room: &str) -> Option<f64> {
        self.rooms.iter().find(|r| r.room == room).map(|r| r.p)
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

    fn run_batch(&self, rows: &[Vec<f32>]) -> Result<Vec<Vec<f32>>> {
        let classes = self.sidecar.labels.len();
        let width = rows[0].len();
        let mut session = self.session.lock().unwrap();
        let flat: Vec<f32> = rows.iter().flatten().copied().collect();
        let tensor = Tensor::from_array(([rows.len(), width], flat))?;
        let batched = session.run(ort::inputs![self.sidecar.input_name.as_str() => tensor]).ok().and_then(|outputs| {
            let (_, p) = outputs["probabilities"].try_extract_tensor::<f32>().ok()?;
            (p.len() == rows.len() * classes).then(|| p.chunks(classes).map(<[f32]>::to_vec).collect::<Vec<_>>())
        });
        if let Some(result) = batched {
            return Ok(result);
        }
        rows.iter()
            .map(|row| {
                let tensor = Tensor::from_array(([1usize, width], row.clone()))?;
                let outputs = session.run(ort::inputs![self.sidecar.input_name.as_str() => tensor])?;
                let (_, p) = outputs["probabilities"].try_extract_tensor::<f32>()?;
                Ok(p.to_vec())
            })
            .collect()
    }

    pub fn explain(&self, aps: &[AccessPoint]) -> Result<Explanation> {
        if aps.is_empty() {
            return Ok(Explanation::default());
        }
        let known: HashSet<String> = self.sidecar.bssids.iter().map(|b| b.to_lowercase()).collect();
        let mut strongest: Vec<AccessPoint> = Vec::new();
        for ap in aps.iter().filter(|ap| known.contains(&ap.bssid.to_lowercase())) {
            let bssid = ap.bssid.to_lowercase();
            match strongest.iter_mut().find(|s| s.bssid == bssid) {
                Some(s) if ap.rssi > s.rssi => *s = AccessPoint { bssid, ..ap.clone() },
                Some(_) => {}
                None => strongest.push(AccessPoint { bssid, ..ap.clone() }),
            }
        }
        let mut rows = vec![self.sidecar.features(aps)];
        for removed in &strongest {
            let rest: Vec<AccessPoint> = aps.iter().filter(|ap| ap.bssid.to_lowercase() != removed.bssid).cloned().collect();
            rows.push(self.sidecar.features(&rest));
        }
        let probabilities = self.run_batch(&rows)?;
        let (full, without) = probabilities.split_first().ok_or_else(|| anyhow!("empty batch output"))?;
        let mut rooms: Vec<RoomShare> = self
            .sidecar
            .labels
            .iter()
            .enumerate()
            .map(|(c, room)| {
                let p = f64::from(full[c]);
                let drops: Vec<(usize, f64)> =
                    without.iter().enumerate().map(|(i, w)| (i, f64::from((full[c] - w[c]).max(0.0)))).filter(|d| d.1 > 0.0).collect();
                let total: f64 = drops.iter().map(|d| d.1).sum();
                let parts = drops.iter().map(|&(i, d)| Part { bssid: strongest[i].bssid.clone(), share: p * d / total }).collect();
                RoomShare { room: room.clone(), p, parts }
            })
            .collect();
        rooms.sort_by(|a, b| b.p.total_cmp(&a.p));
        let mut totals: Vec<(f64, &AccessPoint)> = strongest
            .iter()
            .map(|ap| (rooms.iter().flat_map(|r| &r.parts).filter(|p| p.bssid == ap.bssid).map(|p| p.share).sum::<f64>(), ap))
            .filter(|t| t.0 > 0.0)
            .collect();
        totals.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(Explanation { rooms, aps: totals.into_iter().map(|t| t.1.clone()).collect() })
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

    #[test]
    fn explanation_shares_sum_to_probability() {
        let model = RoomModel::load(&Path::new(FIXTURES).join("room_model.onnx")).unwrap();
        let scan = example_scan();
        let ex = model.explain(&scan.aps).unwrap();
        assert_eq!(ex.rooms.len(), 3);
        assert!(ex.rooms.windows(2).all(|w| w[0].p >= w[1].p));
        assert_eq!(ex.rooms[0].room, "wohnzimmer");
        let total: f64 = ex.rooms.iter().map(|r| r.p).sum();
        assert!((total - 1.0).abs() < 1e-4);
        let (_, p) = model.predict(&scan.aps).unwrap();
        assert!((ex.probability("wohnzimmer").unwrap() - p).abs() < 1e-6);
        for room in &ex.rooms {
            if !room.parts.is_empty() {
                let sum: f64 = room.parts.iter().map(|p| p.share).sum();
                assert!((sum - room.p).abs() < 1e-6, "{} {sum} {}", room.room, room.p);
            }
            for part in &room.parts {
                assert!(model.sidecar.bssids.contains(&part.bssid));
                assert!(part.share > 0.0);
                assert!(ex.aps.iter().any(|ap| ap.bssid == part.bssid));
            }
        }
        assert!(!ex.aps.is_empty());
        assert!(ex.aps.iter().all(|ap| ap.bssid != "aa:bb:cc:00:00:99"));
        let share_of = |b: &str| ex.rooms.iter().flat_map(|r| &r.parts).filter(|p| p.bssid == b).map(|p| p.share).sum::<f64>();
        assert!(ex.aps.windows(2).all(|w| share_of(&w[0].bssid) >= share_of(&w[1].bssid)));
    }

    #[test]
    fn empty_scan_has_empty_explanation() {
        let model = RoomModel::load(&Path::new(FIXTURES).join("room_model.onnx")).unwrap();
        assert_eq!(model.explain(&[]).unwrap(), Explanation::default());
    }
}
