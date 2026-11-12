//! `@` at the start of a message: agent or file (messaging.md A5). Only the first word counts;
//! a mention further in is always a file.

/// What the first word of a submitted message means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// Not an agent address: the text goes to the current target.
    None,
    /// `@main` / `@name` / `@agent:name` of a live instance. `rest` empty = only switch the target.
    /// `hint` is shown when a file or folder of that name exists too (the agent won).
    To {
        name: String,
        rest: String,
        hint: Option<String>,
    },
    /// `@agent:<definition> text`: a new instance.
    Start { def: String, rest: String },
    /// `@agent:x` names neither an instance nor a definition.
    Unknown(String),
    /// `@sess:...` / `@ws:...`: another session (not available yet).
    Later(String),
}

/// A name an `@` can address.
pub(crate) struct Known<'a> {
    pub names: &'a [String],
    pub defs: &'a [String],
}

fn agent_charset(n: &str) -> bool {
    !n.is_empty()
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// `exists(name)`: a file or folder of that name is in the working directory.
pub(crate) fn parse(text: &str, known: &Known, exists: impl Fn(&str) -> bool) -> Route {
    // no trim: a leading space is how pasted / burst text is marked as not typed
    let Some(body) = text.strip_prefix('@') else {
        return Route::None;
    };
    let end = body.find(char::is_whitespace).unwrap_or(body.len());
    let (word, rest) = (&body[..end], body[end..].trim().to_string());
    let has = |n: &str| n == "main" || known.names.iter().any(|k| k == n);
    if let Some(name) = word.strip_prefix("agent:") {
        return if has(name) {
            Route::To {
                name: name.into(),
                rest,
                hint: None,
            }
        } else if known.defs.iter().any(|d| d == name) {
            Route::Start {
                def: name.into(),
                rest,
            }
        } else if name.is_empty() {
            Route::None
        } else {
            Route::Unknown(name.into())
        };
    }
    if word.starts_with("sess:") || word.starts_with("ws:") {
        return Route::Later(word.into());
    }
    // a path (`.`, `/`, `\`), another namespace (`:`) or `@@scope/pkg` is a file
    if !agent_charset(word) || !has(word) {
        return Route::None;
    }
    let hint = exists(word)
        .then(|| format!("sent to @{word} (agent) · write @./{word} to attach the folder or file"));
    Route::To {
        name: word.into(),
        rest,
        hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Route {
        let names = ["worker".to_string(), "docs".to_string()];
        let defs = ["reviewer".to_string(), "worker".to_string()];
        parse(
            text,
            &Known {
                names: &names,
                defs: &defs,
            },
            |n| n == "docs",
        )
    }

    fn to(name: &str, rest: &str) -> Route {
        Route::To {
            name: name.into(),
            rest: rest.into(),
            hint: None,
        }
    }

    #[test]
    fn the_first_word_routes() {
        assert_eq!(p("@worker add tests"), to("worker", "add tests"));
        assert_eq!(p("@worker\nmulti\nline"), to("worker", "multi\nline"));
        assert_eq!(p(" @worker pasted"), Route::None, "only a typed @ routes");
        assert_eq!(p("@worker"), to("worker", ""));
        assert_eq!(p("@main hi"), to("main", "hi"));
        assert_eq!(p("@agent:worker hi"), to("worker", "hi"));
    }

    #[test]
    fn agent_colon_starts_a_definition() {
        assert_eq!(
            p("@agent:reviewer look at x"),
            Route::Start {
                def: "reviewer".into(),
                rest: "look at x".into()
            }
        );
        assert_eq!(p("@agent:nope hi"), Route::Unknown("nope".into()));
        assert_eq!(p("@agent: hi"), Route::None);
    }

    #[test]
    fn files_and_mid_text_mentions_are_never_agents() {
        for t in [
            "read @worker please",
            "@README.md summarise",
            "@src/x.rs fix",
            "@./worker fix",
            "@docs/ list",
            "@@types/node what",
            "foo@worker",
            "@./ws:x hi",
            "@Worker hi",
            "@nobody hi",
            "@worker.rs hi",
        ] {
            assert_eq!(p(t), Route::None, "{t}");
        }
    }

    #[test]
    fn windows_paths_are_files() {
        for t in [
            r"@src\x.rs fix",
            r"@worker\ x",
            r"@.\worker x",
            r"@C:\work\x.rs",
        ] {
            assert_eq!(p(t), Route::None, "{t}");
        }
    }

    #[test]
    fn the_agent_wins_a_name_clash_with_a_hint() {
        let Route::To { name, hint, .. } = p("@docs hello") else {
            panic!()
        };
        assert_eq!(name, "docs");
        assert!(hint.unwrap().contains("@./docs"));
    }

    #[test]
    fn other_sessions_are_reserved() {
        assert_eq!(p("@sess:backend hi"), Route::Later("sess:backend".into()));
        assert_eq!(
            p("@ws:a/sess:b/agent:c hi"),
            Route::Later("ws:a/sess:b/agent:c".into())
        );
    }
}
