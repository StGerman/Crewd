//! Secret redaction for the lines crewd stores: transcript lines and its own log lines (#138).
//!
//! A transcript copies every worker stream line through before the parser decides whether it
//! has a use for it, so a token the agent read and printed would otherwise sit on disk for as
//! long as retention keeps the run. Nothing on the host confines that: a worker that cannot reach
//! `github.com` can still print what it read. The line is kept, because it is the post-mortem;
//! only the secret in it is replaced, by [`MARKER`].
//!
//! Matching is by shape, never by the values crewd knows: the agent's environment and tool
//! results hold credentials crewd never saw. A shape that is not listed here is not redacted,
//! so this narrows what a transcript keeps rather than guaranteeing it keeps nothing.
//!
//! A line with no match is returned borrowed, byte for byte, so a clean transcript is unchanged
//! and costs no allocation.

use std::borrow::Cow;
use std::io::Write;
use std::sync::LazyLock;

use regex::Regex;

/// What a secret is replaced with. Bracketed, so a reader of the transcript sees that a value
/// was there rather than a line that looks malformed.
pub const MARKER: &str = "[REDACTED]";

/// An ANSI style sequence. Under a terminal, `tracing`'s formatter wraps a field's name and its
/// `=` in these, and a `key=value` pattern that did not skip them would miss every coloured log
/// line.
const ANSI: &str = r"(?:\x1b\[[0-9;]*m)*";

