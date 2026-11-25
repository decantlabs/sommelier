//! Role lease state, ROLE table representation, and epoch fencing (sections 3, 13.1, 13.3, and step 7).
//!
//! Defines the `ROLE` table row (`RoleRow`), lease scopes (`full` vs `limited`),
//! role lease states (`held`, `handing_over`, `vacant`), and command fencing by
//! epoch and lease scope.

use serde::{Deserialize, Serialize};

/// Scope of a role lease (section 3 and 13.3).
/// Our own seats receive `Full`; opted-in guest leads receive `Limited`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LeaseScope {
    #[default]
    Full,
    Limited,
}

impl LeaseScope {
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseScope::Full => "full",
            LeaseScope::Limited => "limited",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" => Some(LeaseScope::Full),
            "limited" => Some(LeaseScope::Limited),
            _ => None,
        }
    }
}

/// State of a role in the ROLE table (section 3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleState {
    Held,
    HandingOver,
    #[default]
    Vacant,
}

impl RoleState {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleState::Held => "held",
            RoleState::HandingOver => "handing_over",
            RoleState::Vacant => "vacant",
        }
    }
}

/// One row of the `ROLE` table (section 3):
/// Key: `(role, scope)` where `scope` is `"global"` or a machine name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleRow {
    pub role: String,
    /// `"global"` or a machine name (for per-machine roles like `run_keeper`).
    pub scope: String,
    pub holder: Option<String>,
    pub lease_scope: LeaseScope,
    pub epoch: u64,
    pub state: RoleState,
    pub granted_at: u64,
    pub heartbeat_at: u64,
    pub min_hold_until: u64,
    pub last_reason: String,
    pub changes_last_hour: u32,
    #[serde(default)]
    pub frozen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handover_note: Option<String>,
    #[serde(default)]
    pub msg_id: u64,
}

impl RoleRow {
    pub fn vacant(role: impl Into<String>, scope: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            scope: scope.into(),
            holder: None,
            lease_scope: LeaseScope::Full,
            epoch: 0,
            state: RoleState::Vacant,
            granted_at: 0,
            heartbeat_at: 0,
            min_hold_until: 0,
            last_reason: "initial".into(),
            changes_last_hour: 0,
            frozen: false,
            handover_note: None,
            msg_id: 0,
        }
    }
}

/// Role actions governed by epoch and scope fencing (section 13.3 table).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleCommand {
    // Allowed for both full and limited coordinator leases:
    AckAlert,
    RequeueTask,
    WithdrawOffer,
    AnswerStuck,
    RouteConsult,
    PostNudge,
    // Full lease only (never allowed for a limited guest coordinator lease):
    CancelTask,
    ApproveHeldBranch,
    Land,
    PauseMachine,
    ResumeMachine,
    AskBrain,
    PostGithub,
    RestartHub,
    PublishPolicy,
    TouchTokens,
    VetoRole,
    GrantRole,
}

