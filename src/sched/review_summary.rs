//! Review summaries as findings (#126).
//!
//! Delivery used to read only a pull request's inline comments, so a finding a reviewer put in
//! a review's summary — Copilot's "Previously missed", or a human's `CHANGES_REQUESTED` with no
//! line to hang it on — never reached an agent and never held `ready`. Here a summary becomes a
//! [`ReviewComment`] keyed by its review ([`SUMMARY_PREFIX`]), and from there it has the same
//! lifecycle as an inline comment: handed back in a round, settled once by a verdict in
//! `review_verdict`, never re-argued.
//!
//! The summary is handed over whole. Copilot's format is not a published contract, and a parser
//! for its sections that drifted would drop findings silently; a noisy summary costs one
//! `rejected` verdict instead. The only text recognised is the one that says there is nothing:
//! a body that is "Findings: None" once Copilot's template is set aside — headings, HTML
//! comments and tags, the "Review effort" line, and its section labels (#201). Any prose left
//! over is a finding, except the overview sentence of Copilot's approval (#234); a template line
//! counted as one spent a delivery round on every review.

use crate::forge::{Review, ReviewComment, SUMMARY_PREFIX, summary_review_id};

/// The summaries on `head` that carry a finding, oldest first: from a login in
/// `summary_reviewers`, or in the `CHANGES_REQUESTED` state whoever wrote it. A review on an
/// earlier head was about code a later push replaced, so it is not handed back.
pub(super) fn summary_findings(
    reviews: &[Review],
    head: &str,
    summary_reviewers: &[String],
) -> Vec<ReviewComment> {
    reviews
        .iter()
        .filter(|r| r.commit_sha == head)
        .filter(|r| r.state == "CHANGES_REQUESTED" || summary_reviewers.contains(&r.reviewer))
        .filter(|r| carries_findings(&r.body))
        .map(|r| ReviewComment {
            id: format!("{SUMMARY_PREFIX}{}", r.id),
            author: r.reviewer.clone(),
            path: None,
            line: None,
            body: r.body.clone(),
            url: r.url.clone(),
        })
        .collect()
}

/// Whether `body` says anything beyond "Findings: None". The one prose allowed is the overview
/// sentence under Copilot's approval (`🟢 Approved`), and only in a summary that also says
/// "Findings: None": that sentence is on every approval, and handing it back spent a delivery
/// round on each (#234). Prose under any other status, or in any other section, still counts.
fn carries_findings(body: &str) -> bool {
    let mut in_html_comment = false;
    let mut under_approval = false;
    let mut says_none = false;
    let mut overview = false;
    for line in body.lines() {
        let mut rest = line.trim();
        // HTML comments are markers for the tool that wrote them, never text for a reader.
        let mut text = String::new();
        loop {
            if in_html_comment {
                match rest.find("-->") {
                    Some(i) => {
                        in_html_comment = false;
                        rest = &rest[i + 3..];
                    }
                    None => break,
                }
            } else {
                match rest.find("<!--") {
                    Some(i) => {
                        text.push_str(&rest[..i]);
                        in_html_comment = true;
                        rest = &rest[i + 4..];
                    }
                    None => {
                        text.push_str(rest);
                        break;
                    }
                }
            }
        }
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if let Some(heading) = text.strip_prefix('#') {
            under_approval = bare(heading.trim_start_matches('#')) == "🟢 approved";
            continue;
        }
        let bare = bare(text);
        if text.starts_with('<') {
            // A `<details>` block is a section of its own, never the overview.
            under_approval = false;
        }
        if bare == "findings: none" {
            says_none = true;
            continue;
        }
        let template = bare.is_empty()
            || bare.starts_with("review effort:")
            || bare == "in code that hasn't changed since last review"
            || (text.starts_with('<') && is_section_label(&bare));
        if template {
            continue;
        }
        if !under_approval {
            return true;
        }
        overview = true;
    }
    overview && !says_none
}

