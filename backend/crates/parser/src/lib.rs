//! `/tip` command grammar (PRD: "Command syntax", FR-2). Pure, no I/O.
//!
//! ```text
//! /tip @<github-username> <amount>[k] [sats] [on-merge] [for <note>]
//! ```
//!
//! A command starts at the first `/tip` on a line that is at the start of the line or follows
//! whitespace, and runs to the end of that line. Commands inside fenced or inline code, `>`
//! quotes and HTML comments are ignored, so quoting someone's tip never repeats it.

mod mask;

use mergetip_core::Sats;

/// Maximum `/tip` commands honoured per comment (FR-2.1).
pub const MAX_COMMANDS_PER_COMMENT: usize = 5;
/// Maximum note length, in characters.
pub const MAX_NOTE_LEN: usize = 140;
/// Largest single tip the grammar accepts: 1 BTC. Sender caps are applied later and are lower.
pub const MAX_AMOUNT_SATS: u64 = 100_000_000;
/// The syntax line shown in every error reply.
pub const SYNTAX: &str = "/tip @username <amount>[k] [sats] [on-merge] [for <note>]";

const MAX_LOGIN_LEN: usize = 39;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TipCommand {
    /// Position among the commands found in the comment (valid or not). Stable for a given
    /// body, so it is part of the tip's idempotency key.
    pub index: u16,
    /// GitHub login without the `@`, as written (GitHub logins are case-insensitive).
    pub receiver: String,
    pub amount: Sats,
    pub on_merge: bool,
    pub note: Option<String>,
}

/// A malformed command, with enough context to tell the sender which one and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub index: u16,
    /// The command as written, from `/tip` to the end of its line.
    pub text: String,
    pub error: ParseError,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("missing receiver")]
    MissingReceiver,
    #[error("invalid GitHub username {0:?}")]
    InvalidUsername(String),
    #[error("missing amount")]
    MissingAmount,
    #[error("invalid amount {0:?}")]
    InvalidAmount(String),
    #[error("amount out of range")]
    AmountOutOfRange,
    #[error("note too long")]
    NoteTooLong,
    #[error("unexpected {0:?}")]
    UnexpectedToken(String),
    #[error("too many /tip commands in one comment")]
    TooManyCommands,
}

impl ParseError {
    /// A one-sentence fix for the sender, shown in the bot reply above [`SYNTAX`].
    pub fn help(&self) -> String {
        match self {
            Self::MissingReceiver => {
                "Say who to tip, starting with @, e.g. `/tip @alice 2100`.".into()
            }
            Self::InvalidUsername(name) => format!(
                "`{name}` isn't a valid GitHub username: use letters, digits and single hyphens, \
                 up to {MAX_LOGIN_LEN} characters."
            ),
            Self::MissingAmount => {
                "Add an amount in sats after the username, e.g. `2100` or `2.1k`.".into()
            }
            Self::InvalidAmount(raw) => format!(
                "`{raw}` isn't an amount: use whole sats like `2100`, or `k` for thousands like \
                 `2.1k` (up to 3 decimals)."
            ),
            Self::AmountOutOfRange => format!(
                "Tips must be between 1 sat and {} sats.",
                group_thousands(MAX_AMOUNT_SATS)
            ),
            Self::NoteTooLong => format!("Keep the note after `for` to {MAX_NOTE_LEN} characters."),
            Self::UnexpectedToken(token) => format!(
                "`{token}` isn't understood here. One receiver per command: put each tip on its \
                 own line, and write the note after `for`."
            ),
            Self::TooManyCommands => format!(
                "Only {MAX_COMMANDS_PER_COMMENT} tips are allowed per comment; post the rest in \
                 another comment."
            ),
        }
    }
}

/// Extracts every `/tip` command from a comment body, in order.
///
/// Commands beyond [`MAX_COMMANDS_PER_COMMENT`] come back as [`ParseError::TooManyCommands`].
pub fn parse_comment(body: &str) -> Vec<Result<TipCommand, CommandError>> {
    let masked = mask::mask(body);
    let mut results = Vec::new();
    for (masked_line, line) in masked.split('\n').zip(body.split('\n')) {
        let Some(start) = find_command_start(masked_line) else {
            continue;
        };
        let index = u16::try_from(results.len()).unwrap_or(u16::MAX);
        // The mask is byte-aligned with the body, so the offset found in the masked line points
        // at the same `/tip` in the original. Parse the original, so code spans inside a note
        // are kept as written.
        let text = line[start..].trim_end().to_owned();
        let result = if results.len() >= MAX_COMMANDS_PER_COMMENT {
            Err(ParseError::TooManyCommands)
        } else {
            parse_command(&text[4..])
        };
        results.push(
            result
                .map_err(|error| CommandError {
                    index,
                    text: text.clone(),
                    error,
                })
                .map(|(receiver, amount, on_merge, note)| TipCommand {
                    index,
                    receiver,
                    amount,
                    on_merge,
                    note,
                }),
        );
    }
    results
}

