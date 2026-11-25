//! Which lines of a file were added, edited or removed, read from a unified diff (`FileDiff::lines`) the way an
//! editor's gutter shows it: a change block that has removed AND added lines is an edit (blue), only added is new
//! (green), only removed is a deletion (red). Pure text work: no git, no file access.

use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Added,
    Modified,
    Removed,
}

/// The marks of one file's lines (1-based numbers of the NEW file). `removed` holds the line a deletion sits
/// above; one past the last line means the deletion was at the end of the file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Gutter {
    pub added: BTreeSet<usize>,
    pub modified: BTreeSet<usize>,
    pub removed: BTreeSet<usize>,
}

impl Gutter {
    pub fn mark(&self, line: usize) -> Option<Mark> {
        if self.modified.contains(&line) {
            Some(Mark::Modified)
        } else if self.added.contains(&line) {
            Some(Mark::Added)
        } else {
            None
        }
    }
}

/// The marks of a run of line signs (`+`, `-`, anything else is context): every sign of a change block that has
/// both `-` and `+` is `Modified`; the others are `Added` or `Removed`. For the diff views, which show both sides.
pub fn sign_marks(signs: &[char]) -> Vec<Option<Mark>> {
    let mut out = vec![None; signs.len()];
    let mut i = 0;
    while i < signs.len() {
        if !matches!(signs[i], '+' | '-') {
            i += 1;
            continue;
        }
        let end = signs[i..]
            .iter()
            .position(|c| !matches!(c, '+' | '-'))
            .map_or(signs.len(), |n| i + n);
        let both = signs[i..end].contains(&'+') && signs[i..end].contains(&'-');
        for (k, c) in signs[i..end].iter().enumerate() {
            out[i + k] = Some(match (both, c) {
                (true, _) => Mark::Modified,
                (_, '+') => Mark::Added,
                _ => Mark::Removed,
            });
        }
        i = end;
    }
    out
}

/// The sign of each line of a unified diff: `+`, `-` or ` ` (context) for a line inside a hunk, NUL for the
/// header lines, `@@` and `\ No newline`.
pub fn diff_signs(lines: &[String]) -> Vec<char> {
    let mut in_hunk = false;
    lines
        .iter()
        .map(|l| {
            if l.starts_with("@@") {
                in_hunk = true;
                return '\0';
            }
            if l.starts_with("diff ") {
                in_hunk = false;
            }
            match l.chars().next() {
                Some(c @ ('+' | '-' | ' ')) if in_hunk => c,
                None if in_hunk => ' ',
                _ => '\0',
            }
        })
        .collect()
}

/// The gutter of the new file from its diff against the old one.
pub fn gutter(lines: &[String]) -> Gutter {
    let mut g = Gutter::default();
    let mut at = 0usize; // number of the next line of the new file
    let (mut dels, mut adds): (usize, Vec<usize>) = (0, vec![]);
    let flush = |g: &mut Gutter, at: usize, dels: &mut usize, adds: &mut Vec<usize>| {
        match (*dels, adds.len()) {
            (0, 0) => {}
            (_, 0) => {
                g.removed.insert(at);
            }
            (0, _) => g.added.extend(adds.iter()),
            (d, _) => {
                g.modified.extend(adds.iter().take(d));
                g.added.extend(adds.iter().skip(d));
            }
        }
        (*dels, *adds) = (0, vec![]);
    };
    let signs = diff_signs(lines);
    for (l, s) in lines.iter().zip(signs) {
        if let Some(h) = l.strip_prefix("@@ ") {
            flush(&mut g, at, &mut dels, &mut adds);
            // `-a,b +c,d @@`: the new file continues at c
            at = h
                .split_whitespace()
                .find_map(|w| w.strip_prefix('+'))
                .and_then(|w| w.split(',').next()?.parse().ok())
                .unwrap_or(at);
            continue;
        }
        match s {
            '-' => dels += 1,
            '+' => {
                adds.push(at);
                at += 1;
            }
            c => {
                flush(&mut g, at, &mut dels, &mut adds);
                at += usize::from(c == ' '); // a context line of the hunk
            }
        }
    }
    flush(&mut g, at, &mut dels, &mut adds);
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    const HEAD: &str = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n";

    #[test]
    fn added_modified_and_removed_lines_are_told_apart() {
        // line 2 edited, line 4 new, old line 6 deleted (now sits above new line 7)
        let d = v(&format!(
            "{HEAD}@@ -1,6 +1,6 @@\n one\n-two\n+TWO\n three\n+four\n five\n-six\n seven"
        ));
        let g = gutter(&d);
        assert_eq!(g.modified, BTreeSet::from([2]));
        assert_eq!(g.added, BTreeSet::from([4]));
        assert_eq!(g.removed, BTreeSet::from([6]));
        assert_eq!(g.mark(2), Some(Mark::Modified));
        assert_eq!(g.mark(4), Some(Mark::Added));
        assert_eq!(g.mark(3), None);
    }

    #[test]
    fn more_added_than_removed_makes_the_rest_added() {
        let d = v(&format!("{HEAD}@@ -1,2 +1,4 @@\n a\n-b\n+B\n+c\n+d"));
        let g = gutter(&d);
        assert_eq!(g.modified, BTreeSet::from([2]));
        assert_eq!(g.added, BTreeSet::from([3, 4]));
        assert!(g.removed.is_empty());
    }

    #[test]
    fn a_deletion_at_the_end_of_the_file_sits_one_past_the_last_line() {
        let d = v(&format!("{HEAD}@@ -1,3 +1,2 @@\n a\n b\n-c"));
        let g = gutter(&d);
        assert_eq!(g.removed, BTreeSet::from([3]));
        assert!(g.added.is_empty() && g.modified.is_empty());
    }

    #[test]
    fn a_deleted_first_line_and_two_hunks() {
        let d = v(&format!(
            "{HEAD}@@ -1,2 +1,1 @@\n-a\n b\n@@ -10,1 +9,2 @@\n x\n+y"
        ));
        let g = gutter(&d);
        assert_eq!(g.removed, BTreeSet::from([1]));
        assert_eq!(g.added, BTreeSet::from([10]));
    }

    #[test]
    fn a_new_file_is_all_added_and_content_that_looks_like_a_header_is_content() {
        let mut d = vec![
            "--- /dev/null".to_string(),
            "+++ b/n".into(),
            "@@ -0,0 +1,3 @@".into(),
        ];
        d.extend(["+x", "+++ y", "+-- z"].map(String::from));
        let g = gutter(&d);
        assert_eq!(g.added, BTreeSet::from([1, 2, 3]));
        // the same text as removed lines inside a hunk
        let r = v(&format!("{HEAD}@@ -1,2 +1,1 @@\n--- y\n a"));
        assert_eq!(gutter(&r).removed, BTreeSet::from([1]));
    }

    #[test]
    fn no_newline_marker_is_not_a_line() {
        let d = v(&format!(
            "{HEAD}@@ -1,1 +1,2 @@\n a\n\\ No newline at end of file\n+b"
        ));
        assert_eq!(gutter(&d).added, BTreeSet::from([2]));
    }

    #[test]
    fn a_change_block_with_both_signs_is_modified_in_the_diff_views() {
        use Mark::*;
        let s: Vec<char> = " -+- +  -".chars().collect();
        assert_eq!(
            sign_marks(&s),
            vec![
                None,
                Some(Modified),
                Some(Modified),
                Some(Modified),
                None,
                Some(Added),
                None,
                None,
                Some(Removed)
            ]
        );
    }
}
