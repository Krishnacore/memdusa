//! Per-project access control and the audit log.
//!
//! Grants are matched by exact project key first, then by the longest `/*`
//! prefix. A `none` grant carves a project out of a broader rule. Admins (and
//! the shared token / an open server / local access, all `Identity::root`)
//! bypass grants entirely.
//!
//! `AccessPolicy` is a snapshot: the HTTP layer builds one per request, so a
//! grant change takes effect on the next request without re-querying for every
//! endpoint a request touches.

use sqlx::Row;
use sqlx::any::AnyRow;

use crate::model::*;
use crate::store::{Q, Store};
use crate::{Error, Result};

/// Audit events are kept this many days by default (`MEMDUSA_AUDIT_DAYS`).
pub const DEFAULT_AUDIT_DAYS: i64 = 365;
pub const AUDIT_DAYS_ENV: &str = "MEMDUSA_AUDIT_DAYS";

pub fn audit_retention_days() -> i64 {
    std::env::var(AUDIT_DAYS_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|d| *d >= 0)
        .unwrap_or(DEFAULT_AUDIT_DAYS)
}

/// Validate a grant pattern: a plain project key, or a prefix ending in `/*`.
fn validate_pattern(pattern: &str) -> Result<()> {
    let p = pattern.trim();
    if p.is_empty() || p.contains("**") || (p.contains('*') && !p.ends_with("/*")) {
        return Err(Error::Invalid(format!(
            "invalid grant pattern '{pattern}': use a project key or a prefix ending in /*"
        )));
    }
    Ok(())
}

/// An immutable snapshot of the default policy plus one user's grants.
#[derive(Debug, Clone)]
pub struct AccessPolicy {
    default: DefaultPolicy,
    grants: Vec<ProjectGrant>,
}

impl AccessPolicy {
    pub fn new(default: DefaultPolicy, grants: Vec<ProjectGrant>) -> Self {
        Self { default, grants }
    }

    pub fn default_policy(&self) -> DefaultPolicy {
        self.default
    }

    pub fn grants(&self) -> &[ProjectGrant] {
        &self.grants
    }

    /// Pure matching logic: exact beats prefix, longest prefix wins, `none`
    /// role overrides the default (and a broader prefix).
    pub fn access(&self, identity: &Identity, project: &str) -> Access {
        if identity.admin {
            return Access::Write;
        }
        let mut exact: Option<&ProjectGrant> = None;
        let mut prefix: Option<(&ProjectGrant, usize)> = None;
        for g in &self.grants {
            if g.pattern == project {
                if exact.is_none_or(|e| (g.created_at, g.id.as_str()) > (e.created_at, e.id.as_str())) {
                    exact = Some(g);
                }
            } else if let Some(head) = g.pattern.strip_suffix("/*")
                && project.starts_with(head)
            {
                let len = head.len();
                if prefix.is_none_or(|(e, n)| (len, g.created_at, g.id.as_str()) > (n, e.created_at, e.id.as_str())) {
                    prefix = Some((g, len));
                }
            }
        }
        exact.or_else(|| prefix.map(|(g, _)| g)).map_or_else(|| self.default.access(), |g| g.role.access())
    }

    /// Projects from `projects` that `identity` may read.
    pub fn readable(&self, identity: &Identity, projects: &[String]) -> Vec<String> {
        projects.iter().filter(|p| self.access(identity, p).satisfies(Access::Read)).cloned().collect()
    }

    /// Pure check; callers audit denials through the store.
    pub fn allows(&self, identity: &Identity, project: &str, needed: Access) -> bool {
        self.access(identity, project).satisfies(needed)
    }
}

fn grant_from_row(r: &AnyRow) -> Result<ProjectGrant> {
    Ok(ProjectGrant {
        id: r.try_get("id")?,
        pattern: r.try_get("pattern")?,
        user: r.try_get("user_name")?,
        role: Role::parse(&r.try_get::<String, _>("role")?).unwrap_or(Role::None),
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
    })
}

