//! Deciding when to (re)start the helpers, and which of them captures the
//! screen. Pure logic, no Windows calls, so it is unit-tested everywhere;
//! `crate::win::service` and `crate::win::bridge` feed it observations and
//! carry out its actions.
//!
//! Why a helper at all: the service runs in session 0, which Windows isolates
//! from users. Session 0 has its own window station and desktop that nobody
//! sees, so a SYSTEM service there cannot capture the user's screen (it would
//! get session 0's empty desktop), inject input into it, or show the user a
//! tray icon. Anything that touches the user's desktop has to run as a
//! process *inside* the user's session. The service spawns that helper and
//! keeps it alive.
//!
//! The session helper runs as the user, so it needs one logged on. The
//! system helper runs as SYSTEM and also serves the logon screen, so for it
//! "a user is logged on" below reads "there is a console session".
//!
//! Rules:
//! - No user logged on at the console: do nothing, and spawn immediately
//!   once one appears.
//! - A user is logged on and no helper is running: spawn one.
//! - Spawning fails, or the helper dies soon after starting: retry with
//!   exponential backoff (a crash loop must not become a fork bomb).
//! - The console user changes (fast user switching, logoff): terminate the
//!   old helper; the next poll spawns one for the new session.

use std::time::{Duration, Instant};

/// A helper that stays up this long is healthy, and its death resets backoff.
pub const STABLE_AFTER: Duration = Duration::from_secs(60);

/// How often to re-check even without a session-change notification.
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub base: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            max: Duration::from_secs(60),
        }
    }
}

impl Backoff {
    /// Delay after the `failures`-th consecutive failure (1-based).
    pub fn delay(&self, failures: u32) -> Duration {
        let exp = failures.saturating_sub(1).min(16);
        self.base.saturating_mul(1 << exp).min(self.max)
    }
}

/// What the caller should do after [`Supervisor::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Idle,
    Spawn {
        session_id: u32,
    },
    /// Kill the running helper (its session is no longer the console user's).
    Terminate,
}

#[derive(Debug, Clone, Copy)]
struct Running {
    session_id: u32,
    since: Instant,
}

#[derive(Debug)]
pub struct Supervisor {
    backoff: Backoff,
    helper: Option<Running>,
    failures: u32,
    next_attempt: Option<Instant>,
}

impl Supervisor {
    pub fn new(backoff: Backoff) -> Self {
        Self {
            backoff,
            helper: None,
            failures: 0,
            next_attempt: None,
        }
    }

    /// Consecutive failures so far (for logging).
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// `console_session`: the console session if a user is logged on to it.
    /// `helper_running`: whether the helper we last spawned is still alive.
    pub fn poll(
        &mut self,
        now: Instant,
        console_session: Option<u32>,
        helper_running: bool,
    ) -> Action {
        if let Some(running) = self.helper {
            if !helper_running {
                // It died on its own.
                self.helper = None;
                if now.duration_since(running.since) >= STABLE_AFTER {
                    self.failures = 0;
                }
                self.fail(now);
            } else if console_session != Some(running.session_id) {
                self.helper = None;
                self.failures = 0;
                self.next_attempt = None;
                return Action::Terminate;
            } else {
                return Action::Idle;
            }
        }

        match console_session {
            None => {
                // Nobody to serve. Start fresh when someone logs on.
                self.failures = 0;
                self.next_attempt = None;
                Action::Idle
            }
            Some(session_id) if self.next_attempt.is_none_or(|t| now >= t) => {
                Action::Spawn { session_id }
            }
            Some(_) => Action::Idle,
        }
    }

    /// Report the outcome of an [`Action::Spawn`].
    pub fn spawned(&mut self, now: Instant, session_id: u32, ok: bool) {
        if ok {
            self.helper = Some(Running {
                session_id,
                since: now,
            });
            self.next_attempt = None;
        } else {
            self.fail(now);
        }
    }

    fn fail(&mut self, now: Instant) {
        self.failures += 1;
        self.next_attempt = Some(now + self.backoff.delay(self.failures));
    }

    /// How long to sleep before the next poll.
    pub fn next_wake(&self, now: Instant) -> Duration {
        match self.next_attempt {
            Some(t) if self.helper.is_none() => t.saturating_duration_since(now).min(POLL_INTERVAL),
            _ => POLL_INTERVAL,
        }
    }
}

/// One of the two helpers in the console session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperRole {
    /// The session helper, running as the logged-on user.
    User,
    /// The system helper, running as SYSTEM.
    System,
}

