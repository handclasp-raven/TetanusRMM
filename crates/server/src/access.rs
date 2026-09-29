//! Who may do what on which agent (RBAC).
//!
//! | role | sees | may do |
//! |---|---|---|
//! | `admin` | every agent | everything |
//! | `support_engineer` | agents it holds a grant for | what its grants allow |
//! | `auditor` | every agent | nothing: read-only |
//!
//! A grant gives one support engineer a set of [`Capability`]s on one
//! agent, on every agent in one group (membership is resolved at use, so
//! adding an agent to a group extends every grant on the group), or on all
//! agents. Grants add up; there are no deny rules.
//!
//! Every enforcement point (viewer tokens, viewer connect, shells, scripts,
//! file transfer, the agent list and policies) asks this module, and a
//! refusal of a remote operation is audited as `permission.denied`.

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgExecutor, PgPool};

use crate::audit::{self, Action, NewEntry};
use crate::users::{Capability, Role, User};

pub type Capabilities = BTreeSet<Capability>;

/// Every capability.
pub fn all_capabilities() -> Capabilities {
    Capability::ALL.into_iter().collect()
}

/// Which agents a user can see, and what they may do on each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// Every agent, with the same capabilities on each (admins: all;
    /// auditors: none).
    All(Capabilities),
    /// Only these agents, with these capabilities.
    Only(HashMap<String, Capabilities>),
}

impl Visibility {
    pub fn sees(&self, agent_id: &str) -> bool {
        match self {
            Visibility::All(_) => true,
            Visibility::Only(agents) => agents.contains_key(agent_id),
        }
    }

    /// What the user may do on `agent_id` (empty if nothing, or unseen).
    pub fn capabilities(&self, agent_id: &str) -> Capabilities {
        match self {
            Visibility::All(caps) => caps.clone(),
            Visibility::Only(agents) => agents.get(agent_id).cloned().unwrap_or_default(),
        }
    }
}

fn parse_capabilities(names: &[String]) -> Capabilities {
    names.iter().filter_map(|n| Capability::parse(n)).collect()
}

/// Capabilities an engineer's grants give on agents (all agents, or those
/// in `only`).
async fn granted<'e>(
    db: impl PgExecutor<'e>,
    user_id: i64,
    only: Option<&[String]>,
) -> sqlx::Result<HashMap<String, Capabilities>> {
    let rows: Vec<(String, Vec<String>)> = sqlx::query_as(
        "SELECT a.id, array_agg(DISTINCT c)
         FROM access_grants g
         JOIN agents a ON g.all_agents OR g.agent_id = a.id OR EXISTS (
             SELECT 1 FROM agent_group_members m
             WHERE m.group_id = g.group_id AND m.agent_id = a.id)
         CROSS JOIN LATERAL unnest(g.capabilities) AS c
         WHERE g.user_id = $1 AND ($2::text[] IS NULL OR a.id = ANY($2))
         GROUP BY a.id",
    )
    .bind(user_id)
    .bind(only)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(agent, caps)| (agent, parse_capabilities(&caps)))
        .collect())
}

/// What `user` can see and do.
pub async fn visibility<'e>(db: impl PgExecutor<'e>, user: &User) -> sqlx::Result<Visibility> {
    Ok(match user.role {
        Role::Admin => Visibility::All(all_capabilities()),
        Role::Auditor => Visibility::All(Capabilities::new()),
        Role::SupportEngineer => Visibility::Only(granted(db, user.id, None).await?),
    })
}

/// What `user` may do on `agent_id`.
pub async fn agent_capabilities<'e>(
    db: impl PgExecutor<'e>,
    user: &User,
    agent_id: &str,
) -> sqlx::Result<Capabilities> {
    Ok(match user.role {
        Role::Admin => all_capabilities(),
        Role::Auditor => Capabilities::new(),
        Role::SupportEngineer => granted(db, user.id, Some(&[agent_id.to_owned()]))
            .await?
            .remove(agent_id)
            .unwrap_or_default(),
    })
}

/// Which of `agents` `user` may *not* use `capability` on (empty: all
/// allowed). Unknown agents count as refused.
pub async fn refused<'e>(
    db: impl PgExecutor<'e>,
    user: &User,
    capability: Capability,
    agents: &[String],
) -> sqlx::Result<Vec<String>> {
    Ok(match user.role {
        Role::Admin => Vec::new(),
        Role::Auditor => agents.to_vec(),
        Role::SupportEngineer => {
            let granted = granted(db, user.id, Some(agents)).await?;
            agents
                .iter()
                .filter(|a| {
                    !granted
                        .get(*a)
                        .is_some_and(|caps| caps.contains(&capability))
                })
                .cloned()
                .collect()
        }
    })
}

/// Audit a refusal as `permission.denied`.
pub async fn audit_denied(
    pool: &PgPool,
    user: &User,
    capability: Capability,
    agents: &[String],
) -> sqlx::Result<()> {
    let mut entry = NewEntry::new(&user.username, Action::PermissionDenied).detail(json!({
        "capability": capability,
        "role": user.role,
        "agents": agents,
    }));
    if let [agent] = agents {
        entry = entry.target(agent);
    }
    crate::metrics::get()
        .permission_denied
        .get_or_create(&crate::metrics::DeniedLabels {
            capability: capability.as_str(),
        })
        .inc();
    audit::append_now(pool, entry).await.map(drop)
}

/// Check that `user` may use `capability` on every one of `agents`; if
/// not, audit the refusal and return the agents refused.
pub async fn check(
    pool: &PgPool,
    user: &User,
    capability: Capability,
    agents: &[String],
) -> sqlx::Result<Result<(), Vec<String>>> {
    let refused = refused(pool, user, capability, agents).await?;
    if refused.is_empty() {
        return Ok(Ok(()));
    }
    audit_denied(pool, user, capability, &refused).await?;
    Ok(Err(refused))
}

