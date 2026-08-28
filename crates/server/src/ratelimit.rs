//! Flood control. The server used to trust clients to behave: nothing capped
//! how fast one connection could send messages, toggle reactions, or upload.
//! One runaway script — or one stolen token — could fill every screen in the
//! server. These are token buckets: a burst is fine, a sustained flood isn't.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Refills continuously at `per_sec`, capped at `capacity`. Taking a token
/// costs one action; when the bucket is empty, the action is refused.
#[derive(Debug)]
pub struct Bucket {
    tokens: f64,
    capacity: f64,
    per_sec: f64,
    last: Instant,
}

impl Bucket {
    pub fn new(capacity: f64, per_sec: f64) -> Self {
        Self { tokens: capacity, capacity, per_sec, last: Instant::now() }
    }

    pub fn take(&mut self) -> bool {
        self.take_at(Instant::now())
    }

    /// Split out so tests can drive time instead of sleeping.
    pub fn take_at(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// One connection's budget. Chat traffic and reactions get separate buckets
/// so a reaction spree can't silence someone mid-conversation, and typing
/// notifications get their own because they fan out to everyone.
pub struct ConnectionLimits {
    pub messages: Bucket,
    pub reactions: Bucket,
    pub typing: Bucket,
    pub edits: Bucket,
    /// Refusals so far; enough of them means this isn't a person.
    pub strikes: u32,
}

/// After this many refused actions on one connection, drop it. A human
/// hitting a limit sees a warning and slows down; a script never does.
pub const MAX_STRIKES: u32 = 30;

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            // ~10 in a burst, then one a second: faster than anyone types.
            messages: Bucket::new(10.0, 1.0),
            // Reactions are cheap and clicky — allow a flurry.
            reactions: Bucket::new(25.0, 4.0),
            // The client already throttles to one every 2.5s.
            typing: Bucket::new(6.0, 0.5),
            edits: Bucket::new(15.0, 1.0),
            strikes: 0,
        }
    }
}

/// Uploads are the expensive path (disk, storage cap, thumbnail CPU), and
/// they're per user rather than per connection — opening more sockets
/// shouldn't buy more upload budget.
pub struct UploadLimits {
    buckets: Mutex<HashMap<i64, Bucket>>,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self { buckets: Mutex::new(HashMap::new()) }
    }
}

impl UploadLimits {
    /// 20 in a burst (drag-and-drop of a folder is normal), then 1 every 3s.
    pub fn take(&self, user_id: i64) -> bool {
        let mut buckets = self.buckets.lock().unwrap();
        buckets.entry(user_id).or_insert_with(|| Bucket::new(20.0, 1.0 / 3.0)).take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn burst_then_throttle() {
        let mut b = Bucket::new(5.0, 1.0);
        let t0 = Instant::now();
        // The whole burst is allowed...
        for i in 0..5 {
            assert!(b.take_at(t0), "burst token {i} refused");
        }
        // ...then the tap closes.
        assert!(!b.take_at(t0));
        // A second later, exactly one more.
        let t1 = t0 + Duration::from_secs(1);
        assert!(b.take_at(t1));
        assert!(!b.take_at(t1));
    }

    #[test]
    fn refills_to_capacity_and_no_further() {
        let mut b = Bucket::new(3.0, 10.0);
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(b.take_at(t0));
        }
        // Idle for a minute: capacity is a ceiling, not a savings account.
        let t1 = t0 + Duration::from_secs(60);
        for i in 0..3 {
            assert!(b.take_at(t1), "post-idle token {i} refused");
        }
        assert!(!b.take_at(t1), "bucket exceeded its capacity after idling");
    }

    #[test]
    fn sustained_rate_matches_refill() {
        // One per second for ten seconds against a 1/s bucket: all allowed.
        let mut b = Bucket::new(2.0, 1.0);
        let t0 = Instant::now();
        for i in 0..10 {
            let t = t0 + Duration::from_secs(i);
            assert!(b.take_at(t), "steady message {i} refused");
        }
    }

    #[test]
    fn a_flood_is_mostly_refused() {
        // 200 messages as fast as a script can send them.
        let mut b = Bucket::new(10.0, 1.0);
        let t0 = Instant::now();
        let allowed = (0..200).filter(|_| b.take_at(t0)).count();
        assert_eq!(allowed, 10, "a flood should only get the burst");
    }

    #[test]
    fn uploads_are_per_user() {
        let limits = UploadLimits::default();
        // One user exhausts their budget...
        let mut allowed = 0;
        for _ in 0..40 {
            if limits.take(1) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, 20);
        assert!(!limits.take(1));
        // ...without touching anyone else's.
        assert!(limits.take(2), "one user's flood blocked another user");
    }
}
