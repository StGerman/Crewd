//! Jira's issue payload, trimmed to what this adapter reads, and its mapping onto [`Issue`].
//!
//! Every field below is optional or defaulted: `fields` is asked for by name in every request
//! this adapter makes, but a custom Jira workflow can still omit one (a screen without a
//! priority field, an issue with no assignee), and a payload this tolerant never fails to parse
//! over a shape difference the scheduler does not care about.

use std::sync::LazyLock;

use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;

use super::adf;
use crate::model::Issue;
use crate::tracker::normalize_labels;

/// Every field `JiraTracker` asks Jira for, on both the search and the single-issue read — one
/// list, so the two requests can never drift onto different shapes of the same issue.
pub(crate) const FIELDS: &[&str] = &[
    "summary",
    "description",
    "status",
    "priority",
    "labels",
    "created",
    "issuelinks",
    "project",
    "assignee",
];

#[derive(Debug, Deserialize)]
pub(crate) struct JiIssue {
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) fields: JiFields,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct JiFields {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    description: Option<Value>,
    #[serde(default)]
    status: Option<JiStatus>,
    #[serde(default)]
    priority: Option<JiPriority>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    created: Option<String>,
    #[serde(default)]
    issuelinks: Vec<JiLink>,
    #[serde(default)]
    pub(crate) project: Option<JiProject>,
    #[serde(default)]
    assignee: Option<JiUser>,
}

#[derive(Debug, Deserialize)]
struct JiStatus {
    name: String,
}

#[derive(Debug, Deserialize)]
struct JiPriority {
    name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct JiProject {
    pub(crate) key: String,
}

#[derive(Debug, Deserialize)]
struct JiUser {
    #[serde(rename = "accountId")]
    account_id: String,
}

#[derive(Debug, Deserialize)]
struct JiLink {
    #[serde(rename = "type")]
    kind: JiLinkType,
    #[serde(default, rename = "inwardIssue")]
    inward_issue: Option<JiLinkedIssue>,
}

#[derive(Debug, Deserialize)]
struct JiLinkType {
    name: String,
}

#[derive(Debug, Deserialize)]
struct JiLinkedIssue {
    key: String,
}

/// Highest/Blocker through Lowest/Trivial, in that order. Never by Jira's own priority id: a
/// site's ids are creation order, not severity order — GETT's own "Trivial" is id 10000.
fn priority_rank(name: &str) -> Option<i32> {
    match name.to_lowercase().as_str() {
        "highest" | "blocker" => Some(1),
        "high" | "critical" => Some(2),
        "medium" | "major" => Some(3),
        "low" | "minor" => Some(4),
        "lowest" | "trivial" => Some(5),
        _ => None,
    }
}

/// Jira's colonless-offset format, e.g. `2026-09-27T08:32:31.410+0300`, parsed once rather than
/// on every call: the description is a fixed literal, so re-parsing it bought nothing but an
/// `expect` that could only fail if this literal itself were wrong. `None` only if the literal
/// stops parsing, which `created_at_parses_jiras_colonless_offset` would catch.
static JIRA_CREATED_FORMAT: LazyLock<Option<Vec<BorrowedFormatItem<'static>>>> =
    LazyLock::new(|| {
        time::format_description::parse_borrowed::<2>(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]\
         [offset_hour sign:mandatory][offset_minute]",
        )
        .ok()
    });

/// Jira's `2026-09-27T08:32:31.410+0300`: a four-digit, colonless offset that is not valid RFC
/// 3339. Tried first; RFC 3339 is the fallback for a site or a webhook payload that sends the
/// standard form instead.
fn parse_created_at(s: &str) -> Option<i64> {
    JIRA_CREATED_FORMAT
        .as_ref()
        .and_then(|fmt| OffsetDateTime::parse(s, fmt).ok())
        .or_else(|| OffsetDateTime::parse(s, &Rfc3339).ok())
        .map(|t| t.unix_timestamp() * 1000)
}

fn render_description(doc: &Value) -> Option<String> {
    if doc.is_null() {
        return None;
    }
    let rendered = adf::render(doc);
    (!rendered.is_empty()).then_some(rendered)
}

/// Everything [`to_issue`] needs beyond the payload itself, gathered into one value rather than
/// five positional arguments — `base_url`, `project`, `dispatch_label` and the resolved `me`
/// travel together on every call this adapter makes.
pub(crate) struct MapContext<'a> {
    pub(crate) base_url: &'a str,
    pub(crate) project: &'a str,
    pub(crate) dispatch_label: &'a str,
    /// The token owner's `accountId`, already resolved by the caller when `assigned_to_me`
    /// needs it — this function never fetches anything itself, so it stays a pure mapping.
    pub(crate) me: Option<&'a str>,
    pub(crate) assigned_to_me: bool,
}

