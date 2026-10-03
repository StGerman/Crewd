//! Review summaries and conversation comments as findings (#126, #263).
//!
//! Delivery used to read only a pull request's inline comments, so a finding a reviewer put in
//! a review's summary — Copilot's "Previously missed", or a human's `CHANGES_REQUESTED` with no
//! line to hang it on — never reached an agent and never held `ready`. Here a summary becomes a
//! [`ReviewComment`] keyed by its review ([`SUMMARY_PREFIX`]), and a comment on the pull
//! request's conversation one keyed by its id ([`CONVERSATION_PREFIX`]); from there each has the
//! same lifecycle as an inline comment: handed back in a round, settled once by a verdict in
//! `review_verdict`, never re-argued. Neither has a thread, so a verdict on one is a comment on
//! the pull request quoting it.
//!
//! Whoever wrote it, except crewd: its own verdict comments land in the same conversation, and
//! handing one back would have an agent settle its own answer, round after round (#263).
//!
//! The summary is handed over whole. Copilot's format is not a published contract, and a parser
//! for its sections that drifted would drop findings silently; a noisy summary costs one
//! `rejected` verdict instead. The only text recognised is the one that says there is nothing:
//! a body that is "Findings: None" (or "0") once Copilot's template is set aside — headings, HTML
//! comments and tags, the "Review effort" line, and its section labels (#201). Any prose left
//! over is a finding, except the overview sentence under Copilot's status heading, whatever its
//! emoji, when the count says there is none (#234, #280); a template line counted as one spent a
//! delivery round on every review.

use crate::config::COPILOT_REVIEWER;
use crate::forge::{CONVERSATION_PREFIX, CommentKind, Review, ReviewComment, SUMMARY_PREFIX};

/// The summaries on `head` that carry a finding, oldest first, from anyone but `own_login`. An
/// `APPROVED` review is not a request, and a review on an earlier head was about code a later
/// push replaced, so neither is handed back.
pub(super) fn summary_findings(
    reviews: &[Review],
    head: &str,
    own_login: &str,
) -> Vec<ReviewComment> {
    reviews
        .iter()
        .filter(|r| r.commit_sha == head && r.reviewer != own_login)
        .filter(|r| r.state == "COMMENTED" || r.state == "CHANGES_REQUESTED")
        .filter(|r| carries_findings(&r.body, r.reviewer == COPILOT_REVIEWER))
        .map(summary_comment)
        .collect()
}

/// A review's summary as the finding it is handed back as.
pub(super) fn summary_comment(r: &Review) -> ReviewComment {
    ReviewComment {
        id: format!("{SUMMARY_PREFIX}{}", r.id),
        author: r.reviewer.clone(),
        path: None,
        line: None,
        body: r.body.clone(),
        url: r.url.clone(),
    }
}

/// The conversation's comments as findings, keyed by [`CONVERSATION_PREFIX`], from anyone but
/// `own_login`. An empty one says nothing to settle.
pub(super) fn conversation_findings(
    comments: Vec<ReviewComment>,
    own_login: &str,
) -> Vec<ReviewComment> {
    comments
        .into_iter()
        .filter(|c| c.author != own_login && !c.body.trim().is_empty())
        .map(|c| ReviewComment { id: format!("{CONVERSATION_PREFIX}{}", c.id), ..c })
        .collect()
}

