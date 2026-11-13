//! Pickers for slash commands typed without an argument (`/effort`, `/panels`, `/permissions`, ...): the
//! options of a command are listed with the current one marked instead of being typed after it. Pure data;
//! `Live` shows it as a [`crate::live::PickerKind::Menu`] dialog and runs the chosen row.
//!
//! A row key is one of: `/cmd args` (run as if typed), `menu:cmd level` (open the next picker of a chain),
//! `type:text` (put `text` in the input box: a free-text argument), `info` (inert).

use crate::tui_view::layout::{HIDEABLE, Hidden};

pub const TYPE: &str = "type:";
pub const MENU: &str = "menu:";

/// Rows above this many get a type-to-filter header.
const FILTER_FROM: usize = 7;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Menu {
    pub title: String,
    /// (key, label), the current row marked in its label.
    pub rows: Vec<(String, String)>,
    pub filter: String,
    /// Key of the row the highlight starts on.
    pub current: Option<String>,
}

fn mark(label: &str, current: bool) -> String {
    if current {
        format!("* {label}  (current)")
    } else {
        format!("  {label}")
    }
}

impl Menu {
    fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            ..Self::default()
        }
    }

    /// `current`: whether this row is the value in force (it also gets the initial highlight).
    fn row(mut self, key: impl Into<String>, label: &str, current: bool) -> Self {
        let key = key.into();
        if current {
            self.current = Some(key.clone());
        }
        self.rows.push((key, mark(label, current)));
        self
    }

    /// The options the dialog shows for the filter, and the index to highlight.
    pub fn options(&self) -> (Vec<(String, String)>, usize) {
        let n = self.filter.to_lowercase();
        let mut o: Vec<_> = self
            .rows
            .iter()
            .filter(|(_, l)| l.to_lowercase().contains(&n))
            .cloned()
            .collect();
        if o.is_empty() {
            o.push(("info".into(), "no option matches the filter".into()));
        }
        let at = self
            .current
            .as_ref()
            .and_then(|c| o.iter().position(|(k, _)| k == c))
            .unwrap_or(0);
        (o, at)
    }

    /// The filter header (`None`: a short list needs none).
    pub fn header(&self) -> Option<String> {
        (self.rows.len() >= FILTER_FROM || !self.filter.is_empty()).then(|| {
            format!(
                "filter: {}_   (type to filter \u{b7} * marks the current one)",
                self.filter
            )
        })
    }
}

/// `/effort`: auto, then the model's tiers.
pub fn effort(tiers: &[String], cur: Option<&str>) -> Menu {
    let mut m = Menu::new("Effort").row("/effort auto", "auto (the model decides)", cur.is_none());
    for t in tiers {
        m = m.row(format!("/effort {t}"), t, cur == Some(t.as_str()));
    }
    m
}

/// `/verbose` and `/focus`: on or off.
pub fn on_off(cmd: &str, what: &str, on: bool) -> Menu {
    Menu::new(format!("{what}: {}", if on { "on" } else { "off" }))
        .row(format!("/{cmd} on"), "on", on)
        .row(format!("/{cmd} off"), "off", !on)
}

/// `/images` with images attached.
pub fn images(n: usize) -> Menu {
    Menu::new(format!("{n} attached image(s)"))
        .row("/images list", "show what is attached", false)
        .row("/images clear", "drop them all", false)
}

pub fn agents() -> Menu {
    Menu::new("Agents")
        .row("/agents list", "this session's sub-agents", false)
        .row("/agents --all", "and the other sessions too", false)
}

/// `/context`: common windows; the last row asks for a number.
pub fn context(title: String) -> Menu {
    let mut m = Menu::new(title);
    for w in ["128k", "200k", "400k", "1m"] {
        m = m.row(format!("/context {w}"), &format!("{w} tokens"), false);
    }
    m.row(
        "/context unset",
        "unset (use what the provider reports)",
        false,
    )
    .row(format!("{TYPE}/context "), "another size...", false)
}

pub fn permissions() -> Menu {
    Menu::new("Permissions (this session)")
        .row("/permissions list", "show the rules in force", false)
        .row(
            format!("{TYPE}/permissions allow "),
            "allow a rule...",
            false,
        )
        .row(
            format!("{MENU}permissions remove"),
            "remove a rule...",
            false,
        )
}

/// The rules of the session to remove (`rules`: the raw text, shown sanitized by the caller).
pub fn permissions_remove(rules: &[(String, String)]) -> Menu {
    let mut m = Menu::new("Remove a rule");
    for (raw, shown) in rules {
        m = m.row(format!("/permissions remove {raw}"), shown, false);
    }
    m
}

pub fn trust(has_records: bool) -> Menu {
    let m = Menu::new("Trust")
        .row("/trust list", "list the remembered folders", false)
        .row("/trust ask", "trust this folder...", false);
    if has_records {
        m.row(
            format!("{MENU}trust revoke"),
            "revoke a remembered folder...",
            false,
        )
    } else {
        m
    }
}

/// The remembered roots (raw, shown) to revoke.
pub fn trust_revoke(roots: &[(String, String)]) -> Menu {
    let mut m = Menu::new("Revoke trust for");
    for (raw, shown) in roots {
        m = m.row(format!("/trust revoke {raw}"), shown, false);
    }
    m
}