// --- Grants -------------------------------------------------------------

/// Where a grant applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Agent(String),
    Group(i64),
    AllAgents,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Grant {
    pub id: i64,
    pub user_id: i64,
    pub username: String,
    /// Exactly one of `agent_id`, `group_id` and `all_agents` is set.
    pub agent_id: Option<String>,
    pub group_id: Option<i64>,
    pub group_name: Option<String>,
    pub all_agents: bool,
    pub capabilities: Vec<String>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum GrantError {
    #[error("no such user")]
    UnknownUser,
    #[error("grants are for support engineers; admins may do everything and auditors nothing")]
    NotAnEngineer,
    #[error("no such agent")]
    UnknownAgent,
    #[error("no such group")]
    UnknownGroup,
    #[error("no capabilities given")]
    NoCapabilities,
    #[error("no such grant")]
    NotFound,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// `SELECT` of [`Grant`] rows followed by `$rest`, as a static string.
macro_rules! select_grants {
    ($rest:literal) => {
        concat!(
            "SELECT g.id, g.user_id, u.username, g.agent_id, g.group_id,
                 ag.name AS group_name, g.all_agents, g.capabilities, g.created_by, g.created_at
             FROM access_grants g JOIN users u ON u.id = g.user_id
             LEFT JOIN agent_groups ag ON ag.id = g.group_id ",
            $rest
        )
    };
}

/// Grants, all or one user's, oldest first.
pub async fn list_grants(pool: &PgPool, user_id: Option<i64>) -> sqlx::Result<Vec<Grant>> {
    sqlx::query_as(select_grants!(
        "WHERE $1::bigint IS NULL OR g.user_id = $1 ORDER BY g.id"
    ))
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// Give support engineer `user_id` `capabilities` within `scope`. Audited
/// as `grant.create` by `actor`.
pub async fn create_grant(
    pool: &PgPool,
    actor: &str,
    user_id: i64,
    scope: Scope,
    capabilities: &Capabilities,
) -> Result<Grant, GrantError> {
    if capabilities.is_empty() {
        return Err(GrantError::NoCapabilities);
    }
    let mut tx = pool.begin().await?;
    let role: Option<Role> = sqlx::query_scalar("SELECT role FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
    match role {
        None => return Err(GrantError::UnknownUser),
        Some(Role::SupportEngineer) => {}
        Some(_) => return Err(GrantError::NotAnEngineer),
    }
    let (agent_id, group_id, all_agents) = match &scope {
        Scope::Agent(id) => (Some(id.as_str()), None, false),
        Scope::Group(id) => (None, Some(*id), false),
        Scope::AllAgents => (None, None, true),
    };
    let names: Vec<&str> = capabilities.iter().map(|c| c.as_str()).collect();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO access_grants (user_id, agent_id, group_id, all_agents, capabilities, created_by)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(user_id)
    .bind(agent_id)
    .bind(group_id)
    .bind(all_agents)
    .bind(&names)
    .bind(actor)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.is_foreign_key_violation() => match scope {
            Scope::Group(_) => GrantError::UnknownGroup,
            _ => GrantError::UnknownAgent,
        },
        _ => GrantError::Db(e),
    })?;
    let grant: Grant = sqlx::query_as(select_grants!("WHERE g.id = $1"))
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::GrantCreate)
            .target(&grant.username)
            .detail(grant_detail(&grant)),
    )
    .await?;
    tx.commit().await?;
    Ok(grant)
}

fn grant_detail(grant: &Grant) -> serde_json::Value {
    json!({
        "grant_id": grant.id,
        "user_id": grant.user_id,
        "agent_id": grant.agent_id,
        "group_id": grant.group_id,
        "group_name": grant.group_name,
        "all_agents": grant.all_agents,
        "capabilities": grant.capabilities,
    })
}

/// Revoke a grant. Audited as `grant.delete` by `actor`, with what it gave.
pub async fn delete_grant(pool: &PgPool, actor: &str, id: i64) -> Result<(), GrantError> {
    let mut tx = pool.begin().await?;
    let grant: Option<Grant> = sqlx::query_as(select_grants!("WHERE g.id = $1 FOR UPDATE OF g"))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let grant = grant.ok_or(GrantError::NotFound)?;
    sqlx::query("DELETE FROM access_grants WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::GrantDelete)
            .target(&grant.username)
            .detail(grant_detail(&grant)),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_answers_per_agent() {
        let admin = Visibility::All(all_capabilities());
        assert!(admin.sees("any"));
        assert_eq!(admin.capabilities("any").len(), 4);

        let auditor = Visibility::All(Capabilities::new());
        assert!(auditor.sees("any"));
        assert!(auditor.capabilities("any").is_empty());

        let engineer = Visibility::Only(HashMap::from([(
            "agt-1".to_owned(),
            Capabilities::from([Capability::Desktop]),
        )]));
        assert!(engineer.sees("agt-1") && !engineer.sees("agt-2"));
        assert_eq!(
            engineer.capabilities("agt-1"),
            Capabilities::from([Capability::Desktop])
        );
        assert!(engineer.capabilities("agt-2").is_empty());
    }

    #[test]
    fn unknown_capability_names_are_ignored() {
        let caps = parse_capabilities(&["shell".into(), "bogus".into(), "desktop".into()]);
        assert_eq!(
            caps,
            Capabilities::from([Capability::Desktop, Capability::Shell])
        );
    }
}