/// Whether `body` says anything beyond "Findings: None" (or "0"). The one prose allowed is the
/// overview sentence under Copilot's status heading (`🟢 Approved`, `🔵 Needs a closer look`,
/// `🟡 Changes recommended`, any status emoji), before that count, and only when `from_copilot`:
/// that sentence is on every summary, and handing it back spent a delivery round on each (#234,
/// #280). A second line of prose, prose under any other heading or after the count, anything in
/// another section, and the same heading from any other reviewer still count.
fn carries_findings(body: &str, from_copilot: bool) -> bool {
    let mut in_html_comment = false;
    let mut under_status = false;
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
            under_status =
                from_copilot && is_status_heading(&bare(heading.trim_start_matches('#')));
            continue;
        }
        let bare = bare(text);
        if text.starts_with('<') {
            // A `<details>` block is a section of its own, never the overview.
            under_status = false;
        }
        if bare == "findings: none" || bare == "findings: 0" {
            says_none = true;
            // The overview precedes the count; prose after it is something else.
            under_status = false;
            continue;
        }
        let template = bare.is_empty()
            || bare.starts_with("review effort:")
            || bare == "in code that hasn't changed since last review"
            || (text.starts_with('<') && is_section_label(&bare));
        if template {
            continue;
        }
        // The overview is one sentence; a second line is something else.
        if !under_status || overview {
            return true;
        }
        overview = true;
    }
    overview && !says_none
}