fn audit_from_row(r: &AnyRow) -> Result<AuditEvent> {
    Ok(AuditEvent {
        id: r.try_get("id")?,
        at: r.try_get("at")?,
        actor: r.try_get("actor")?,
        action: r.try_get("action")?,
        project: r.try_get("project")?,
        target: r.try_get("target")?,
        detail: r.try_get::<Option<String>, _>("detail")?.and_then(|s| serde_json::from_str(&s).ok()),
        outcome: AuditOutcome::parse(&r.try_get::<String, _>("outcome")?),
    })
}

impl Store {
    // ---------------------------------------------------------------- grants

    /// Grants of one user, or everyone's when `user` is `None`.
    pub async fn grants_for_user(&self, user: Option<&str>) -> Result<Vec<ProjectGrant>> {
        let mut q = Q::new("SELECT * FROM project_grants");
        if let Some(u) = user {
            let u = q.text(u);
            q.push(&format!(" WHERE user_name = {u}"));
        }
        q.push(" ORDER BY pattern, created_at");
        q.build().fetch_all(&self.pool).await?.iter().map(grant_from_row).collect()
    }

    pub async fn grant_get(&self, id: &str) -> Result<ProjectGrant> {
        let mut q = Q::new("SELECT * FROM project_grants WHERE id = ");
        let id = q.text(id);
        q.push(&id);
        let row = q.build().fetch_optional(&self.pool).await?.ok_or_else(|| Error::NotFound(format!("grant {id}")))?;
        grant_from_row(&row)
    }

