/// Coalesces watcher bursts while retaining edits received during an active pass.
/// All times are wall-clock milliseconds.
#[derive(Clone, Debug)]
pub struct Scheduling {
    quiet_ms: i64,
    checkpoint_ms: i64,
    remote_poll_ms: i64,
    reconcile_ms: i64,
    retry_max_ms: i64,
    first_edit: Option<i64>,
    last_edit: i64,
    remote_at: i64,
    retry_at: i64,
    reconcile_at: i64,
}

impl Scheduling {
    pub fn new(
        quiet_ms: i64,
        checkpoint_ms: i64,
        remote_poll_ms: i64,
        reconcile_ms: i64,
        retry_max_ms: i64,
    ) -> Self {
        Self {
            quiet_ms,
            checkpoint_ms,
            remote_poll_ms,
            reconcile_ms,
            retry_max_ms,
            first_edit: None,
            last_edit: 0,
            remote_at: 0,
            retry_at: 0,
            reconcile_at: 0,
        }
    }

    /// Records an edit. Returns true when it starts a new batch, which is the
    /// only case where the next deadline can move earlier.
    pub fn changed(&mut self, now: i64) -> bool {
        let first = self.first_edit.is_none();
        self.first_edit.get_or_insert(now);
        self.last_edit = now;
        first
    }

    /// The earliest time at which a pass is due.
    pub fn next_due(&self) -> i64 {
        let periodic = self.remote_at.min(self.reconcile_at);
        match self.first_edit {
            Some(first) => periodic
                .min(self.last_edit.saturating_add(self.quiet_ms))
                .min(first.saturating_add(self.checkpoint_ms)),
            None => periodic,
        }
    }

    pub fn due(&self, now: i64) -> bool {
        now >= self.next_due()
    }

    /// Starts a pass. Returns whether it may contact the remote.
    pub fn begin(&mut self, now: i64, force: bool) -> bool {
        self.first_edit = None;
        self.reconcile_at = now.saturating_add(self.reconcile_ms);
        force || now >= self.retry_at
    }

    pub fn completed(&mut self, now: i64, failures: i64, network: bool) {
        if !network {
            return;
        }

        let wait = if failures > 0 {
            let exponent = (failures - 1).min(20) as u32;
            self.retry_max_ms
                .min(self.remote_poll_ms.saturating_mul(2_i64.pow(exponent)))
        } else {
            self.remote_poll_ms
        };
        self.remote_at = now.saturating_add(wait);
        self.retry_at = if failures > 0 { self.remote_at } else { 0 };
    }

    pub fn next_remote(&self) -> i64 {
        self.remote_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_period_continuous_edit_checkpoint_and_fresh_local_network_attempt() {
        let mut timing = Scheduling::new(10_000, 60_000, 90_000, 300_000, 300_000);
        assert!(timing.begin(0, false));
        timing.completed(0, 0, true);
        timing.changed(1_000);
        timing.changed(5_000);
        assert!(!timing.due(14_999));
        assert!(timing.due(15_000));
        assert!(timing.begin(15_000, false));
        timing.completed(15_000, 0, true);

        for now in (20_000..=75_000).step_by(5_000) {
            timing.changed(now);
        }
        assert!(!timing.due(79_999));
        assert!(timing.due(80_000));
    }

    #[test]
    fn backoff_preserves_local_checkpoints_and_manual_requests_bypass_it() {
        let mut timing = Scheduling::new(1_000, 5_000, 10_000, 300_000, 25_000);
        timing.begin(0, false);
        timing.completed(0, 2, true);
        timing.changed(1_000);
        assert!(timing.due(2_000));
        assert!(!timing.begin(2_000, false));
        timing.completed(2_000, 2, false);
        assert_eq!(timing.next_remote(), 20_000);
        assert!(timing.begin(3_000, true));
        timing.completed(3_000, 20, true);
        assert_eq!(timing.next_remote(), 28_000);
    }

    #[test]
    fn events_during_a_pass_survive_completion_and_missed_wake_deadlines_are_due() {
        let mut timing = Scheduling::new(10, 60, 100, 300, 500);
        timing.begin(0, false);
        timing.changed(1);
        timing.completed(5, 0, true);
        assert!(timing.due(11));
        timing.begin(11, false);
        timing.completed(12, 0, true);
        assert!(timing.due(1_000));
    }

    #[test]
    fn only_the_first_edit_of_a_batch_can_move_the_deadline_earlier() {
        let mut timing = Scheduling::new(10_000, 60_000, 90_000, 300_000, 300_000);
        timing.begin(0, false);
        timing.completed(0, 0, true);
        assert_eq!(timing.next_due(), 90_000);
        assert!(timing.changed(1_000));
        assert_eq!(timing.next_due(), 11_000);
        assert!(!timing.changed(2_000));
        assert_eq!(timing.next_due(), 12_000);
        timing.begin(12_000, false);
        assert!(timing.changed(13_000));
    }

    #[test]
    fn next_due_is_the_first_instant_due_reports_true() {
        let mut timing = Scheduling::new(300, 1_200, 800, 5_000, 3_000);
        let mut now = 0;
        timing.begin(now, false);
        timing.completed(now, 0, true);

        for step in 0..200 {
            now += 37;
            if step % 3 == 0 {
                timing.changed(now);
            }
            if step % 11 == 0 {
                timing.begin(now, false);
                timing.completed(now, step % 4, true);
            }
            let due = timing.next_due();
            assert!(timing.due(due));
            assert!(!timing.due(due - 1));
        }
    }

    #[test]
    fn backoff_saturates_instead_of_overflowing() {
        let mut timing = Scheduling::new(1, 1, i64::MAX / 2, 1, i64::MAX);
        timing.completed(i64::MAX / 2, 50, true);
        assert_eq!(timing.next_remote(), i64::MAX);
    }
}
