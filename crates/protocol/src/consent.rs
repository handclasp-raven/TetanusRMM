//! Per-device consent policy (section 3 of the build plan).
//!
//! When a technician opens a session, the server sends the agent a
//! [`SessionRequest`] carrying the device's policy. Only the agent knows
//! whether a user is logged on, so the agent runs [`decide`], shows the
//! consent prompt if one is needed, and answers with an [`Outcome`]. The
//! server audits the outcome and starts the session only if
//! [`Outcome::allows_session`].
//!
//! | mode | user logged on | no user |
//! |---|---|---|
//! | `require` | prompt: `granted` / `denied` / `timeout` | `on_no_user`: `deny` -> `consent_unavailable`, `allow` -> `bypassed_no_user` |
//! | `notify` | `notify` (toast) | `notify_no_user` |
//! | `unattended` | `unattended` (silent) | `unattended` |
//!
//! A user who is logged on but whose screen is locked cannot see a prompt
//! or a toast, so the agent counts a locked console as "no user".

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentMode {
    /// Blocking prompt; the session starts only if the user accepts.
    Require,
    /// No prompt; the user gets a toast naming the technician.
    Notify,
    /// No prompt, no toast.
    Unattended,
}

/// What `require` does when nobody is logged on to answer the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnNoUser {
    Deny,
    Allow,
}

/// What kind of machine the agent runs on. Picks the default consent mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// A user's machine.
    Workstation,
    /// A server or headless device.
    Server,
}

/// Default consent mode for a newly enrolled device.
pub fn default_mode(kind: DeviceKind) -> ConsentMode {
    match kind {
        DeviceKind::Workstation => ConsentMode::Notify,
        DeviceKind::Server => ConsentMode::Unattended,
    }
}

/// How a session request ended. Audited with the mode in effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// `require`: the user accepted.
    Granted,
    /// `require`: the user declined.
    Denied,
    /// `require`: the user did not answer in time.
    Timeout,
    /// `notify`: started; the user was shown a toast.
    Notify,
    /// `notify` with nobody logged on: started, nobody to tell.
    NotifyNoUser,
    /// `unattended`: started silently.
    Unattended,
    /// `require` with nobody logged on and `on_no_user = allow`: started.
    BypassedNoUser,
    /// `require` and the prompt could not be shown (nobody logged on and
    /// `on_no_user = deny`, or the helper was unavailable): refused.
    ConsentUnavailable,
    /// The user pressed Ctrl+F12 and ended the session.
    UserTerminatedSession,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Granted => "granted",
            Outcome::Denied => "denied",
            Outcome::Timeout => "timeout",
            Outcome::Notify => "notify",
            Outcome::NotifyNoUser => "notify_no_user",
            Outcome::Unattended => "unattended",
            Outcome::BypassedNoUser => "bypassed_no_user",
            Outcome::ConsentUnavailable => "consent_unavailable",
            Outcome::UserTerminatedSession => "user_terminated_session",
        }
    }

    /// Whether the session may start.
    pub fn allows_session(self) -> bool {
        matches!(
            self,
            Outcome::Granted
                | Outcome::Notify
                | Outcome::NotifyNoUser
                | Outcome::Unattended
                | Outcome::BypassedNoUser
        )
    }

    /// Why a refused session was refused, for the technician.
    pub fn refusal_reason(self) -> &'static str {
        match self {
            Outcome::Denied => "the user declined the session",
            Outcome::Timeout => "the user did not respond to the consent prompt",
            Outcome::ConsentUnavailable => "nobody is available to approve the session",
            Outcome::UserTerminatedSession => "the user ended the session (Ctrl+F12)",
            _ => "session not allowed",
        }
    }
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the agent must do with a session request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Start the session now, with this outcome.
    Proceed(Outcome),
    /// Refuse the session, with this outcome.
    Refuse(Outcome),
    /// Ask the logged-on user; see [`PromptAnswer::outcome`].
    Ask,
}

/// The consent decision, before any prompt is shown.
pub fn decide(mode: ConsentMode, user_present: bool, on_no_user: OnNoUser) -> Decision {
    match (mode, user_present, on_no_user) {
        (ConsentMode::Unattended, _, _) => Decision::Proceed(Outcome::Unattended),
        (ConsentMode::Notify, true, _) => Decision::Proceed(Outcome::Notify),
        (ConsentMode::Notify, false, _) => Decision::Proceed(Outcome::NotifyNoUser),
        (ConsentMode::Require, true, _) => Decision::Ask,
        (ConsentMode::Require, false, OnNoUser::Allow) => {
            Decision::Proceed(Outcome::BypassedNoUser)
        }
        (ConsentMode::Require, false, OnNoUser::Deny) => {
            Decision::Refuse(Outcome::ConsentUnavailable)
        }
    }
}

