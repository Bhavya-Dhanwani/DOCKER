//! Live resource usage: parses `docker stats --no-stream --json` and keeps a short history per container.
use serde::Deserialize;
use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

/// ~2 minutes at one sample per second.
pub const HISTORY: usize = 120;
/// RAM growth between two samples that makes a container card flash.
pub const JUMP_FRACTION: f64 = 0.25;
const FLASH: Duration = Duration::from_millis(2000);
/// Ignore "jumps" smaller than this (a 2 MB process doubling is noise).
const JUMP_MIN_BYTES: u64 = 4 << 20;

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Sample {
    pub id: String,
    pub name: String,
    pub image: String,
    pub cpu_percent: f64,
    pub mem_bytes: u64,
    pub mem_limit: u64,
    pub mem_percent: f64,
    pub disk_bytes: u64,
    pub pids: u32,
}

/// One JSON object per line; lines that are not JSON objects are skipped.
pub fn parse_json_lines(out: &str) -> Vec<Sample> {
    out.lines().filter_map(|l| serde_json::from_str::<Sample>(l.trim()).ok()).filter(|s| !s.id.is_empty()).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Ok,
    Warn,
    Hot,
}

/// green < 60 %, amber 60-85 %, red > 85 %.
pub fn level(pct: f64) -> Level {
    if pct > 85.0 {
        Level::Hot
    } else if pct >= 60.0 {
        Level::Warn
    } else {
        Level::Ok
    }
}

pub fn jumped(prev: u64, now: u64) -> bool {
    now > prev && now - prev >= JUMP_MIN_BYTES && (now - prev) as f64 > prev as f64 * JUMP_FRACTION
}

pub struct Track {
    pub last: Sample,
    pub ring: VecDeque<u64>,
    pub peak: u64,
    pub live: bool,
    pub flash_until: Option<Instant>,
}

impl Track {
    pub fn flashing(&self, now: Instant) -> bool {
        self.flash_until.is_some_and(|t| now < t)
    }
    pub fn pct(&self) -> f64 {
        if self.last.mem_limit > 0 { self.last.mem_bytes as f64 * 100.0 / self.last.mem_limit as f64 } else { self.last.mem_percent }
    }
}

#[derive(Default)]
pub struct History {
    pub tracks: HashMap<String, Track>,
    pub err: Option<String>,
    pub loaded: bool,
}

impl History {
    pub fn ingest(&mut self, samples: Vec<Sample>, now: Instant) {
        self.err = None;
        self.loaded = true;
        for t in self.tracks.values_mut() {
            t.live = false;
        }
        for s in samples {
            let t = self.tracks.entry(s.id.clone()).or_insert_with(|| Track { last: s.clone(), ring: VecDeque::new(), peak: 0, live: true, flash_until: None });
            if let Some(&prev) = t.ring.back()
                && jumped(prev, s.mem_bytes)
            {
                t.flash_until = Some(now + FLASH);
            }
            t.ring.push_back(s.mem_bytes);
            while t.ring.len() > HISTORY {
                t.ring.pop_front();
            }
            t.peak = t.peak.max(s.mem_bytes);
            t.live = true;
            t.last = s;
        }
    }

    pub fn fail(&mut self, e: String) {
        self.err = Some(e);
        self.loaded = true;
    }

    /// Forget containers that no longer exist (by id prefix, as `ps` ids may be shorter or longer).
    pub fn retain_ids(&mut self, ids: &[&str]) {
        self.tracks.retain(|k, _| ids.iter().any(|i| i.starts_with(k.as_str()) || k.starts_with(i)));
    }

    /// Find the track for a `ps` row (ids can differ in length).
    pub fn get(&self, id: &str) -> Option<&Track> {
        self.tracks.get(id).or_else(|| self.tracks.iter().find(|(k, _)| !id.is_empty() && (id.starts_with(k.as_str()) || k.starts_with(id))).map(|(_, t)| t))
    }