pub fn memory() -> Menu {
    Menu::new("Memory")
        .row("/memory status", "what is loaded now", false)
        .row("/memory list", "list the records", false)
        .row("/memory review", "review the pending ones", false)
        .row(format!("{TYPE}/memory search "), "search...", false)
        .row(format!("{TYPE}/memory add "), "add a record...", false)
        .row(format!("{TYPE}/memory show "), "show one (by id)...", false)
        .row(format!("{TYPE}/memory edit "), "edit one (by id)...", false)
        .row(
            format!("{TYPE}/memory forget "),
            "forget one (by id)...",
            false,
        )
}

/// `/panels`: every panel with its state (a pick flips it), the bars, focus mode and hide/show all.
pub fn panels(h: &Hidden) -> Menu {
    let mut m = Menu::new("Panels (a pick flips it and keeps this list open; Esc closes)");
    let flip =
        |key: &str, shown: bool| format!("/panels {key} {}", if shown { "off" } else { "on" });
    for (k, name) in HIDEABLE {
        let shown = !h.is_hidden(k);
        m = m.row(flip(k, shown), &format!("{} {name}", on(shown)), false);
    }
    for (k, name, shown) in [
        ("header", "Header", !h.header),
        ("footer", "Footer", !h.footer),
    ] {
        m = m.row(flip(k, shown), &format!("{} {name}", on(shown)), false);
    }
    m.row(
        flip("focus", h.focus),
        &format!("{} Focus mode (only the chat)", on(h.focus)),
        false,
    )
    .row("/panels hide-all", "hide all panels", false)
    .row("/panels show-all", "show all panels", false)
}

fn on(b: bool) -> &'static str {
    if b { "on " } else { "off" }
}

/// The next picker of a chain needs its command to say which; also what `Live` asks per level.
pub fn split(key: &str) -> Option<(&str, &str)> {
    key.strip_prefix(MENU)?.split_once(' ')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(m: &Menu) -> Vec<String> {
        m.rows.iter().map(|r| r.0.clone()).collect()
    }

    #[test]
    fn effort_lists_auto_and_tiers_and_marks_the_current_one() {
        let t = ["low".to_string(), "high".to_string()];
        let m = effort(&t, Some("high"));
        assert_eq!(keys(&m), ["/effort auto", "/effort low", "/effort high"]);
        let (o, at) = m.options();
        assert_eq!(at, 2);
        assert!(o[2].1.contains("(current)") && !o[0].1.contains("(current)"));
        // no stored value is "auto"
        assert_eq!(effort(&t, None).options().1, 0);
    }

    #[test]
    fn on_off_marks_the_state_in_force() {
        assert_eq!(on_off("verbose", "Verbose", false).options().1, 1);
        assert_eq!(on_off("focus", "Focus", true).options().1, 0);
    }

    #[test]
    fn filtering_narrows_and_an_empty_match_is_one_inert_row() {
        let mut m = permissions_remove(&[
            ("Bash(git status:*)".into(), "Bash(git status:*)".into()),
            ("Edit".into(), "Edit".into()),
        ]);
        m.filter = "GIT".into();
        let (o, _) = m.options();
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].0, "/permissions remove Bash(git status:*)");
        m.filter = "zzz".into();
        assert_eq!(
            m.options().0,
            [(
                "info".to_string(),
                "no option matches the filter".to_string()
            )]
        );
    }

    #[test]
    fn long_lists_ask_for_a_filter_short_ones_do_not() {
        assert!(effort(&["low".into()], None).header().is_none());
        assert!(panels(&Hidden::default()).header().is_some());
        let mut m = effort(&[], None);
        m.filter = "a".into();
        assert!(m.header().is_some());
    }

    #[test]
    fn chains_open_the_next_level_or_ask_for_text() {
        let m = permissions();
        let chain = m
            .rows
            .iter()
            .find_map(|r| split(&r.0))
            .expect("a chained row");
        assert_eq!(chain, ("permissions", "remove"));
        assert!(m.rows.iter().any(|r| r.0 == "type:/permissions allow "));
        // revoke is offered only when there is something to revoke
        assert!(trust(false).rows.iter().all(|r| split(&r.0).is_none()));
        assert!(trust(true).rows.iter().any(|r| split(&r.0).is_some()));
    }

    #[test]
    fn every_panel_row_flips_its_current_state_and_runs_a_valid_command() {
        use crate::tui_view::layout::{PanelsCmd, parse_panels};
        let h = Hidden {
            usage: true,
            focus: true,
            ..Hidden::default()
        };
        let m = panels(&h);
        assert!(m.rows.iter().any(|r| r.0 == "/panels usage on"));
        assert!(m.rows.iter().any(|r| r.0 == "/panels tools off"));
        assert!(m.rows.iter().any(|r| r.0 == "/panels focus off"));
        for (k, _) in &m.rows {
            let arg = k.strip_prefix("/panels ").expect("panels row");
            assert!(parse_panels(arg).is_ok(), "{arg}");
        }
        assert_eq!(
            parse_panels("focus off"),
            Ok(PanelsCmd::Set("focus", false))
        );
    }

    #[test]
    fn typed_rows_keep_the_command_for_the_input_box() {
        assert!(
            context("c".into())
                .rows
                .last()
                .is_some_and(|r| r.0 == "type:/context ")
        );
        assert!(
            memory()
                .rows
                .iter()
                .filter(|r| r.0.starts_with(TYPE))
                .count()
                >= 4
        );
    }
}