/// `text` as compared against the template: tags and emphasis gone, the curly apostrophe
/// straightened, whitespace collapsed, lowercase.
fn bare(text: &str) -> String {
    let bare: String = strip_tags(text)
        .chars()
        .filter(|c| !matches!(c, '*' | '_'))
        .map(|c| if c == '\u{2019}' { '\'' } else { c })
        .collect::<String>()
        .to_lowercase();
    bare.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Treating comparison prose such as "a < b" as markup would silently drop a finding, so only a
/// `<` followed by a letter or `/` opens a tag here; `text` comes back without its HTML tags.
fn strip_tags(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let opens_tag = after.starts_with(|c: char| c.is_ascii_alphabetic() || c == '/');
        match after.find('>') {
            Some(j) if opens_tag => rest = &after[j + 1..],
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Whether `bare` is a `<summary>` heading such as "Open (1)" or "Previously missed (5)": the
/// entries under it are what carry a finding, so the heading alone must not.
fn is_section_label(bare: &str) -> bool {
    let Some(head) = bare.strip_suffix(')') else { return false };
    let Some((words, count)) = head.rsplit_once(" (") else { return false };
    !count.is_empty()
        && count.chars().all(|c| c.is_ascii_digit())
        && words.chars().all(|c| c.is_alphabetic() || c == ' ')
}

/// What is posted on the pull request when a summary's verdict lands: the verdict, and the
/// finding it answers quoted, since a summary has no thread for the verdict to sit under.
/// `review` is `None` when the review is no longer there to quote.
pub(super) fn verdict_comment(comment_id: &str, review: Option<&Review>, verdict: &str) -> String {
    let who = review.map_or("the reviewer", |r| r.reviewer.as_str());
    let id = summary_review_id(comment_id).unwrap_or(comment_id);
    let mut s = match review.and_then(|r| r.url.as_deref()) {
        Some(url) => format!("On [the review summary]({url}) by {who}:\n\n"),
        None => format!("On review {id} by {who}:\n\n"),
    };
    if let Some(r) = review {
        for line in excerpt(&r.body).lines() {
            s.push_str(&format!("> {line}\n"));
        }
        s.push('\n');
    }
    s.push_str(verdict);
    s
}

/// The start of a summary, bounded: a quote is there to say which finding is answered, and the
/// review itself is one link away.
fn excerpt(body: &str) -> String {
    const MAX: usize = 600;
    let body = body.trim();
    if body.len() <= MAX {
        return body.to_string();
    }
    let cut = (0..=MAX).rev().find(|i| body.is_char_boundary(*i)).unwrap_or(0);
    format!("{}…", &body[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(id: &str, reviewer: &str, state: &str, head: &str, body: &str) -> Review {
        Review {
            id: id.into(),
            reviewer: reviewer.into(),
            commit_sha: head.into(),
            state: state.into(),
            body: body.into(),
            url: None,
        }
    }

    const COPILOT: &str = "copilot-pull-request-reviewer[bot]";

    #[test]
    fn a_summary_that_is_only_findings_none_carries_no_finding() {
        assert!(!carries_findings(""));
        assert!(!carries_findings("  \n\n"));
        assert!(!carries_findings("**Findings:** None"));
        assert!(!carries_findings(
            "<!-- ccr-overview-v2 -->\n\n## Copilot review overview\n\n**Findings:** None\n"
        ));
        assert!(!carries_findings("<!-- a\nmultiline marker -->\nFindings:   none"));
    }

    #[test]
    fn a_summary_with_anything_beside_findings_none_is_a_finding() {
        assert!(carries_findings(
            "## Copilot review overview\n\n### Needs a closer look\n\nCorrect the dirty-tree \
             no-rebase status.\n\n**Findings:** None"
        ));
        assert!(carries_findings("Previously missed: src/gate/git.rs:325 reports on_base: false"));
    }

    /// Counting this template as a finding spent a delivery round on every Copilot review (#201):
    /// it is Copilot's summary as posted, with the overview sentence and entries cut.
    const TEMPLATE: &str = "<!-- ccr-overview-v2 -->\n\n## Copilot review overview\n\n\
        ### 🟢 Approved\n\n**Review effort:** Balanced  \n**Findings:** None\n\n\
        <details>\n<summary><strong>Previously missed (0)</strong></summary>\n\n\
        In code that hasn’t changed since last review\n<br>\n\n</details>\n";

    #[test]
    fn a_copilot_summary_that_is_only_its_template_is_not_a_finding() {
        assert!(!carries_findings(TEMPLATE));
        assert!(!carries_findings("*Review effort:* Lite\n<details open>\n</details>"));
        let reviews = [review("1", COPILOT, "COMMENTED", "head", TEMPLATE)];
        assert!(summary_findings(&reviews, "head", &[COPILOT.to_string()]).is_empty());
    }

    #[test]
    fn a_summary_with_an_overview_sentence_is_still_a_finding() {
        let body = TEMPLATE.replace(
            "### 🟢 Approved\n\n",
            "### 🔵 Needs a closer look\n\nThe guard can still judge an older stale head.\n\n",
        );
        let reviews = [review("1", COPILOT, "COMMENTED", "head", &body)];
        assert_eq!(summary_findings(&reviews, "head", &[COPILOT.to_string()]).len(), 1);
        assert!(carries_findings("a < b is <b>still</b> said"));
    }

    /// PR #229's round 3 of 3 was spent on this summary (#234).
    #[test]
    fn an_approving_copilot_summary_with_no_findings_is_not_a_finding() {
        let body = "### 🟢 Approved\nThe fail-fast paths are consistently propagated, documented, \
            and covered by focused startup tests.\n**Findings:** None";
        assert!(!carries_findings(body));
        let with_template = TEMPLATE.replace(
            "### 🟢 Approved\n\n",
            "### 🟢 Approved\n\nThe fail-fast paths are consistently propagated.\n\n",
        );
        let reviews = [review("1", COPILOT, "COMMENTED", "head", &with_template)];
        assert!(summary_findings(&reviews, "head", &[COPILOT.to_string()]).is_empty());
    }

    #[test]
    fn an_approval_is_still_a_finding_without_findings_none_or_with_another_section() {
        assert!(carries_findings("### 🟢 Approved\nLooks good.\n**Findings:** 1"));
        assert!(carries_findings("### 🟢 Approved\nLooks good."));
        assert!(carries_findings(
            "### 🟢 Approved\nLooks good.\n**Findings:** None\n### Notes\nRename the guard."
        ));
        assert!(carries_findings(
            "### 🟢 Approved\nLooks good.\n**Findings:** None\n<details>\n\
             <summary>Open (1)</summary>\nRename the guard.\n</details>"
        ));
    }

    #[test]
    fn a_summary_with_a_previously_missed_entry_is_still_a_finding() {
        let body = TEMPLATE.replace(
            "<br>\n",
            "<details>\n<summary><picture><img src=\"low.png\" alt=\"Low severity\"></picture> \
             Doc comment should lead with the failure</summary>\n\n`src/workspace.rs:435`\n\
             </details>\n",
        );
        let reviews = [review("1", COPILOT, "COMMENTED", "head", &body)];
        assert_eq!(summary_findings(&reviews, "head", &[COPILOT.to_string()]).len(), 1);
    }

    #[test]
    fn only_copilot_and_changes_requested_summaries_on_the_head_are_findings() {
        let reviewers = vec![COPILOT.to_string()];
        let reviews = vec![
            review("1", COPILOT, "COMMENTED", "old", "stale finding"),
            review("2", COPILOT, "COMMENTED", "head", "a finding"),
            review("3", COPILOT, "COMMENTED", "head", "**Findings:** None"),
            review("4", "alice", "COMMENTED", "head", "a thought"),
            review("5", "alice", "APPROVED", "head", "looks fine"),
            review("6", "bob", "CHANGES_REQUESTED", "head", "please split this"),
            review("7", "bob", "CHANGES_REQUESTED", "head", ""),
        ];
        let got = summary_findings(&reviews, "head", &reviewers);
        let ids: Vec<&str> = got.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["review-2", "review-6"]);
        assert_eq!(got[1].author, "bob");
        assert_eq!(got[1].path, None);
    }

    #[test]
    fn a_verdict_comment_quotes_the_finding_it_answers() {
        let r = review("9", COPILOT, "COMMENTED", "head", "line one\nline two");
        let s = verdict_comment("review-9", Some(&r), "**Rejected** — noise.");
        assert!(s.contains("> line one\n> line two\n"), "{s}");
        assert!(s.ends_with("**Rejected** — noise."), "{s}");
        let gone = verdict_comment("review-9", None, "**Rejected** — noise.");
        assert!(gone.starts_with("On review 9 by the reviewer"), "{gone}");
    }
}