    pub async fn grant_create(&self, pattern: &str, user: &str, role: Role, created_by: &str) -> Result<ProjectGrant> {
        validate_pattern(pattern)?;
        let g = ProjectGrant {
            id: new_id(),
            pattern: pattern.trim().into(),
            user: user.into(),
            role,
            created_by: created_by.into(),
            created_at: now_ms(),
        };
        let mut q =
            Q::new("INSERT INTO project_grants (id, pattern, user_name, role, created_by, created_at) VALUES (");
        let vals = [
            q.text(&g.id),
            q.text(&g.pattern),
            q.text(&g.user),
            q.text(g.role.as_str()),
            q.text(&g.created_by),
            q.int(g.created_at),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(g)
    }

    pub async fn grant_delete(&self, id: &str) -> Result<ProjectGrant> {
        let g = self.grant_get(id).await?;
        let mut q = Q::new("DELETE FROM project_grants WHERE id = ");
        let id = q.text(id);
        q.push(&id);
        q.build().execute(&self.pool).await?;
        Ok(g)
    }

    /// One grants query per request; the returned snapshot answers any number
    /// of `authorize` calls without touching the database again.
    pub async fn access_policy(&self, user: Option<&str>, default: DefaultPolicy) -> Result<AccessPolicy> {
        Ok(AccessPolicy::new(default, self.grants_for_user(user).await?))
    }

    /// Effective access of `identity` on one project.
    pub async fn effective_access(&self, identity: &Identity, project: &str, default: DefaultPolicy) -> Result<Access> {
        let policy = self.access_policy(identity.user.as_deref(), default).await?;
        Ok(policy.access(identity, project))
    }

    /// Authorize with the server setting (`MEMDUSA_DEFAULT_ACCESS`). Denials
    /// return `Error::Forbidden` and are written to the audit log.
    pub async fn authorize(&self, identity: &Identity, project: &str, needed: Access) -> Result<()> {
        let policy = self.access_policy(identity.user.as_deref(), DefaultPolicy::from_env()).await?;
        if policy.allows(identity, project, needed) {
            return Ok(());
        }
        self.audit(
            NewAuditEvent::denied(identity.actor(), "denied")
                .project(project)
                .detail(serde_json::json!({ "needed": needed.as_str() })),
        )
        .await?;
        Err(Error::Forbidden(format!("access denied to project '{project}' (needs {})", needed.as_str())))
    }

    // ---------------------------------------------------------------- audit

    /// Append one audit event (detail is redacted like any other text).
    pub async fn audit(&self, e: NewAuditEvent) -> Result<AuditEvent> {
        let ev = AuditEvent {
            id: new_id(),
            at: now_ms(),
            actor: self.redactor().string(e.actor),
            action: e.action,
            project: e.project.map(|p| self.redactor().string(p)),
            target: e.target.map(|t| self.redactor().string(t)),
            detail: self.redactor().opt_json(e.detail),
            outcome: e.outcome,
        };
        let mut q =
            Q::new("INSERT INTO audit_events (id, at, actor, action, project, target, detail, outcome) VALUES (");
        let vals = [
            q.text(&ev.id),
            q.int(ev.at),
            q.text(&ev.actor),
            q.text(&ev.action),
            q.opt_text(ev.project.clone()),
            q.opt_text(ev.target.clone()),
            q.opt_text(ev.detail.as_ref().map(|v| v.to_string())),
            q.text(ev.outcome.as_str()),
        ];
        q.push(&vals.join(", ")).push(")");
        q.build().execute(&self.pool).await?;
        Ok(ev)
    }

    pub async fn audit_query(&self, f: AuditQuery) -> Result<Vec<AuditEvent>> {
        let mut q = Q::new("SELECT * FROM audit_events WHERE 1=1");
        if let Some(since) = f.since {
            let t = q.int(since);
            q.push(&format!(" AND at >= {t}"));
        }
        if let Some(actor) = f.actor {
            let a = q.text(actor);
            q.push(&format!(" AND actor = {a}"));
        }
        if let Some(project) = f.project {
            let p = q.text(project);
            q.push(&format!(" AND project = {p}"));
        }
        let l = q.int(f.limit.unwrap_or(100).clamp(1, 1000));
        q.push(&format!(" ORDER BY at DESC, id DESC LIMIT {l}"));
        q.build().fetch_all(&self.pool).await?.iter().map(audit_from_row).collect()
    }

    /// Delete audit events at or before `before`; returns the number removed.
    pub async fn audit_delete_before(&self, before: Millis) -> Result<usize> {
        let mut q = Q::new("DELETE FROM audit_events WHERE at <= ");
        let t = q.int(before);
        q.push(&t);
        Ok(q.build().execute(&self.pool).await?.rows_affected() as usize)
    }

    // ---------------------------------------------------------------- listings

    /// All distinct project keys across the data tables.
    pub async fn distinct_projects(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT DISTINCT project FROM memories WHERE project IS NOT NULL \
             UNION SELECT DISTINCT project FROM events \
             UNION SELECT DISTINCT project FROM checkpoints \
             UNION SELECT DISTINCT project FROM runs \
             ORDER BY project",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>("project"))
            .collect::<std::result::Result<Vec<_>, sqlx::Error>>()?)
    }

    /// Projects the caller may read, for filtering cross-project listings.
    pub async fn readable_projects(&self, identity: &Identity, default: DefaultPolicy) -> Result<Vec<String>> {
        let policy = self.access_policy(identity.user.as_deref(), default).await?;
        Ok(policy.readable(identity, &self.distinct_projects().await?))
    }

    /// A memory's project: `None` for a global memory (private to its owner).
    pub async fn memory_project(&self, id: &str) -> Result<Option<String>> {
        let mut q = Q::new("SELECT project FROM memories WHERE id = ");
        let id = q.text(id);
        q.push(&id);
        let row = q.build().fetch_optional(&self.pool).await?.ok_or_else(|| Error::NotFound(format!("memory {id}")))?;
        Ok(row.try_get("project")?)
    }

    /// The project a run belongs to. Run/step endpoints authorize against this
    /// stored value, never a caller-supplied one.
    pub async fn run_project(&self, id: &str) -> Result<String> {
        let mut q = Q::new("SELECT project FROM runs WHERE id = ");
        let p = q.text(id);
        q.push(&p);
        // report the run id, not the SQL placeholder (`$1`) the shadowed binding used to print
        let row = q.build().fetch_optional(&self.pool).await?.ok_or_else(|| Error::NotFound(format!("run {id}")))?;
        Ok(row.try_get("project")?)
    }