/// Each pattern with its replacement. A replacement that keeps a capture group keeps the name
/// of the thing redacted, which is what the reader needs to know which credential leaked.
static PATTERNS: LazyLock<Vec<(Regex, String)>> = LazyLock::new(|| {
    let kv_name = r"(?i)\b([A-Za-z0-9_.-]*(?:password|passwd|secret|token|api[_-]?key|apikey|access[_-]?key|secret[_-]?key|private[_-]?key|credentials?))";
    let sep = format!(r#"({ANSI}\\?["']?{ANSI}\s*[:=]{ANSI}\s*\\?["']?)"#);
    let quoted_sep = format!(r#"{ANSI}\\?["']?{ANSI}\s*[:=]{ANSI}\s*"#);
    let quoted = format!("${{1}}${{2}}${{3}}{MARKER}${{4}}");
    let url_params = r"(?i)([?&](?:access_token|refresh_token|id_token|token|api_key|apikey|key|password|passwd|pwd|secret|client_secret|state|code|sig|signature|auth|x-amz-signature|x-amz-credential|x-amz-security-token)=)[^&\s#\x22'\\]+";
    let table: Vec<(String, String)> = vec![
        // Multi-line in a log, `\n`-escaped inside a JSON stream line; an unterminated block
        // runs to the end of the line rather than leaving its tail in place, and
        // [`LineRedactor`] carries it into the lines after.
        (
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----(?s:.*?)(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|\z)"
                .into(),
            MARKER.into(),
        ),
        (r"(://[^/\s:@\x22'\\]+:)[^@\s/\x22'\\]+@".into(), format!("${{1}}{MARKER}@")),
        (url_params.into(), format!("${{1}}{MARKER}")),
        (r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{22,})".into(), MARKER.into()),
        (r"\bglpat-[A-Za-z0-9_-]{20,}".into(), MARKER.into()),
        (r"\bxox[abposr]-[A-Za-z0-9-]{10,}".into(), MARKER.into()),
        (r"\bxapp-[0-9]-[A-Za-z0-9-]{10,}".into(), MARKER.into()),
        (r"\bsk-[A-Za-z0-9_-]{20,}".into(), MARKER.into()),
        (r"\bAIza[0-9A-Za-z_-]{35}".into(), MARKER.into()),
        (r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b".into(), MARKER.into()),
        (r"\beyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]+".into(), MARKER.into()),
        // Any spelling of the header: raw, `=`, JSON-quoted or coloured by `tracing`. The scheme
        // is kept and only the credential after it replaced.
        (
            format!(
                r#"(?i)(\b[A-Za-z0-9_-]*authorization{sep}(?:basic|bearer|token|digest)?{ANSI}\s*)[A-Za-z0-9._~+/-]{{6,}}=*"#
            ),
            format!("${{1}}{MARKER}"),
        ),
        (r"(?i)\b(bearer)(\s+)[A-Za-z0-9._~+/-]{8,}=*".into(), format!("${{1}}${{2}}{MARKER}")),
        // A quoted value runs to its closing quote, JSON-escaped or not: stopping at whitespace
        // would leave `horse battery staple` of a quoted passphrase in place.
        (format!(r#"{kv_name}({quoted_sep})(\\")(?:[^"\\]|\\[^"])+(\\")"#), quoted.clone()),
        (format!(r#"{kv_name}({quoted_sep})(")(?:[^"\\]|\\.)+(")"#), quoted.clone()),
        (format!(r#"{kv_name}({quoted_sep})(')[^'\n]+(')"#), quoted),
        // The name must end in the keyword, so `input_tokens` and `token_count` are left alone:
        // every usage event in a stream carries the first. A value opening with `[` is skipped,
        // so a value already redacted above is not read again with what follows it.
        (format!(r#"{kv_name}{sep}[^\s"'\\&,;\x1b\[][^\s"'\\&,;\x1b]*"#), format!("${{1}}${{2}}{MARKER}")),
    ];
    table
        .into_iter()
        .map(|(re, with)| {
            (Regex::new(&re).expect("every pattern is a literal the tests compile"), with)
        })
        .collect()
});

/// `line` with every known secret shape replaced by [`MARKER`]; borrowed when nothing matched.
pub fn redact(line: &str) -> Cow<'_, str> {
    let mut out = Cow::Borrowed(line);
    for (re, with) in PATTERNS.iter() {
        if let Cow::Owned(replaced) = re.replace_all(&out, with.as_str()) {
            out = Cow::Owned(replaced);
        }
    }
    out
}

static PEM_BEGIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----").expect("a literal the tests compile")
});
static PEM_END: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"-----END [A-Z0-9 ]*PRIVATE KEY-----").expect("a literal the tests compile")
});

/// [`redact`] across a sequence of lines, for a writer handed one line at a time.
///
/// A raw PEM block written line by line (a worker's stderr, a line the parser could not read)
/// reaches [`redact`] as a `BEGIN` line with no `END`, then a base64 body that matches no shape
/// on its own. Without the state carried here, every line after the first is stored whole.
#[derive(Default)]
pub struct LineRedactor {
    in_pem: bool,
}

impl LineRedactor {
    pub fn redact<'a>(&mut self, line: &'a str) -> Cow<'a, str> {
        if self.in_pem {
            let Some(end) = PEM_END.find(line) else { return Cow::Borrowed(MARKER) };
            self.in_pem = false;
            return Cow::Owned(format!("{MARKER}{}", redact(&line[end.end()..])));
        }
        // Only a header that ends its physical line opens a block across lines: one inside a
        // JSON line with its newlines escaped has already been redacted to that line's end, and
        // carrying it on would blank every event after it.
        if let Some(begin) = PEM_BEGIN.find_iter(line).last() {
            self.in_pem = line[begin.end()..].trim().is_empty();
        }
        redact(line)
    }
}

/// A `tracing_subscriber` writer that redacts each formatted event before it reaches `W`.
///
/// The formatter hands its writer one whole event per `write_all`, so a secret is never split
/// across two calls to [`Write::write`] here. A short write from the formatter would break
/// that, which is why `write` reports the caller's length only after the whole redacted buffer
/// went through.
pub struct Redacting<W>(pub W);

impl<W: Write> Write for Redacting<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match std::str::from_utf8(buf) {
            Ok(text) => self.0.write_all(redact(text).as_bytes())?,
            Err(_) => self.0.write_all(redact(&String::from_utf8_lossy(buf)).as_bytes())?,
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// [`Redacting`] over every writer `M` makes: what `main` installs as the log's writer.
#[derive(Clone)]
pub struct RedactingMakeWriter<M>(pub M);

impl<'a, M: tracing_subscriber::fmt::MakeWriter<'a>> tracing_subscriber::fmt::MakeWriter<'a>
    for RedactingMakeWriter<M>
{
    type Writer = Redacting<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        Redacting(self.0.make_writer())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_with_no_secret_is_stored_unchanged() {
        for line in [
            r#"{"type":"assistant","message":{"usage":{"input_tokens":23169,"cache_read_input_tokens":0,"output_tokens":263}}}"#,
            r#"{"type":"result","subtype":"success","num_turns":4,"session_id":"0b6c-11"}"#,
            "2026-10-03T10:00:00Z  INFO crew::sched: dispatched issue=MT-649 state=Todo run=r-1",
            "see https://github.com/StGerman/Crewd/issues/138?page=2 and token_count=12",
            "the token budget is spent; max_tokens: 4096",
            "",
        ] {
            assert!(matches!(redact(line), Cow::Borrowed(_)), "{line} was rewritten");
        }
    }

    #[test]
    fn every_listed_shape_is_replaced_and_the_rest_of_the_line_kept() {
        let gh = format!("ghp_{}", "a1".repeat(18));
        let cases = [
            format!("before {gh} after"),
            format!("before github_pat_{} after", "A1_".repeat(10)),
            format!("before glpat-{} after", "x".repeat(20)),
            format!("before xoxb-{} after", "1234-".repeat(4)),
            format!("before sk-ant-api03-{} after", "Q".repeat(40)),
            "before AKIAIOSFODNN7EXAMPLE after".to_string(),
            format!("before eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.{} after", "s".repeat(20)),
            format!("before Authorization: Bearer {} after", "t".repeat(30)),
            "before aws_secret_access_key=wJalrXUtnFEMI/K7MDENG after".to_string(),
            r#"before {\"password\": \"hunter2hunter2\"} after"#.to_string(),
            "before GITHUB_TOKEN=abc123def after".to_string(),
            "before https://deploy:s3cr3t@example.com/repo.git after".to_string(),
            r#"before password=\"correct horse, battery; staple!\" after"#.to_string(),
            r#"before "client_secret": "a b\"c d" after"#.to_string(),
            "before api_key='open sesame & co' after".to_string(),
        ];
        for line in cases {
            let out = redact(&line);
            assert!(out.contains(MARKER), "nothing redacted in {line}: {out}");
            assert!(out.starts_with("before") && out.ends_with("after"), "{line} became {out}");
        }
        let line = format!("{{\"text\":\"use {gh}\"}}");
        let out = redact(&line);
        assert!(!out.contains(&gh), "{out}");
    }

    #[test]
    fn a_quoted_secret_with_spaces_is_replaced_through_its_closing_quote() {
        assert_eq!(
            redact(r#"{"content":"password=\"correct horse battery staple\" next"}"#),
            format!(r#"{{"content":"password=\"{MARKER}\" next"}}"#)
        );
        assert_eq!(
            redact(r#"{"secret": "in the, tool result", "n": 1}"#),
            format!(r#"{{"secret": "{MARKER}", "n": 1}}"#)
        );
        assert_eq!(redact("token='a b c' rest"), format!("token='{MARKER}' rest"));
    }

    #[test]
    fn a_recorded_grok_stream_passes_through_unchanged() {
        for name in ["stream.jsonl", "resume.jsonl", "sigterm.jsonl"] {
            let path = format!("{}/tests/fixtures/grok/{name}", env!("CARGO_MANIFEST_DIR"));
            for line in std::fs::read_to_string(path).unwrap().lines() {
                assert_eq!(redact(line), line, "{name}");
            }
        }
    }

    #[test]
    fn a_pem_block_is_replaced_whole_whether_escaped_or_multi_line() {
        let escaped = r#"{"content":"-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nAAAA\n-----END RSA PRIVATE KEY-----\n","x":1}"#;
        assert_eq!(redact(escaped), format!(r#"{{"content":"{MARKER}\n","x":1}}"#));
        let raw = "key:\n-----BEGIN PRIVATE KEY-----\nMIIEow\n-----END PRIVATE KEY-----\ndone";
        assert_eq!(redact(raw), format!("key:\n{MARKER}\ndone"));
        let cut = "-----BEGIN PRIVATE KEY-----\nMIIEow";
        assert_eq!(redact(cut), MARKER);
    }

    #[test]
    fn a_pem_block_handed_over_one_line_at_a_time_is_replaced_to_its_end_line() {
        let mut r = LineRedactor::default();
        let lines = [
            "key follows",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQ",
            "AAAAMwAAAAtzc2gtZWQyNTUxOQAAACD",
            "-----END OPENSSH PRIVATE KEY----- tail",
            "after",
        ];
        let out: Vec<String> = lines.iter().map(|l| r.redact(l).into_owned()).collect();
        assert_eq!(
            out,
            ["key follows", MARKER, MARKER, MARKER, &format!("{MARKER} tail"), "after"]
        );
    }

    #[test]
    fn an_unterminated_pem_inside_one_json_line_does_not_blank_the_lines_after_it() {
        let mut r = LineRedactor::default();
        let cut = r#"{"content":"-----BEGIN PRIVATE KEY-----\nMIIEow\nAAAA","x":1}"#;
        assert_eq!(r.redact(cut), format!(r#"{{"content":"{MARKER}"#));
        let next = r#"{"type":"assistant","message":{"content":"done"}}"#;
        assert_eq!(r.redact(next), next);
    }

    #[test]
    fn an_authorization_credential_is_replaced_in_every_spelling_and_the_scheme_kept() {
        let cred = "dXNlcjp0b2tlbg==";
        for (line, want) in [
            (format!("Authorization: Basic {cred}"), format!("Authorization: Basic {MARKER}")),
            (
                format!("Authorization=Basic {cred} next"),
                format!("Authorization=Basic {MARKER} next"),
            ),
            (
                format!(r#"{{"Authorization": "Basic {cred}"}}"#),
                format!(r#"{{"Authorization": "Basic {MARKER}"}}"#),
            ),
            (
                format!(r#"{{\"authorization\":\"token {cred}\"}}"#),
                format!(r#"{{\"authorization\":\"token {MARKER}\"}}"#),
            ),
            (format!("proxy-authorization: {cred}"), format!("proxy-authorization: {MARKER}")),
            (
                format!("\x1b[3mauthorization\x1b[0m\x1b[2m=\x1b[0mBasic {cred} x"),
                format!("\x1b[3mauthorization\x1b[0m\x1b[2m=\x1b[0mBasic {MARKER} x"),
            ),
        ] {
            assert_eq!(redact(&line), want);
        }
    }

    #[test]
    fn every_listed_url_parameter_loses_its_value_and_keeps_the_rest_of_the_url() {
        let url = "https://h/cb?code=abc&state=xyz&page=2&access_token=t0k&api_key=k#frag";
        assert_eq!(
            redact(url),
            format!(
                "https://h/cb?code={MARKER}&state={MARKER}&page=2&access_token={MARKER}&api_key={MARKER}#frag"
            )
        );
    }

    #[test]
    fn a_coloured_log_field_is_redacted_through_its_escape_codes() {
        let line = "\x1b[3mauth_token\x1b[0m\x1b[2m=\x1b[0mabc123 next";
        assert_eq!(redact(line), format!("\x1b[3mauth_token\x1b[0m\x1b[2m=\x1b[0m{MARKER} next"));
    }

    #[test]
    fn a_url_with_an_access_token_is_logged_without_the_token() {
        #[derive(Clone, Default)]
        struct Capture(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);
        impl Write for Capture {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let capture = Capture::default();
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(RedactingMakeWriter(move || writer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(
                url = "https://api.example.com/v1/items?access_token=gho_s3cr3tvalue&page=2",
                "tracker call failed"
            );
        });

        let log = String::from_utf8(capture.0.lock().clone()).unwrap();
        assert!(log.contains("tracker call failed"), "the line is kept: {log}");
        assert!(log.contains(&format!("access_token={MARKER}&page=2")), "{log}");
        assert!(!log.contains("gho_s3cr3tvalue"), "the token reached the log: {log}");
    }
}