/// Maps one Jira payload onto the scheduler's [`Issue`].
pub(crate) fn to_issue(ctx: &MapContext<'_>, raw: JiIssue) -> Issue {
    let key = raw.key;
    let fields = raw.fields;
    let labels = normalize_labels(fields.labels);
    let assignee_id = fields.assignee.map(|a| a.account_id);
    let dispatchable = labels.iter().any(|l| l == ctx.dispatch_label)
        && (!ctx.assigned_to_me || assignee_id.as_deref() == ctx.me);
    let project_key = fields.project.map(|p| p.key).unwrap_or_else(|| ctx.project.to_string());
    let blocked_by = fields
        .issuelinks
        .into_iter()
        .filter(|l| l.kind.name.eq_ignore_ascii_case("blocks"))
        .filter_map(|l| l.inward_issue.map(|i| i.key))
        .collect();

    Issue {
        id: key.clone(),
        identifier: key.clone(),
        title: fields.summary,
        body: fields.description.as_ref().and_then(render_description),
        state: fields.status.map(|s| s.name).unwrap_or_default(),
        priority: fields.priority.and_then(|p| priority_rank(&p.name)),
        url: Some(format!("{}/browse/{key}", ctx.base_url)),
        labels,
        dispatchable,
        created_at: fields.created.as_deref().and_then(parse_created_at),
        native_ref: Some(json!({ "key": key, "project": project_key })),
        blocked_by,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(json: Value) -> JiIssue {
        serde_json::from_value(json).unwrap()
    }

    fn ctx<'a>(me: Option<&'a str>, assigned_to_me: bool) -> MapContext<'a> {
        MapContext {
            base_url: "https://x.atlassian.net",
            project: "PROJ",
            dispatch_label: "crewd",
            me,
            assigned_to_me,
        }
    }

    #[test]
    fn a_jira_issue_maps_key_status_priority_links_and_body() {
        let raw = raw(serde_json::json!({
            "key": "PROJ-7",
            "fields": {
                "summary": "Fix the thing",
                "description": {
                    "type": "doc", "version": 1,
                    "content": [{"type": "paragraph", "content": [{"type": "text", "text": "details"}]}]
                },
                "status": {"name": "In Progress"},
                "priority": {"name": "High"},
                "labels": ["Crewd", " bug "],
                "created": "2026-09-27T08:32:31.410+0300",
                "issuelinks": [
                    {"type": {"name": "Blocks"}, "inwardIssue": {"key": "PROJ-1"}},
                    {"type": {"name": "Relates"}, "outwardIssue": {"key": "PROJ-2"}}
                ],
                "project": {"key": "PROJ"},
                "assignee": {"accountId": "acc-1"}
            }
        }));
        let issue = to_issue(&ctx(Some("acc-1"), true), raw);
        insta::assert_snapshot!(format!("{issue:#?}"));
    }

    #[test]
    fn created_at_parses_jiras_colonless_offset() {
        assert!(parse_created_at("2026-09-27T08:32:31.410+0300").is_some());
        assert!(parse_created_at("2026-09-27T08:32:31.410+00:00").is_some(), "rfc 3339 fallback");
        assert_eq!(parse_created_at("not a date"), None);
    }

    #[test]
    fn dispatchability_requires_the_label_and_the_assignment_when_assigned_to_me_is_set() {
        let with_assignee = |acc: Option<&str>| {
            raw(serde_json::json!({
                "key": "PROJ-1",
                "fields": {
                    "summary": "s", "labels": ["crewd"],
                    "assignee": acc.map(|a| serde_json::json!({"accountId": a}))
                }
            }))
        };
        let mine = to_issue(&ctx(Some("me"), true), with_assignee(Some("me")));
        assert!(mine.dispatchable);
        let theirs = to_issue(&ctx(Some("me"), true), with_assignee(Some("them")));
        assert!(!theirs.dispatchable, "reassigned away from the token owner");
        let unassigned = to_issue(&ctx(Some("me"), true), with_assignee(None));
        assert!(!unassigned.dispatchable);
        let no_flag = to_issue(&ctx(Some("me"), false), with_assignee(Some("them")));
        assert!(no_flag.dispatchable, "without assigned_to_me any assignee is irrelevant");
    }

    #[test]
    fn jira_labels_are_normalized_at_the_adapter_boundary() {
        let raw = raw(serde_json::json!({
            "key": "PROJ-1",
            "fields": {"summary": "s", "labels": ["Crewd", " Bug ", "crewd"]}
        }));
        let issue = to_issue(&ctx(None, false), raw);
        assert_eq!(issue.labels, vec!["crewd", "bug"]);
    }
}
