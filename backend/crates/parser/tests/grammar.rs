use mergetip_core::Sats;
use mergetip_parser::{
    CommandError, MAX_COMMANDS_PER_COMMENT, ParseError, SYNTAX, TipCommand, parse_comment,
};

fn one(body: &str) -> Result<TipCommand, ParseError> {
    let mut results = parse_comment(body);
    assert_eq!(
        results.len(),
        1,
        "expected one command in {body:?}: {results:?}"
    );
    results.remove(0).map_err(|e| e.error)
}

fn tip(receiver: &str, sats: u64, on_merge: bool, note: Option<&str>) -> TipCommand {
    TipCommand {
        index: 0,
        receiver: receiver.into(),
        amount: Sats::new(sats),
        on_merge,
        note: note.map(Into::into),
    }
}

#[test]
fn prd_examples() {
    assert_eq!(
        one("/tip @alice 2100"),
        Ok(tip("alice", 2_100, false, None))
    );
    assert_eq!(
        one("/tip @alice 2.1k for the review"),
        Ok(tip("alice", 2_100, false, Some("the review")))
    );
    assert_eq!(
        one("/tip @bob 5000 on-merge"),
        Ok(tip("bob", 5_000, true, None))
    );
    assert_eq!(
        one("/tip @alice 500 @bob 500"),
        Err(ParseError::UnexpectedToken("@bob".into()))
    );
}

#[test]
fn optional_parts_in_grammar_order() {
    assert_eq!(
        one("/tip @alice 21 sats on-merge for docs and triage"),
        Ok(tip("alice", 21, true, Some("docs and triage")))
    );
    assert_eq!(one("/tip @alice 1 sat"), Ok(tip("alice", 1, false, None)));
    assert_eq!(
        one("/TIP @Alice 2K SATS ON-MERGE FOR Thanks!"),
        Ok(tip("Alice", 2_000, true, Some("Thanks!")))
    );
}

#[test]
fn amounts() {
    for (raw, sats) in [
        ("1", 1),
        ("2.1k", 2_100),
        ("2.125k", 2_125),
        ("5k", 5_000),
        ("0.5k", 500),
    ] {
        assert_eq!(
            one(&format!("/tip @a {raw}")).map(|t| t.amount),
            Ok(Sats::new(sats)),
            "{raw}"
        );
    }
    for raw in [
        "2.0001k", "2,100", "1.5", "k", ".5k", "2.k", "-5", "+5", "1e3", "abc",
    ] {
        assert_eq!(
            one(&format!("/tip @a {raw}")),
            Err(ParseError::InvalidAmount(raw.into())),
            "{raw}"
        );
    }
    for raw in ["0", "0k", "100000001", "99999999999999999999", "100001k"] {
        assert_eq!(
            one(&format!("/tip @a {raw}")),
            Err(ParseError::AmountOutOfRange),
            "{raw}"
        );
    }
    assert_eq!(
        one("/tip @a 100000k").map(|t| t.amount),
        Ok(Sats::new(100_000_000))
    );
}

#[test]
fn usernames() {
    let forty = "a".repeat(40);
    let thirty_nine = "a".repeat(39);
    assert!(one(&format!("/tip @{thirty_nine} 1")).is_ok());
    for bad in ["a-", "-a", "a--b", "a_b", "a.b", "", forty.as_str()] {
        assert_eq!(
            one(&format!("/tip @{bad} 1")),
            Err(ParseError::InvalidUsername(bad.into())),
            "{bad:?}"
        );
    }
    assert_eq!(one("/tip alice 1"), Err(ParseError::MissingReceiver));
    assert_eq!(one("/tip"), Err(ParseError::MissingReceiver));
    assert_eq!(one("/tip @alice"), Err(ParseError::MissingAmount));
}

#[test]
fn modifiers_out_of_order_or_repeated_are_rejected() {
    assert_eq!(
        one("/tip @a 1 on-merge sats"),
        Err(ParseError::UnexpectedToken("sats".into()))
    );
    assert_eq!(
        one("/tip @a 1 on-merge on-merge"),
        Err(ParseError::UnexpectedToken("on-merge".into()))
    );
    assert_eq!(
        one("/tip @a 1 for"),
        Err(ParseError::UnexpectedToken("for".into()))
    );
    assert_eq!(
        one("/tip @a 1 thanks"),
        Err(ParseError::UnexpectedToken("thanks".into()))
    );
}