/// Which helper captures the screen, given which are attached and whether
/// the secure desktop (logon screen, lock screen, UAC prompt) is showing.
///
/// The user's helper captures the user's desktop: it runs as the user, so
/// it is the natural owner, and has the better capture API. It cannot see
/// the secure desktop, so there the system helper captures, as it does
/// when nobody is logged on and there is no user helper at all.
pub fn capturer(user: bool, system: bool, secure: bool) -> Option<HelperRole> {
    match (user, system, secure) {
        (_, true, true) => Some(HelperRole::System),
        (_, false, true) => None,
        (true, _, false) => Some(HelperRole::User),
        (false, true, false) => Some(HelperRole::System),
        (false, false, false) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_helper_captures_what_the_user_helper_cannot() {
        use HelperRole::{System, User};
        // The user's desktop: the user's helper, whoever else is there.
        assert_eq!(capturer(true, true, false), Some(User));
        assert_eq!(capturer(true, false, false), Some(User));
        // Lock screen or UAC prompt, with a user logged on.
        assert_eq!(capturer(true, true, true), Some(System));
        // Logon screen: nobody logged on, so no user helper.
        assert_eq!(capturer(false, true, true), Some(System));
        // The user helper is restarting.
        assert_eq!(capturer(false, true, false), Some(System));
        // The secure desktop without the system helper: nobody can.
        assert_eq!(capturer(true, false, true), None);
        assert_eq!(capturer(false, false, true), None);
        assert_eq!(capturer(false, false, false), None);
    }

    const S: Duration = Duration::from_secs(1);

    fn sup() -> (Supervisor, Instant) {
        (Supervisor::new(Backoff::default()), Instant::now())
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let b = Backoff::default();
        let delays: Vec<u64> = (1..=8).map(|n| b.delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
        assert_eq!(b.delay(u32::MAX), b.max);
    }

    #[test]
    fn no_user_means_no_helper_and_a_user_gets_one_immediately() {
        let (mut s, t0) = sup();
        for i in 0..10 {
            assert_eq!(s.poll(t0 + S * i, None, false), Action::Idle);
        }
        assert_eq!(
            s.poll(t0 + S * 10, Some(1), false),
            Action::Spawn { session_id: 1 }
        );
        s.spawned(t0 + S * 10, 1, true);
        assert_eq!(s.poll(t0 + S * 11, Some(1), true), Action::Idle);
    }

    #[test]
    fn failed_spawns_back_off_exponentially() {
        let (mut s, t0) = sup();
        let mut now = t0;
        let mut waits = Vec::new();
        for _ in 0..5 {
            assert_eq!(s.poll(now, Some(1), false), Action::Spawn { session_id: 1 });
            s.spawned(now, 1, false);
            // Not allowed again until the backoff has elapsed.
            let mut t = now;
            while s.poll(t, Some(1), false) == Action::Idle {
                t += Duration::from_millis(250);
            }
            waits.push((t - now).as_millis());
            now = t;
        }
        assert_eq!(waits, [1000, 2000, 4000, 8000, 16000]);
        assert_eq!(s.failures(), 5);
    }

    #[test]
    fn crash_loop_backs_off_but_a_stable_helper_resets_it() {
        let (mut s, t0) = sup();
        s.poll(t0, Some(1), false);
        s.spawned(t0, 1, true);
        // Dies after 2 s: counts as a failure.
        assert_eq!(s.poll(t0 + S * 2, Some(1), false), Action::Idle);
        assert_eq!(s.failures(), 1);
        assert_eq!(
            s.poll(t0 + S * 3, Some(1), false),
            Action::Spawn { session_id: 1 }
        );
        s.spawned(t0 + S * 3, 1, true);
        // Dies again quickly: backoff grows.
        s.poll(t0 + S * 4, Some(1), false);
        assert_eq!(s.failures(), 2);

        // A helper that ran for longer than STABLE_AFTER resets the count.
        let later = t0 + S * 10;
        assert_eq!(
            s.poll(later, Some(1), false),
            Action::Spawn { session_id: 1 }
        );
        s.spawned(later, 1, true);
        s.poll(later + STABLE_AFTER + S, Some(1), false);
        assert_eq!(s.failures(), 1);
    }

    #[test]
    fn user_switch_terminates_then_respawns_in_the_new_session() {
        let (mut s, t0) = sup();
        s.poll(t0, Some(1), false);
        s.spawned(t0, 1, true);
        assert_eq!(s.poll(t0 + S, Some(2), true), Action::Terminate);
        assert_eq!(
            s.poll(t0 + S, Some(2), false),
            Action::Spawn { session_id: 2 }
        );
    }

    #[test]
    fn logoff_terminates_and_waits_for_the_next_user() {
        let (mut s, t0) = sup();
        s.poll(t0, Some(1), false);
        s.spawned(t0, 1, true);
        assert_eq!(s.poll(t0 + S, None, true), Action::Terminate);
        assert_eq!(s.poll(t0 + S * 2, None, false), Action::Idle);
        assert_eq!(
            s.poll(t0 + S * 3, Some(1), false),
            Action::Spawn { session_id: 1 }
        );
    }

    #[test]
    fn failures_reset_when_the_user_leaves() {
        let (mut s, t0) = sup();
        s.poll(t0, Some(1), false);
        s.spawned(t0, 1, false);
        assert_eq!(s.failures(), 1);
        s.poll(t0 + S / 2, None, false);
        assert_eq!(s.failures(), 0);
        // Next logon spawns straight away, without waiting out the old backoff.
        assert_eq!(
            s.poll(t0 + S / 2, Some(3), false),
            Action::Spawn { session_id: 3 }
        );
    }

    #[test]
    fn next_wake_tracks_the_backoff_deadline() {
        let (mut s, t0) = sup();
        assert_eq!(s.next_wake(t0), POLL_INTERVAL);
        s.poll(t0, Some(1), false);
        s.spawned(t0, 1, false);
        assert_eq!(s.next_wake(t0), S);
        assert_eq!(s.next_wake(t0 + S * 2), Duration::ZERO);
    }
}
