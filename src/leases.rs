//! Who wants a feed and how often. A lease belongs to a bus client and ends when it unsubscribes or
//! leaves the bus, so a feed costs nothing while nobody watches it.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
struct State {
    owners: HashMap<String, Duration>,
    generation: u64,
}

#[derive(Default)]
pub struct Leases {
    state: Mutex<State>,
    changed: Condvar,
}

impl Leases {
    pub fn add(&self, owner: &str, every: Duration) {
        let mut state = self.state.lock().unwrap();
        state.owners.insert(owner.to_owned(), every);
        state.generation += 1;
        self.changed.notify_all();
    }

    pub fn remove(&self, owner: &str) {
        let mut state = self.state.lock().unwrap();
        if state.owners.remove(owner).is_some() {
            state.generation += 1;
            self.changed.notify_all();
        }
    }

    pub fn active(&self) -> bool {
        !self.state.lock().unwrap().owners.is_empty()
    }

    /// Blocks while there is no lease, then returns the shortest interval asked for
    pub fn wait_interval(&self) -> Duration {
        let state = self.changed.wait_while(self.state.lock().unwrap(), |s| s.owners.is_empty()).unwrap();
        state.owners.values().copied().min().unwrap_or_default()
    }

    /// Sleeps for `every`, or less if a lease starts or ends meanwhile
    pub fn sleep(&self, every: Duration) {
        let state = self.state.lock().unwrap();
        let generation = state.generation;
        let _ = self.changed.wait_timeout_while(state, every, |s| s.generation == generation).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    #[test]
    fn the_shortest_interval_wins_and_leaving_restores_the_next() {
        let leases = Leases::default();
        leases.add(":1.1", Duration::from_secs(4));
        leases.add(":1.2", Duration::from_secs(2));
        assert_eq!(leases.wait_interval(), Duration::from_secs(2));
        leases.remove(":1.2");
        assert_eq!(leases.wait_interval(), Duration::from_secs(4));
    }

    #[test]
    fn a_new_lease_cuts_the_sleep_short() {
        let leases = Arc::new(Leases::default());
        let other = leases.clone();
        let started = Instant::now();
        let handle = std::thread::spawn(move || other.sleep(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(50));
        leases.add(":1.3", Duration::from_secs(1));
        handle.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