impl RoleCommand {
    pub fn as_str(self) -> &'static str {
        match self {
            RoleCommand::AckAlert => "ack_alert",
            RoleCommand::RequeueTask => "requeue_task",
            RoleCommand::WithdrawOffer => "withdraw_offer",
            RoleCommand::AnswerStuck => "answer_stuck",
            RoleCommand::RouteConsult => "route_consult",
            RoleCommand::PostNudge => "post_nudge",
            RoleCommand::CancelTask => "cancel_task",
            RoleCommand::ApproveHeldBranch => "approve_held_branch",
            RoleCommand::Land => "land",
            RoleCommand::PauseMachine => "pause_machine",
            RoleCommand::ResumeMachine => "resume_machine",
            RoleCommand::AskBrain => "ask_brain",
            RoleCommand::PostGithub => "post_github",
            RoleCommand::RestartHub => "restart_hub",
            RoleCommand::PublishPolicy => "publish_policy",
            RoleCommand::TouchTokens => "touch_tokens",
            RoleCommand::VetoRole => "veto_role",
            RoleCommand::GrantRole => "grant_role",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "ack_alert" | "alert_ack" | "ack" => Some(RoleCommand::AckAlert),
            "requeue_task" | "task_requeue" | "requeue" => Some(RoleCommand::RequeueTask),
            "withdraw_offer" | "offer_withdraw" => Some(RoleCommand::WithdrawOffer),
            "answer_stuck" | "stuck" => Some(RoleCommand::AnswerStuck),
            "route_consult" | "consult" => Some(RoleCommand::RouteConsult),
            "post_nudge" | "nudge" => Some(RoleCommand::PostNudge),
            "cancel_task" | "task_cancel" | "cancel" => Some(RoleCommand::CancelTask),
            "approve_held_branch" | "land_approve" | "approve" => {
                Some(RoleCommand::ApproveHeldBranch)
            }
            "land" | "land_batch" => Some(RoleCommand::Land),
            "pause_machine" | "pause" => Some(RoleCommand::PauseMachine),
            "resume_machine" | "resume" => Some(RoleCommand::ResumeMachine),
            "ask_brain" | "ask" => Some(RoleCommand::AskBrain),
            "post_github" | "github" => Some(RoleCommand::PostGithub),
            "restart_hub" => Some(RoleCommand::RestartHub),
            "publish_policy" | "policy_publish" => Some(RoleCommand::PublishPolicy),
            "touch_tokens" | "token" => Some(RoleCommand::TouchTokens),
            "veto_role" | "role_veto" | "veto" => Some(RoleCommand::VetoRole),
            "grant_role" | "role_grant" => Some(RoleCommand::GrantRole),
            _ => None,
        }
    }

    /// Returns `true` when this command requires a `full` lease scope and is forbidden
    /// for a `limited` lease (section 13.3 table).
    pub fn is_full_only(self) -> bool {
        !matches!(
            self,
            RoleCommand::AckAlert
                | RoleCommand::RequeueTask
                | RoleCommand::WithdrawOffer
                | RoleCommand::AnswerStuck
                | RoleCommand::RouteConsult
                | RoleCommand::PostNudge
        )
    }
}

/// Refusal error with exit code 3 (`REFUSED`) returned when a role command fails fencing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FenceRefusal {
    pub code: u8,
    pub role: String,
    pub given_epoch: u64,
    pub current_epoch: u64,
    pub message: String,
}

impl std::fmt::Display for FenceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Verifies that `actor` holds `row` at `given_epoch`, and if `cmd` is specified,
/// verifies that the lease scope (`full` vs `limited`) allows `cmd` (section 13.3).
///
/// On stale epoch, refuses with exit code 3 and the exact phrasing of section 13.3:
/// `"you are no longer <role> (your epoch <given>, current <current>), stop"`.
/// On a limited lease attempting a full-only command, refuses with exit code 3.
pub fn check_fence(
    row: &RoleRow,
    actor: Option<&str>,
    given_epoch: u64,
    cmd: Option<RoleCommand>,
) -> Result<(), FenceRefusal> {
    if given_epoch != row.epoch || row.state == RoleState::Vacant {
        return Err(FenceRefusal {
            code: crate::exit::REFUSED,
            role: row.role.clone(),
            given_epoch,
            current_epoch: row.epoch,
            message: format!(
                "you are no longer {} (your epoch {}, current {}), stop",
                row.role, given_epoch, row.epoch
            ),
        });
    }

    if let Some(who) = actor
        && row.holder.as_deref() != Some(who)
    {
        return Err(FenceRefusal {
            code: crate::exit::REFUSED,
            role: row.role.clone(),
            given_epoch,
            current_epoch: row.epoch,
            message: format!(
                "you are no longer {} (held by {}, epoch {}), stop",
                row.role,
                row.holder.as_deref().unwrap_or("none"),
                row.epoch
            ),
        });
    }

    if let Some(command) = cmd
        && row.lease_scope == LeaseScope::Limited
        && command.is_full_only()
    {
        return Err(FenceRefusal {
            code: crate::exit::REFUSED,
            role: row.role.clone(),
            given_epoch,
            current_epoch: row.epoch,
            message: format!(
                "command `{}` requires a full {} lease (your lease scope is limited), ask the owner",
                command.as_str(),
                row.role
            ),
        });
    }

    Ok(())
}