/// Whether `bare` is the heading Copilot puts its verdict under: a status emoji, then words, as
/// in "🟢 approved" or "🔵 needs a closer look".
fn is_status_heading(bare: &str) -> bool {
    const STATUS: [char; 9] = ['🟢', '🔵', '🟡', '🟠', '🔴', '🟣', '🟤', '⚪', '⚫'];
    let mut chars = bare.chars();
    chars.next().is_some_and(|c| STATUS.contains(&c))
        && chars.as_str().starts_with(' ')
        && chars.as_str().trim().chars().all(|c| c.is_alphabetic() || c == ' ')
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

/// What is posted on the pull request when a summary's or a conversation comment's verdict
/// lands: the verdict, and the finding it answers quoted, since neither has a thread for the
/// verdict to sit under. `finding` is `None` when it is no longer there to quote.
pub(super) fn verdict_comment(
    comment_id: &str,
    finding: Option<&ReviewComment>,
    verdict: &str,
) -> String {
    let who = finding.map_or("the reviewer", |c| c.author.as_str());
    let what = match CommentKind::of(comment_id) {
        CommentKind::Conversation => "the comment",
        _ => "the review summary",
    };
    let mut s = match finding.and_then(|c| c.url.as_deref()) {
        Some(url) => format!("On [{what}]({url}) by {who}:\n\n"),
        None => format!("On {comment_id} by {who}:\n\n"),
    };
    if let Some(c) = finding {
        for line in excerpt(&c.body).lines() {
            s.push_str(&format!("> {line}\n"));
        }
        s.push('\n');
    }
    s.push_str(verdict);
    s
}

/// The start of a finding, bounded: a quote is there to say which finding is answered, and the
/// finding itself is one link away.
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

    const COPILOT: &str = COPILOT_REVIEWER;
    const CREW: &str = "crew-bot[bot]";

    #[test]
    fn a_summary_that_is_only_findings_none_carries_no_finding() {
        assert!(!carries_findings("", true));
        assert!(!carries_findings("  \n\n", true));
        assert!(!carries_findings("**Findings:** None", true));
        assert!(!carries_findings("**Findings:** 0", true));
        assert!(!carries_findings(
            "<!-- ccr-overview-v2 -->\n\n## Copilot review overview\n\n**Findings:** None\n",
            true
        ));
        assert!(!carries_findings("<!-- a\nmultiline marker -->\nFindings:   none", true));
    }

    #[test]
    fn a_summary_with_anything_beside_findings_none_is_a_finding() {
        assert!(carries_findings(
            "## Copilot review overview\n\n### Needs a closer look\n\nCorrect the dirty-tree \
             no-rebase status.\n\n**Findings:** None",
            true
        ));
        assert!(carries_findings(
            "Previously missed: src/gate/git.rs:325 reports on_base: false",
            true
        ));
    }

    /// Counting this template as a finding spent a delivery round on every Copilot review (#201):
    /// it is Copilot's summary as posted, with the overview sentence and entries cut.
    const TEMPLATE: &str = "<!-- ccr-overview-v2 -->\n\n## Copilot review overview\n\n\
        ### 🟢 Approved\n\n**Review effort:** Balanced  \n**Findings:** None\n\n\
        <details>\n<summary><strong>Previously missed (0)</strong></summary>\n\n\
        In code that hasn’t changed since last review\n<br>\n\n</details>\n";

    #[test]
    fn a_copilot_summary_that_is_only_its_template_is_not_a_finding() {
        assert!(!carries_findings(TEMPLATE, true));
        assert!(!carries_findings("*Review effort:* Lite\n<details open>\n</details>", true));
        let reviews = [review("1", COPILOT, "COMMENTED", "head", TEMPLATE)];
        assert!(summary_findings(&reviews, "head", CREW).is_empty());
    }

    #[test]
    fn a_summary_with_an_overview_sentence_and_a_count_is_still_a_finding() {
        let body = TEMPLATE
            .replace(
                "### 🟢 Approved\n\n",
                "### 🔵 Needs a closer look\n\nThe guard can still judge an older stale head.\n\n",
            )
            .replace("**Findings:** None", "**Findings:** 1");
        let reviews = [review("1", COPILOT, "COMMENTED", "head", &body)];
        assert_eq!(summary_findings(&reviews, "head", CREW).len(), 1);
        assert!(carries_findings("a < b is <b>still</b> said", true));
    }

    /// #258's last delivery round was spent on this summary, review 5400835744, as posted (#280).
    const NEEDS_A_CLOSER_LOOK: &str = "<!-- ccr-overview-v2 -->\n\n## Copilot review overview\n\n\
        ### 🔵 Needs a closer look\n\nCredential-backed provisioning and changes to delivery’s CI \
        decisions warrant final human validation.\n\n**Review effort:** Balanced  \n\
        **Findings:** None\n";

    #[test]
    fn a_needs_a_closer_look_summary_with_no_findings_opens_no_round() {
        let reviews = [review("5400835744", COPILOT, "COMMENTED", "head", NEEDS_A_CLOSER_LOOK)];
        assert!(summary_findings(&reviews, "head", CREW).is_empty());
        for status in ["🟡 Changes recommended", "🟢 Approved", "🔴 Something new"] {
            let body = NEEDS_A_CLOSER_LOOK.replace("🔵 Needs a closer look", status);
            assert!(!carries_findings(&body, true), "{status}");
        }
        let zero = NEEDS_A_CLOSER_LOOK.replace("**Findings:** None", "**Findings:** 0");
        assert!(!carries_findings(&zero, true));
        let with_template = TEMPLATE.replace(
            "### 🟢 Approved\n\n",
            "### 🔵 Needs a closer look\n\nThe guard can still judge an older stale head.\n\n",
        );
        assert!(!carries_findings(&with_template, true));
        // From anyone but Copilot, the same body is read whole.
        assert!(carries_findings(NEEDS_A_CLOSER_LOOK, false));
    }

    #[test]
    fn a_summary_with_a_previously_missed_item_still_opens_a_round() {
        let body = NEEDS_A_CLOSER_LOOK.to_string()
            + "\n<details>\n<summary><strong>Previously missed (1)</strong></summary>\n\n\
               In code that hasn’t changed since last review\n\n<details>\n<summary>\
               <picture><img src=\"low.png\" alt=\"Low severity\"></picture> ci_status \
               collapses checks from two workflows</summary>\n\n`src/forge/github.rs:612`\n\
               </details>\n</details>\n";
        let reviews = [review("1", COPILOT, "COMMENTED", "head", &body)];
        assert_eq!(summary_findings(&reviews, "head", CREW).len(), 1);
        let second_line =
            NEEDS_A_CLOSER_LOOK.replace("validation.\n\n", "validation.\n\nRename the guard.\n\n");
        assert!(carries_findings(&second_line, true));
    }

    /// PR #229's round 3 of 3 was spent on this summary (#234).
    #[test]
    fn an_approving_copilot_summary_with_no_findings_is_not_a_finding() {
        let body = "### 🟢 Approved\nThe fail-fast paths are consistently propagated, documented, \
            and covered by focused startup tests.\n**Findings:** None";
        assert!(!carries_findings(body, true));
        let with_template = TEMPLATE.replace(
            "### 🟢 Approved\n\n",
            "### 🟢 Approved\n\nThe fail-fast paths are consistently propagated.\n\n",
        );
        let reviews = [review("1", COPILOT, "COMMENTED", "head", &with_template)];
        assert!(summary_findings(&reviews, "head", CREW).is_empty());
    }

    #[test]
    fn an_approval_is_still_a_finding_without_findings_none_or_with_another_section() {
        assert!(carries_findings("### 🟢 Approved\nLooks good.\n**Findings:** 1", true));
        assert!(carries_findings("### 🟢 Approved\nLooks good.", true));
        assert!(carries_findings(
            "### 🟢 Approved\nLooks good.\n**Findings:** None\n### Notes\nRename the guard.",
            true
        ));
        assert!(carries_findings(
            "### 🟢 Approved\nLooks good.\n**Findings:** None\nRename it.",
            true
        ));
        assert!(carries_findings(
            "### 🟢 Approved\nLooks good.\nRename it.\n**Findings:** None",
            true
        ));
        let approval = "### 🟢 Approved\nLooks good.\n**Findings:** None";
        assert!(carries_findings(approval, false));
        let reviews = [review("1", "alice", "COMMENTED", "head", approval)];
        assert_eq!(summary_findings(&reviews, "head", CREW).len(), 1);
        assert!(carries_findings(
            "### 🟢 Approved\nLooks good.\n**Findings:** None\n<details>\n\
             <summary>Open (1)</summary>\nRename the guard.\n</details>",
            true
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
        assert_eq!(summary_findings(&reviews, "head", CREW).len(), 1);
    }

    #[test]
    fn a_commented_or_changes_requested_summary_on_the_head_is_a_finding_from_anyone_but_crewd() {
        let reviews = vec![
            review("1", COPILOT, "COMMENTED", "old", "stale finding"),
            review("2", COPILOT, "COMMENTED", "head", "a finding"),
            review("3", COPILOT, "COMMENTED", "head", "**Findings:** None"),
            review("4", "alice", "COMMENTED", "head", "a thought"),
            review("5", "alice", "APPROVED", "head", "looks fine"),
            review("6", "bob", "CHANGES_REQUESTED", "head", "please split this"),
            review("7", "bob", "CHANGES_REQUESTED", "head", ""),
            review("8", CREW, "COMMENTED", "head", "crewd's own summary"),
            review("9", "carol", "DISMISSED", "head", "withdrawn"),
        ];
        let got = summary_findings(&reviews, "head", CREW);
        let ids: Vec<&str> = got.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["review-2", "review-4", "review-6"]);
        assert_eq!(got[2].author, "bob");
        assert_eq!(got[2].path, None);
    }

    #[test]
    fn conversation_comments_are_findings_keyed_apart_except_crewds_own_and_empty_ones() {
        let c = |id: &str, author: &str, body: &str| ReviewComment {
            id: id.into(),
            author: author.into(),
            path: None,
            line: None,
            body: body.into(),
            url: None,
        };
        let got = conversation_findings(
            vec![c("1", "alice", "Rename it."), c("2", CREW, "**Accepted**"), c("3", "bob", " ")],
            CREW,
        );
        let ids: Vec<&str> = got.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["conversation-1"]);
        assert_eq!(CommentKind::of(&got[0].id), CommentKind::Conversation);
    }

    #[test]
    fn a_verdict_comment_quotes_the_finding_it_answers() {
        let r = summary_comment(&review("9", COPILOT, "COMMENTED", "head", "line one\nline two"));
        let s = verdict_comment("review-9", Some(&r), "**Rejected** — noise.");
        assert!(s.contains("> line one\n> line two\n"), "{s}");
        assert!(s.ends_with("**Rejected** — noise."), "{s}");
        let gone = verdict_comment("review-9", None, "**Rejected** — noise.");
        assert!(gone.starts_with("On review-9 by the reviewer"), "{gone}");
    }
}