#[test]
fn notes_keep_their_text_and_respect_the_limit() {
    let note_140 = "é".repeat(140);
    assert_eq!(
        one(&format!("/tip @a 1 for {note_140}")).map(|t| t.note),
        Ok(Some(note_140.clone()))
    );
    assert_eq!(
        one(&format!("/tip @a 1 for {note_140}x")),
        Err(ParseError::NoteTooLong)
    );
    assert_eq!(
        one("/tip @a 1 for fixing `parse()` ✨  ").map(|t| t.note),
        Ok(Some("fixing `parse()` ✨".into()))
    );
    assert_eq!(
        one("/tip @a 1 for /tip @b 2 is in the note").map(|t| t.note),
        Ok(Some("/tip @b 2 is in the note".into()))
    );
}

#[test]
fn commands_anywhere_one_per_line() {
    let results = parse_comment(
        "Great work!\r\nThanks /tip @alice 100\r\n\r\n  /tip @bob 2k on-merge\nnot/tip @x 1\n/tipping @y 1",
    );
    let ok: Vec<_> = results.into_iter().map(Result::unwrap).collect();
    assert_eq!(ok.len(), 2);
    assert_eq!((ok[0].index, ok[0].receiver.as_str()), (0, "alice"));
    assert_eq!(
        (ok[1].index, ok[1].receiver.as_str(), ok[1].on_merge),
        (1, "bob", true)
    );
}

#[test]
fn ignored_contexts() {
    let body = "\
> /tip @quoted 1
```
/tip @fenced 1
```
~~~sh
/tip @tilde 1
~~~
Use `/tip @inline 1` like this
<!-- /tip @hidden 1 -->
/tip @real 7";
    let results = parse_comment(body);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].as_ref().unwrap().receiver, "real");
}

#[test]
fn errors_carry_index_and_text() {
    let results = parse_comment("/tip @a 1\n/tip @b nope  \n/tip @c 3");
    assert!(results[0].is_ok());
    assert_eq!(
        results[1],
        Err(CommandError {
            index: 1,
            text: "/tip @b nope".into(),
            error: ParseError::InvalidAmount("nope".into()),
        })
    );
    assert_eq!(results[2].as_ref().unwrap().index, 2);
}

#[test]
fn sixth_and_later_commands_are_refused() {
    let body: String = (0..7)
        .map(|i| format!("/tip @user{i} {}\n", i + 1))
        .collect();
    let results = parse_comment(&body);
    assert_eq!(results.len(), 7);
    assert!(
        results[..MAX_COMMANDS_PER_COMMENT]
            .iter()
            .all(Result::is_ok)
    );
    for result in &results[MAX_COMMANDS_PER_COMMENT..] {
        assert_eq!(
            result.as_ref().unwrap_err().error,
            ParseError::TooManyCommands
        );
    }
}

#[test]
fn quoted_input_cannot_break_out_of_code_spans() {
    assert_eq!(
        one("/tip @a 1 ``@everyone"),
        Err(ParseError::UnexpectedToken("''@everyone".into()))
    );
    let long = "x".repeat(60);
    assert_eq!(
        one(&format!("/tip @a {long}")),
        Err(ParseError::InvalidAmount(format!("{}…", "x".repeat(40))))
    );
}

#[test]
fn no_commands_in_plain_text() {
    assert!(parse_comment("LGTM, thanks for the review!").is_empty());
    assert!(parse_comment("").is_empty());
}

#[test]
fn help_messages() {
    let errors = [
        ParseError::MissingReceiver,
        ParseError::InvalidUsername("a--b".into()),
        ParseError::MissingAmount,
        ParseError::InvalidAmount("2,100".into()),
        ParseError::AmountOutOfRange,
        ParseError::NoteTooLong,
        ParseError::UnexpectedToken("@bob".into()),
        ParseError::TooManyCommands,
    ];
    let rendered: Vec<String> = errors.iter().map(ParseError::help).collect();
    insta::assert_snapshot!(rendered.join("\n"));
    insta::assert_snapshot!("syntax", SYNTAX);
}
