//! The hourly cap on automatic model calls (watch mode, the digest).  Chat
//! the user types is never blocked, only counted.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(3600);

/// Who is spending a model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spend {
    /// The user asked: counted, never blocked.
    Chat,
    /// Watch mode / digest: blocked at the cap.
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("hourly cap of {cap} automatic model calls reached")]
pub struct Exhausted {
    pub cap: u32,
}

/// Sliding one-hour window of model calls.
#[derive(Debug)]
pub struct Budget {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    cap: u32,
    calls: VecDeque<Instant>,
}

impl Budget {
    pub fn new(cap: u32) -> Self {
        Self {
            inner: Mutex::new(Inner {
                cap,
                calls: VecDeque::new(),
            }),
        }
    }

    pub fn set_cap(&self, cap: u32) {
        self.lock().cap = cap;
    }

    pub fn cap(&self) -> u32 {
        self.lock().cap
    }

    /// Calls in the last hour (chat included).
    pub fn used(&self) -> u32 {
        self.used_at(Instant::now())
    }

    pub fn used_at(&self, now: Instant) -> u32 {
        let mut inner = self.lock();
        inner.expire(now);
        inner.calls.len() as u32
    }

    pub fn spend(&self, spend: Spend) -> Result<(), Exhausted> {
        self.spend_at(spend, Instant::now())
    }

    pub fn spend_at(&self, spend: Spend, now: Instant) -> Result<(), Exhausted> {
        let mut inner = self.lock();
        inner.expire(now);
        if spend == Spend::Auto && inner.calls.len() as u32 >= inner.cap {
            return Err(Exhausted { cap: inner.cap });
        }
        inner.calls.push_back(now);
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Inner {
    fn expire(&mut self, now: Instant) {
        while self
            .calls
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) >= WINDOW)
        {
            self.calls.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_blocks_at_cap_chat_never() {
        let b = Budget::new(2);
        let t0 = Instant::now();
        b.spend_at(Spend::Auto, t0).unwrap();
        b.spend_at(Spend::Chat, t0).unwrap();
        assert_eq!(b.spend_at(Spend::Auto, t0), Err(Exhausted { cap: 2 }));
        b.spend_at(Spend::Chat, t0).unwrap();
        assert_eq!(b.used_at(t0), 3);
    }

    #[test]
    fn window_slides() {
        let b = Budget::new(1);
        let t0 = Instant::now();
        b.spend_at(Spend::Auto, t0).unwrap();
        assert!(b
            .spend_at(Spend::Auto, t0 + Duration::from_secs(3599))
            .is_err());
        assert!(b.spend_at(Spend::Auto, t0 + WINDOW).is_ok());
        assert_eq!(b.used_at(t0 + WINDOW), 1);
    }

    #[test]
    fn cap_changes_apply_immediately() {
        let b = Budget::new(0);
        assert!(b.spend(Spend::Auto).is_err());
        b.set_cap(5);
        assert!(b.spend(Spend::Auto).is_ok());
        assert_eq!(b.cap(), 5);
    }
}