/// Byte offset of the first `/tip` on the line that starts a command: at the start of the line
/// or after whitespace, and followed by whitespace or the end of the line.
fn find_command_start(line: &str) -> Option<usize> {
    let lower = line.to_ascii_lowercase();
    lower.match_indices("/tip").map(|(at, _)| at).find(|&at| {
        let before_ok = line[..at]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace);
        let after_ok = line[at + 4..]
            .chars()
            .next()
            .is_none_or(char::is_whitespace);
        before_ok && after_ok
    })
}

type Parsed = (String, Sats, bool, Option<String>);

/// Parses everything after `/tip`.
fn parse_command(rest: &str) -> Result<Parsed, ParseError> {
    let mut tokens = Tokens::new(rest);

    let receiver = match tokens.next() {
        Some(token) => token.strip_prefix('@').ok_or(ParseError::MissingReceiver)?,
        None => return Err(ParseError::MissingReceiver),
    };
    if !is_valid_login(receiver) {
        return Err(ParseError::InvalidUsername(truncate(receiver)));
    }

    let amount = parse_amount(tokens.next().ok_or(ParseError::MissingAmount)?)?;

    let mut on_merge = false;
    let mut seen_sats = false;
    let mut note = None;
    while let Some(token) = tokens.next() {
        match token.to_ascii_lowercase().as_str() {
            "sats" | "sat" if !seen_sats && !on_merge => seen_sats = true,
            "on-merge" if !on_merge => on_merge = true,
            "for" => {
                let text = tokens.remainder().trim();
                if text.is_empty() {
                    return Err(ParseError::UnexpectedToken("for".into()));
                }
                if text.chars().count() > MAX_NOTE_LEN {
                    return Err(ParseError::NoteTooLong);
                }
                note = Some(text.to_owned());
                break;
            }
            _ => return Err(ParseError::UnexpectedToken(truncate(token))),
        }
    }
    Ok((receiver.to_owned(), amount, on_merge, note))
}

/// GitHub's login rules: alphanumerics and single hyphens, not at either end, 1–39 characters.
fn is_valid_login(login: &str) -> bool {
    !login.is_empty()
        && login.len() <= MAX_LOGIN_LEN
        && login.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !login.starts_with('-')
        && !login.ends_with('-')
        && !login.contains("--")
}

/// `2100` or `2.1k` (×1,000, up to three decimals, the result a whole number of sats).
fn parse_amount(raw: &str) -> Result<Sats, ParseError> {
    let invalid = || ParseError::InvalidAmount(truncate(raw));
    let lower = raw.to_ascii_lowercase();
    let (number, multiplier) = match lower.strip_suffix('k') {
        Some(number) => (number, 1_000_u64),
        None => (lower.as_str(), 1),
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let digits_only = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty() || !digits_only(whole) || !digits_only(fraction) {
        return Err(invalid());
    }
    if number.contains('.') && (multiplier == 1 || fraction.is_empty() || fraction.len() > 3) {
        return Err(invalid());
    }

    let whole: u64 = whole.parse().map_err(|_| ParseError::AmountOutOfRange)?;
    let fraction_sats = if fraction.is_empty() {
        0
    } else {
        let scale = 10_u64.pow(u32::try_from(fraction.len()).unwrap_or(3));
        let value: u64 = fraction.parse().map_err(|_| invalid())?;
        value * (multiplier / scale)
    };
    let sats = whole
        .checked_mul(multiplier)
        .and_then(|s| s.checked_add(fraction_sats))
        .ok_or(ParseError::AmountOutOfRange)?;
    if sats == 0 || sats > MAX_AMOUNT_SATS {
        return Err(ParseError::AmountOutOfRange);
    }
    Ok(Sats::new(sats))
}

/// Prepares user input for quoting in an error message: short, and with backticks replaced so it
/// cannot break out of the code span it is shown in (which also keeps `@mentions` from pinging).
fn truncate(token: &str) -> String {
    const MAX: usize = 40;
    let mut cut: String = token
        .chars()
        .take(MAX)
        .map(|c| if c == '`' { '\'' } else { c })
        .collect();
    if token.chars().count() > MAX {
        cut.push('…');
    }
    cut
}

fn group_thousands(value: u64) -> String {
    let sats = Sats::new(value).to_string();
    sats.trim_end_matches(" sats").to_owned()
}

/// Whitespace tokenizer that can also hand back the untouched rest of the line (for notes).
struct Tokens<'a> {
    rest: &'a str,
}

impl<'a> Tokens<'a> {
    fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    fn remainder(&self) -> &'a str {
        self.rest
    }
}

impl<'a> Iterator for Tokens<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let trimmed = self.rest.trim_start();
        if trimmed.is_empty() {
            self.rest = trimmed;
            return None;
        }
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        let (token, rest) = trimmed.split_at(end);
        self.rest = rest;
        Some(token)
    }
}