    pub fn live(&self) -> impl Iterator<Item = &Track> {
        self.tracks.values().filter(|t| t.live)
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct Totals {
    pub mem: u64,
    pub disk: u64,
    pub cpu: f64,
}

pub fn totals<'a>(tracks: impl Iterator<Item = &'a Track>) -> Totals {
    tracks.fold(Totals::default(), |mut t, k| {
        t.mem += k.last.mem_bytes;
        t.disk += k.last.disk_bytes;
        t.cpu += k.last.cpu_percent;
        t
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SortKey {
    Name,
    Image,
    Ram,
    Disk,
    Cpu,
    Pids,
}

pub fn sort_tracks(rows: &mut [&Track], key: SortKey, desc: bool) {
    rows.sort_by(|a, b| {
        let (x, y) = (&a.last, &b.last);
        let o = match key {
            SortKey::Name => x.name.cmp(&y.name),
            SortKey::Image => x.image.cmp(&y.image),
            SortKey::Ram => x.mem_bytes.cmp(&y.mem_bytes),
            SortKey::Disk => x.disk_bytes.cmp(&y.disk_bytes),
            SortKey::Cpu => x.cpu_percent.total_cmp(&y.cpu_percent),
            SortKey::Pids => x.pids.cmp(&y.pids),
        };
        let o = if desc { o.reverse() } else { o };
        o.then_with(|| x.name.cmp(&y.name))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = r#"{"cpu_percent":117.6,"disk_bytes":41947136,"id":"102d3a0d054ee65a","image":"alpine","mem_bytes":43274240,"mem_limit":209715200,"mem_percent":20.6,"name":"spike","pids":3}"#;

    fn s(id: &str, mem: u64) -> Sample {
        Sample { id: id.into(), name: id.into(), mem_bytes: mem, ..Default::default() }
    }

    #[test]
    fn parses_cli_json() {
        let v = parse_json_lines(&format!("{LINE}\nnot json\n\n{{\"name\":\"no id\"}}\n"));
        assert_eq!(v.len(), 1);
        assert_eq!((v[0].name.as_str(), v[0].mem_bytes, v[0].disk_bytes, v[0].pids), ("spike", 43274240, 41947136, 3));
        assert!((v[0].cpu_percent - 117.6).abs() < 1e-9);
        assert!(parse_json_lines("").is_empty());
    }

    #[test]
    fn levels() {
        assert_eq!(level(20.6), Level::Ok);
        assert_eq!(level(60.0), Level::Warn);
        assert_eq!(level(85.0), Level::Warn);
        assert_eq!(level(85.1), Level::Hot);
    }

    #[test]
    fn jump_detection() {
        assert!(jumped(40 << 20, 60 << 20));
        assert!(!jumped(40 << 20, 45 << 20)); // < 25 %
        assert!(!jumped(1 << 20, 3 << 20)); // tiny
        assert!(!jumped(60 << 20, 10 << 20)); // shrinking
    }

    #[test]
    fn history_ring_peak_flash_and_stop() {
        let mut h = History::default();
        let t0 = Instant::now();
        h.ingest(vec![s("a", 40 << 20)], t0);
        assert!(!h.tracks["a"].flashing(t0));
        h.ingest(vec![s("a", 80 << 20)], t0);
        assert!(h.tracks["a"].flashing(t0));
        assert!(!h.tracks["a"].flashing(t0 + Duration::from_secs(3)));
        for _ in 0..HISTORY + 20 {
            h.ingest(vec![s("a", 10 << 20)], t0);
        }
        assert_eq!(h.tracks["a"].ring.len(), HISTORY);
        assert_eq!(h.tracks["a"].peak, 80 << 20);
        h.ingest(vec![], t0); // container stopped: track stays with its peak
        assert!(!h.tracks["a"].live && h.live().count() == 0);
        assert_eq!(h.get("a").unwrap().peak, 80 << 20);
        h.retain_ids(&["zzz"]);
        assert!(h.tracks.is_empty());
    }

    #[test]
    fn totals_and_sorting() {
        let mut h = History::default();
        let mut a = s("a", 10);
        a.cpu_percent = 5.0;
        a.disk_bytes = 1;
        let mut b = s("b", 30);
        b.cpu_percent = 1.0;
        b.disk_bytes = 2;
        h.ingest(vec![a, b], Instant::now());
        assert_eq!(totals(h.live()), Totals { mem: 40, disk: 3, cpu: 6.0 });
        let mut rows: Vec<&Track> = h.live().collect();
        sort_tracks(&mut rows, SortKey::Ram, true);
        assert_eq!(rows[0].last.name, "b");
        sort_tracks(&mut rows, SortKey::Cpu, true);
        assert_eq!(rows[0].last.name, "a");
        sort_tracks(&mut rows, SortKey::Name, false);
        assert_eq!(rows[0].last.name, "a");
    }
}