    /// Owner of a run, for auditing finishes by someone other than the owner.
    pub async fn run_owner(&self, id: &str) -> Result<Option<String>> {
        let mut q = Q::new("SELECT owner FROM runs WHERE id = ");
        let p = q.text(id);
        q.push(&p);
        // report the run id, not the SQL placeholder (`$1`) the shadowed binding used to print
        let row = q.build().fetch_optional(&self.pool).await?.ok_or_else(|| Error::NotFound(format!("run {id}")))?;
        Ok(row.try_get("owner")?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(pattern: &str, role: Role, at: i64) -> ProjectGrant {
        ProjectGrant {
            id: new_id(),
            pattern: pattern.into(),
            user: "alice".into(),
            role,
            created_by: "root".into(),
            created_at: at,
        }
    }

    #[test]
    fn pattern_precedence_and_none_carve_out() {
        let alice = Identity { user: Some("alice".into()), admin: false };
        let p = AccessPolicy::new(
            DefaultPolicy::Write,
            vec![
                grant("github.com/acme/*", Role::Write, 1),
                grant("github.com/acme/payroll", Role::None, 2),
                grant("github.com/acme/*/docs", Role::Read, 3),
            ],
        );
        assert_eq!(p.access(&alice, "github.com/acme/shop"), Access::Write, "prefix");
        assert_eq!(p.access(&alice, "github.com/acme/payroll"), Access::None, "exact none carve-out");
        assert_eq!(p.access(&alice, "other/repo"), Access::Write, "default");
        assert_eq!(p.access(&Identity::root(), "github.com/acme/payroll"), Access::Write, "admin bypass");

        // exact beats prefix, even when the prefix is longer for another project
        let p = AccessPolicy::new(
            DefaultPolicy::None,
            vec![grant("github.com/acme/*", Role::Write, 1), grant("github.com/acme/billing", Role::Read, 2)],
        );
        assert_eq!(p.access(&alice, "github.com/acme/billing"), Access::Read);
        assert_eq!(p.access(&alice, "github.com/acme/other"), Access::Write);

        // longest prefix wins among prefixes
        let p = AccessPolicy::new(
            DefaultPolicy::None,
            vec![grant("github.com/*", Role::Read, 5), grant("github.com/acme/*", Role::Write, 1)],
        );
        assert_eq!(p.access(&alice, "github.com/acme/shop"), Access::Write);
        assert_eq!(p.access(&alice, "github.com/other/shop"), Access::Read);
    }

    #[test]
    fn default_policies() {
        let alice = Identity { user: Some("alice".into()), admin: false };
        for (policy, expected) in [
            (DefaultPolicy::Write, Access::Write),
            (DefaultPolicy::Read, Access::Read),
            (DefaultPolicy::None, Access::None),
        ] {
            let p = AccessPolicy::new(policy, vec![]);
            assert_eq!(p.access(&alice, "any/project"), expected, "{policy:?}");
        }
        // a grant overrides every default
        let p = AccessPolicy::new(DefaultPolicy::None, vec![grant("any/*", Role::Read, 1)]);
        assert_eq!(p.access(&alice, "any/project"), Access::Read);
    }

    #[test]
    fn role_and_policy_round_trip() {
        for s in ["read", "write", "none"] {
            assert_eq!(Role::parse(s).unwrap().as_str(), s);
            assert_eq!(DefaultPolicy::parse(s).unwrap().as_str(), s);
        }
        assert!(Role::parse("admin").is_none());
        assert!(validate_pattern("github.com/acme/*").is_ok());
        assert!(validate_pattern("github.com/acme/billing").is_ok());
        assert!(validate_pattern("bad*pattern").is_err());
    }
}
