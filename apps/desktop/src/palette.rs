//! The Cmd/Ctrl+K command palette: a searchable list of pages and actions. Pure logic; the window
//! draws it (`ui`).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    NewTask,
    Sessions,
    Inbox,
    Activity,
    Tools,
    Topology,
    Configuration,
}

pub const ENTRIES: [(&str, Command); 7] = [
    ("New task", Command::NewTask),
    ("Sessions", Command::Sessions),
    ("Inbox", Command::Inbox),
    ("Activity", Command::Activity),
    ("Tools", Command::Tools),
    ("Agent Topology", Command::Topology),
    ("Configuration", Command::Configuration),
];

/// Entries whose label contains every word of `q` (case-insensitive); all of them for an empty query.
pub fn filter(q: &str) -> Vec<(&'static str, Command)> {
    let q = q.to_lowercase();
    ENTRIES
        .into_iter()
        .filter(|(l, _)| q.split_whitespace().all(|w| l.to_lowercase().contains(w)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_lists_everything() {
        assert_eq!(filter("  ").len(), ENTRIES.len());
    }

    #[test]
    fn words_narrow_the_list() {
        assert_eq!(filter("topo"), vec![("Agent Topology", Command::Topology)]);
        assert_eq!(filter("CONF")[0].1, Command::Configuration);
        assert!(filter("zzz").is_empty());
    }
}
