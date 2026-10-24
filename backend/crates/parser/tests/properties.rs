use mergetip_parser::{MAX_AMOUNT_SATS, MAX_COMMANDS_PER_COMMENT, parse_comment};
use proptest::prelude::*;

/// Bodies built from fragments that exercise the tricky paths, mixed with arbitrary text.
fn comment() -> impl Strategy<Value = String> {
    let fragment = prop_oneof![
        Just("/tip ".to_owned()),
        Just("@".to_owned()),
        Just("```".to_owned()),
        Just("~~~".to_owned()),
        Just("`".to_owned()),
        Just("> ".to_owned()),
        Just("<!--".to_owned()),
        Just("-->".to_owned()),
        Just(" for ".to_owned()),
        Just(" on-merge".to_owned()),
        Just("k".to_owned()),
        Just(".".to_owned()),
        Just("\n".to_owned()),
        Just("\r\n".to_owned()),
        "[0-9]{1,12}",
        "[a-zA-Z-]{1,41}",
        any::<String>(),
    ];
    prop::collection::vec(fragment, 0..40).prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2_000))]

    #[test]
    fn never_panics_on_arbitrary_utf8(body in any::<String>()) {
        let _ = parse_comment(&body);
    }

    #[test]
    fn parsing_is_deterministic(body in comment()) {
        prop_assert_eq!(parse_comment(&body), parse_comment(&body));
    }

    #[test]
    fn accepted_commands_respect_the_grammar(body in comment()) {
        let results = parse_comment(&body);
        prop_assert!(results.len() <= body.split('\n').count());
        let accepted: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        prop_assert!(accepted.len() <= MAX_COMMANDS_PER_COMMENT);
        for (i, result) in results.iter().enumerate() {
            let index = match result {
                Ok(tip) => tip.index,
                Err(err) => err.index,
            };
            prop_assert_eq!(usize::from(index), i);
        }
        for tip in accepted {
            prop_assert!((1..=MAX_AMOUNT_SATS).contains(&tip.amount.get()));
            prop_assert!(!tip.receiver.is_empty() && tip.receiver.len() <= 39);
            prop_assert!(tip.receiver.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
            if let Some(note) = &tip.note {
                prop_assert!(!note.is_empty() && note.chars().count() <= 140);
            }
        }
    }
}