/// How the consent prompt was answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PromptAnswer {
    Accepted,
    Declined,
    /// Nobody answered before the timeout.
    TimedOut,
    /// The prompt could not be shown (no helper in the user's session).
    Unavailable,
}

impl PromptAnswer {
    pub fn outcome(self) -> Outcome {
        match self {
            PromptAnswer::Accepted => Outcome::Granted,
            PromptAnswer::Declined => Outcome::Denied,
            PromptAnswer::TimedOut => Outcome::Timeout,
            PromptAnswer::Unavailable => Outcome::ConsentUnavailable,
        }
    }
}

/// Server to agent: a technician wants a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRequest {
    /// Identifies the session in later messages (the server's viewer-session id).
    pub session_id: u64,
    /// Who is connecting, as shown to the user.
    pub technician: String,
    pub mode: ConsentMode,
    pub on_no_user: OnNoUser,
    /// How long the user has to answer a `require` prompt.
    pub timeout_secs: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConsentMode::*;
    use OnNoUser::*;

    #[test]
    fn decision_table_covers_every_mode_presence_and_on_no_user() {
        let cases = [
            // mode, user present, on_no_user -> decision
            (Require, true, Deny, Decision::Ask),
            (Require, true, Allow, Decision::Ask),
            (
                Require,
                false,
                Deny,
                Decision::Refuse(Outcome::ConsentUnavailable),
            ),
            (
                Require,
                false,
                Allow,
                Decision::Proceed(Outcome::BypassedNoUser),
            ),
            (Notify, true, Deny, Decision::Proceed(Outcome::Notify)),
            (Notify, true, Allow, Decision::Proceed(Outcome::Notify)),
            (
                Notify,
                false,
                Deny,
                Decision::Proceed(Outcome::NotifyNoUser),
            ),
            (
                Notify,
                false,
                Allow,
                Decision::Proceed(Outcome::NotifyNoUser),
            ),
            (
                Unattended,
                true,
                Deny,
                Decision::Proceed(Outcome::Unattended),
            ),
            (
                Unattended,
                true,
                Allow,
                Decision::Proceed(Outcome::Unattended),
            ),
            (
                Unattended,
                false,
                Deny,
                Decision::Proceed(Outcome::Unattended),
            ),
            (
                Unattended,
                false,
                Allow,
                Decision::Proceed(Outcome::Unattended),
            ),
        ];
        for (mode, present, no_user, want) in cases {
            assert_eq!(
                decide(mode, present, no_user),
                want,
                "{mode:?} present={present} on_no_user={no_user:?}"
            );
        }
    }

    #[test]
    fn prompt_answers_map_to_outcomes_and_only_accept_starts() {
        let cases = [
            (PromptAnswer::Accepted, Outcome::Granted, true),
            (PromptAnswer::Declined, Outcome::Denied, false),
            (PromptAnswer::TimedOut, Outcome::Timeout, false),
            (
                PromptAnswer::Unavailable,
                Outcome::ConsentUnavailable,
                false,
            ),
        ];
        for (answer, outcome, starts) in cases {
            assert_eq!(answer.outcome(), outcome);
            assert_eq!(outcome.allows_session(), starts, "{outcome}");
        }
    }

    #[test]
    fn proceed_outcomes_start_and_refuse_outcomes_do_not() {
        for mode in [Require, Notify, Unattended] {
            for present in [true, false] {
                for no_user in [Deny, Allow] {
                    match decide(mode, present, no_user) {
                        Decision::Proceed(o) => assert!(o.allows_session(), "{o}"),
                        Decision::Refuse(o) => assert!(!o.allows_session(), "{o}"),
                        Decision::Ask => {}
                    }
                }
            }
        }
        assert!(!Outcome::UserTerminatedSession.allows_session());
    }

    #[test]
    fn outcome_strings_match_the_audit_vocabulary() {
        let all = [
            Outcome::Granted,
            Outcome::Denied,
            Outcome::Timeout,
            Outcome::Notify,
            Outcome::NotifyNoUser,
            Outcome::Unattended,
            Outcome::BypassedNoUser,
            Outcome::ConsentUnavailable,
            Outcome::UserTerminatedSession,
        ];
        let strings: Vec<&str> = all.iter().map(|o| o.as_str()).collect();
        assert_eq!(
            strings,
            [
                "granted",
                "denied",
                "timeout",
                "notify",
                "notify_no_user",
                "unattended",
                "bypassed_no_user",
                "consent_unavailable",
                "user_terminated_session",
            ]
        );
        // serde (used in audit JSON) agrees with as_str.
        for o in all {
            assert_eq!(serde_json::to_value(o).unwrap(), o.as_str());
        }
    }

    #[test]
    fn defaults_are_notify_for_workstations_and_unattended_for_servers() {
        assert_eq!(default_mode(DeviceKind::Workstation), Notify);
        assert_eq!(default_mode(DeviceKind::Server), Unattended);
    }
}
