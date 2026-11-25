use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_EVENTS: usize = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagEvent {
    pub ts_unix: u64,
    pub level: String,
    pub category: String,
    pub message: String,
}

#[derive(Clone)]
pub struct Diagnostics {
    enabled: Arc<AtomicBool>,
    events: Arc<Mutex<VecDeque<DiagEvent>>>,
}

impl Diagnostics {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled: Arc::new(AtomicBool::new(enabled)),
            events: Arc::new(Mutex::new(VecDeque::with_capacity(MAX_EVENTS))),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_enabled(&self, val: bool) {
        self.enabled.store(val, Ordering::Relaxed);
    }

    pub fn record(&self, level: &str, category: &str, message: impl Into<String>) {
        if !self.is_enabled() && level != "error" {
            return;
        }
        let ts_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut q = self.events.lock().unwrap();
        if q.len() >= MAX_EVENTS {
            q.pop_front();
        }
        q.push_back(DiagEvent {
            ts_unix,
            level: level.to_string(),
            category: category.to_string(),
            message: message.into(),
        });
    }

    pub fn snapshot(&self, limit: usize) -> Vec<DiagEvent> {
        let q = self.events.lock().unwrap();
        q.iter().rev().take(limit).cloned().collect()
    }

    pub fn clear(&self) {
        self.events.lock().unwrap().clear();
    }
}
