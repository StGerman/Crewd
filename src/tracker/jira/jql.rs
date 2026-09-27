//! JQL construction for the dispatch poll (#99).
//!
//! Verified live against a Jira Cloud site: JQL compares `status` and `labels` without regard
//! to case, so the lowercased config values `Config::normalize` already produces work unmodified
//! — no case-folding of the operator's own state names is needed here.

/// Wraps `s` in double quotes, escaping `\` and `"` — the two characters JQL's string literal
/// syntax treats specially. Hand-rolled rather than a general JQL builder: this adapter only
/// ever quotes a project key, a status name or a label, never free text a user might type.
pub(crate) fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// The dispatch poll's query: every issue in `project` currently in one of `states`, carrying
/// `label`. `assigned_to_me` narrows the poll itself to the token owner's own assignments — the
/// per-issue `accountId` check in `issue::to_issue` still runs independently, because `by_ids`
/// never goes through this query at all.
pub(crate) fn poll_query(
    project: &str,
    states: &[String],
    label: &str,
    assigned_to_me: bool,
) -> String {
    let states = states.iter().map(|s| quote(s)).collect::<Vec<_>>().join(", ");
    let mut q = format!(
        "project = {} AND status in ({states}) AND labels = {}",
        quote(project),
        quote(label)
    );
    if assigned_to_me {
        q.push_str(" AND assignee = currentUser()");
    }
    q.push_str(" ORDER BY created ASC");
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jql_quotes_every_operator_supplied_value() {
        let states = vec!["in progress".to_string(), "in \"review\"".to_string()];
        let q = poll_query("PROJ", &states, "cr\\ewd", true);
        insta::assert_snapshot!(q);
    }

    #[test]
    fn assigned_to_me_off_omits_the_current_user_clause() {
        let states = vec!["open".to_string()];
        let q = poll_query("PROJ", &states, "crewd", false);
        insta::assert_snapshot!(q);
    }
}
