//! Agent groups: named sets of agents (an agent can be in several), for
//! granting access to many agents at once (`crate::access`), running a
//! script across a group, and filtering the agent list. Enrollment links
//! can put a new agent straight into groups.
//!
//! Every change is audited: `group.create`, `group.update`,
//! `group.delete` and `group.members` (with the agents added and removed).

use std::collections::{BTreeSet, HashMap};

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};

use crate::audit::{self, Action, NewEntry};

pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_DESCRIPTION_CHARS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
    /// Members, sorted.
    pub agent_ids: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum GroupError {
    #[error("group name must be 1-{MAX_NAME_CHARS} characters, with no control characters")]
    InvalidName,
    #[error("description must be at most {MAX_DESCRIPTION_CHARS} characters")]
    InvalidDescription,
    #[error("a group with that name already exists")]
    Duplicate,
    #[error("no such group")]
    NotFound,
    #[error("no such group: {0}")]
    UnknownGroup(i64),
    #[error("no such agent: {0}")]
    UnknownAgent(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

fn check_name(name: &str) -> Result<&str, GroupError> {
    let name = name.trim();
    if name.is_empty()
        || name.chars().count() > MAX_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(GroupError::InvalidName);
    }
    Ok(name)
}

fn check_description(description: &str) -> Result<&str, GroupError> {
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        return Err(GroupError::InvalidDescription);
    }
    Ok(description.trim())
}

fn unique_violation(e: sqlx::Error) -> GroupError {
    match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => GroupError::Duplicate,
        _ => GroupError::Db(e),
    }
}

/// `SELECT` of [`Group`] rows (with members) followed by `$rest`, as a
/// static string.
macro_rules! select_groups {
    ($rest:literal) => {
        concat!(
            "SELECT g.id, g.name, g.description, g.created_at,
                 COALESCE(array_agg(m.agent_id ORDER BY m.agent_id)
                          FILTER (WHERE m.agent_id IS NOT NULL), '{}') AS agent_ids
             FROM agent_groups g LEFT JOIN agent_group_members m ON m.group_id = g.id ",
            $rest
        )
    };
}

pub async fn list(pool: &PgPool) -> sqlx::Result<Vec<Group>> {
    sqlx::query_as(select_groups!("GROUP BY g.id ORDER BY g.name"))
        .fetch_all(pool)
        .await
}

pub async fn get(pool: &PgPool, id: i64) -> sqlx::Result<Option<Group>> {
    fetch(pool, id).await
}

async fn fetch<'e>(db: impl sqlx::PgExecutor<'e>, id: i64) -> sqlx::Result<Option<Group>> {
    sqlx::query_as(select_groups!("WHERE g.id = $1 GROUP BY g.id"))
        .bind(id)
        .fetch_optional(db)
        .await
}

/// Lock a group for changing (so concurrent membership edits serialize).
async fn lock(tx: &mut Transaction<'_, Postgres>, id: i64) -> Result<Group, GroupError> {
    sqlx::query("SELECT 1 FROM agent_groups WHERE id = $1 FOR UPDATE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(GroupError::NotFound)?;
    fetch(&mut **tx, id).await?.ok_or(GroupError::NotFound)
}

pub async fn create(
    pool: &PgPool,
    actor: &str,
    name: &str,
    description: &str,
) -> Result<Group, GroupError> {
    let name = check_name(name)?;
    let description = check_description(description)?;
    let mut tx = pool.begin().await?;
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO agent_groups (name, description) VALUES ($1, $2) RETURNING id",
    )
    .bind(name)
    .bind(description)
    .fetch_one(&mut *tx)
    .await
    .map_err(unique_violation)?;
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::GroupCreate)
            .target(name)
            .detail(json!({ "group_id": id, "description": description })),
    )
    .await?;
    let group = fetch(&mut *tx, id).await?.ok_or(GroupError::NotFound)?;
    tx.commit().await?;
    Ok(group)
}

/// Rename and/or redescribe a group.
pub async fn update(
    pool: &PgPool,
    actor: &str,
    id: i64,
    name: Option<&str>,
    description: Option<&str>,
) -> Result<Group, GroupError> {
    let name = name.map(check_name).transpose()?;
    let description = description.map(check_description).transpose()?;
    let mut tx = pool.begin().await?;
    let before = lock(&mut tx, id).await?;
    sqlx::query(
        "UPDATE agent_groups SET name = COALESCE($2, name),
             description = COALESCE($3, description)
         WHERE id = $1",
    )
    .bind(id)
    .bind(name)
    .bind(description)
    .execute(&mut *tx)
    .await
    .map_err(unique_violation)?;
    let after = fetch(&mut *tx, id).await?.ok_or(GroupError::NotFound)?;
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::GroupUpdate)
            .target(&after.name)
            .detail(json!({
                "group_id": id,
                "before": { "name": before.name, "description": before.description },
                "after": { "name": after.name, "description": after.description },
            })),
    )
    .await?;
    tx.commit().await?;
    Ok(after)
}

/// Delete a group. Grants on it go with it (their access ends).
pub async fn delete(pool: &PgPool, actor: &str, id: i64) -> Result<(), GroupError> {
    let mut tx = pool.begin().await?;
    let group = lock(&mut tx, id).await?;
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM access_grants WHERE group_id = $1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM agent_groups WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit::append(
        &mut tx,
        NewEntry::new(actor, Action::GroupDelete)
            .target(&group.name)
            .detail(json!({
                "group_id": id,
                "agent_ids": group.agent_ids,
                "grants_removed": grants,
            })),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

/// Change a group's membership: add `add`, remove `remove`. Unknown
/// agents are an error; adding a member or removing a non-member is a
/// no-op. Audited as `group.members` if anything changed.
pub async fn change_members(
    pool: &PgPool,
    actor: &str,
    id: i64,
    add: &[String],
    remove: &[String],
) -> Result<Group, GroupError> {
    let mut tx = pool.begin().await?;
    let before = lock(&mut tx, id).await?;
    apply_members(&mut tx, actor, &before, add, remove).await?;
    let after = fetch(&mut *tx, id).await?.ok_or(GroupError::NotFound)?;
    tx.commit().await?;
    Ok(after)
}

/// Make the group's membership exactly `agent_ids`.
pub async fn set_members(
    pool: &PgPool,
    actor: &str,
    id: i64,
    agent_ids: &[String],
) -> Result<Group, GroupError> {
    let mut tx = pool.begin().await?;
    let before = lock(&mut tx, id).await?;
    let wanted: BTreeSet<&String> = agent_ids.iter().collect();
    let current: BTreeSet<&String> = before.agent_ids.iter().collect();
    let add: Vec<String> = wanted.difference(&current).map(|s| (*s).clone()).collect();
    let remove: Vec<String> = current.difference(&wanted).map(|s| (*s).clone()).collect();
    apply_members(&mut tx, actor, &before, &add, &remove).await?;
    let after = fetch(&mut *tx, id).await?.ok_or(GroupError::NotFound)?;
    tx.commit().await?;
    Ok(after)
}

async fn apply_members(
    tx: &mut Transaction<'_, Postgres>,
    actor: &str,
    group: &Group,
    add: &[String],
    remove: &[String],
) -> Result<(), GroupError> {
    if !add.is_empty() {
        let known: Vec<String> = sqlx::query_scalar("SELECT id FROM agents WHERE id = ANY($1)")
            .bind(add)
            .fetch_all(&mut **tx)
            .await?;
        if let Some(unknown) = add.iter().find(|a| !known.contains(a)) {
            return Err(GroupError::UnknownAgent(unknown.clone()));
        }
    }
    let added: Vec<String> = sqlx::query_scalar(
        "INSERT INTO agent_group_members (group_id, agent_id)
         SELECT $1, unnest($2::text[]) ON CONFLICT DO NOTHING RETURNING agent_id",
    )
    .bind(group.id)
    .bind(add)
    .fetch_all(&mut **tx)
    .await?;
    let removed: Vec<String> = sqlx::query_scalar(
        "DELETE FROM agent_group_members WHERE group_id = $1 AND agent_id = ANY($2)
         RETURNING agent_id",
    )
    .bind(group.id)
    .bind(remove)
    .fetch_all(&mut **tx)
    .await?;
    if added.is_empty() && removed.is_empty() {
        return Ok(());
    }
    audit::append(
        tx,
        NewEntry::new(actor, Action::GroupMembers)
            .target(&group.name)
            .detail(json!({ "group_id": group.id, "added": added, "removed": removed })),
    )
    .await?;
    Ok(())
}

/// The members of `group_ids`, deduplicated, in group order then agent id
/// order. Any unknown group is an error.
pub async fn members_of(pool: &PgPool, group_ids: &[i64]) -> Result<Vec<String>, GroupError> {
    let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT g.id, m.agent_id FROM agent_groups g
         LEFT JOIN agent_group_members m ON m.group_id = g.id
         WHERE g.id = ANY($1) ORDER BY g.id, m.agent_id",
    )
    .bind(group_ids)
    .fetch_all(pool)
    .await?;
    if let Some(missing) = group_ids
        .iter()
        .find(|id| !rows.iter().any(|(g, _)| g == *id))
    {
        return Err(GroupError::UnknownGroup(*missing));
    }
    let mut out = Vec::new();
    for id in group_ids {
        for (_, agent) in rows.iter().filter(|(g, _)| g == id) {
            if let Some(agent) = agent.as_ref().filter(|a| !out.contains(*a)) {
                out.push(agent.clone());
            }
        }
    }
    Ok(out)
}

/// Check that every group in `group_ids` exists.
pub async fn check_exist(pool: &PgPool, group_ids: &[i64]) -> Result<(), GroupError> {
    let known: Vec<i64> = sqlx::query_scalar("SELECT id FROM agent_groups WHERE id = ANY($1)")
        .bind(group_ids)
        .fetch_all(pool)
        .await?;
    match group_ids.iter().find(|id| !known.contains(id)) {
        Some(missing) => Err(GroupError::UnknownGroup(*missing)),
        None => Ok(()),
    }
}

/// Group names by agent, for the agent list.
pub async fn names_by_agent(pool: &PgPool) -> sqlx::Result<HashMap<String, Vec<String>>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT m.agent_id, g.name FROM agent_group_members m
         JOIN agent_groups g ON g.id = m.group_id ORDER BY g.name",
    )
    .fetch_all(pool)
    .await?;
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for (agent, name) in rows {
        map.entry(agent).or_default().push(name);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_trimmed_and_bounded() {
        assert_eq!(check_name("  Branch A ").unwrap(), "Branch A");
        assert!(check_name("   ").is_err());
        assert!(check_name(&"x".repeat(65)).is_err());
        assert!(check_name(&"é".repeat(64)).is_ok(), "characters, not bytes");
        assert!(check_name("tab\there").is_err());
        assert!(check_description(&"d".repeat(501)).is_err());
        assert_eq!(check_description(" servers ").unwrap(), "servers");
    }
}
