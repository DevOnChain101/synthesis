//! Multi-line prompt editor with readline-style keys and prompt history (port of `editor.js`).
//! Text is stored as `char`s so the cursor never splits a code point.

use crate::text::is_js_space;

pub const HISTORY_MAX: usize = 200;
/// One limit for every path into the prompt, in code points.
pub const EDITOR_MAX_CODE_POINTS: usize = 65_536;

/// Normalize terminal input before checking its size: CR/CRLF become LF, tabs two spaces,
/// control characters are dropped. `None` when more than `max` characters would result, so an
/// oversized change never touches the prompt or cursor.
pub fn normalized_input(text: &str, max: usize) -> Option<Vec<char>> {
    let mut out = Vec::new();
    let source = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "  ");
    for ch in source.chars() {
        let cp = ch as u32;
        if ch != '\n' && (cp < 32 || (127..=159).contains(&cp)) {
            continue;
        }
        if out.len() >= max {
            return None;
        }
        out.push(ch);
    }
    Some(out)
}

#[derive(PartialEq)]
enum Class {
    Space,
    Word,
    Punct,
}

/// readline word classes. shortcut: combining marks are recognised by block, not by the full
/// Unicode Mark category; upgrade if a script shows up that splits words wrongly.
fn class(c: char) -> Class {
    if is_js_space(c) {
        Class::Space
    } else if c.is_alphanumeric()
        || c == '_'
        || matches!(c, '\u{300}'..='\u{36f}' | '\u{1ab0}'..='\u{1aff}' | '\u{1dc0}'..='\u{1dff}'
            | '\u{200d}' | '\u{20d0}'..='\u{20ff}' | '\u{fe00}'..='\u{fe0f}' | '\u{fe20}'..='\u{fe2f}')
    {
        Class::Word
    } else {
        Class::Punct
    }
}

fn is_combining(c: char) -> bool {
    matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F)
}

#[derive(Debug, Default)]
pub struct LineEditor {
    pub chars: Vec<char>,
    pub cur: usize,
    pub history: Vec<String>,
    /// `None`: editing a draft, otherwise browsing `history[i]`.
    h_index: Option<usize>,
    draft: String,
}

impl LineEditor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Replace the text (cursor at the end). `false`, nothing changed, when it is too long.
    pub fn set(&mut self, text: &str) -> bool {
        let Some(chars) = normalized_input(text, EDITOR_MAX_CODE_POINTS) else {
            return false;
        };
        self.chars = chars;
        self.cur = self.chars.len();
        true
    }

    pub fn clear(&mut self) {
        self.set("");
        self.h_index = None;
    }

    /// Insert at the cursor. `false`, nothing changed, when the prompt would get too long.
    pub fn insert(&mut self, s: &str) -> bool {
        let Some(add) = normalized_input(s, EDITOR_MAX_CODE_POINTS - self.chars.len()) else {
            return false;
        };
        let n = add.len();
        self.chars.splice(self.cur..self.cur, add);
        self.cur += n;
        self.h_index = None;
        true
    }

    /// Deletes the character before the cursor with the combining marks that sit on it, so a
    /// decomposed `ệ` (e + marks) goes in one press like a precomposed one.
    pub fn backspace(&mut self) {
        while self.cur > 0 {
            let c = self.chars.remove(self.cur - 1);
            self.cur -= 1;
            if !is_combining(c) {
                break;
            }
        }
    }

    pub fn del(&mut self) {
        if self.cur < self.chars.len() {
            self.chars.remove(self.cur);
        }
    }

    pub fn left(&mut self) {
        self.cur = self.cur.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cur < self.chars.len() {
            self.cur += 1;
        }
    }

    fn line_start(&self) -> usize {
        let mut i = self.cur;
        while i > 0 && self.chars[i - 1] != '\n' {
            i -= 1;
        }
        i
    }

    fn line_end(&self) -> usize {
        let mut i = self.cur;
        while i < self.chars.len() && self.chars[i] != '\n' {
            i += 1;
        }
        i
    }

    pub fn home(&mut self) {
        self.cur = self.line_start();
    }

    pub fn end(&mut self) {
        self.cur = self.line_end();
    }

    pub fn word_left(&mut self) {
        let mut i = self.cur;
        while i > 0 && is_js_space(self.chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && !is_js_space(self.chars[i - 1]) {
            i -= 1;
        }
        self.cur = i;
    }

    pub fn word_right(&mut self) {
        let mut i = self.cur;
        while i < self.chars.len() && is_js_space(self.chars[i]) {
            i += 1;
        }
        while i < self.chars.len() && !is_js_space(self.chars[i]) {
            i += 1;
        }
        self.cur = i;
    }

    pub fn kill_start(&mut self) {
        let i = self.line_start();
        self.chars.drain(i..self.cur);
        self.cur = i;
    }

    pub fn kill_end(&mut self) {
        let end = self.line_end();
        self.chars.drain(self.cur..end);
    }

    /// Alt+Backspace (readline backward-kill-word): spaces, then one run of word characters or
    /// of punctuation.
    pub fn kill_word_left(&mut self) {
        let mut i = self.cur;
        while i > 0 && class(self.chars[i - 1]) == Class::Space {
            i -= 1;
        }
        if i > 0 {
            let run = class(self.chars[i - 1]);
            while i > 0 && class(self.chars[i - 1]) == run {
                i -= 1;
            }
        }
        self.chars.drain(i..self.cur);
        self.cur = i;
    }

    /// Alt+D / Alt+Delete (readline kill-word): spaces, then one run, ahead of the cursor.
    pub fn kill_word_right(&mut self) {
        let n = self.chars.len();
        let mut i = self.cur;
        while i < n && class(self.chars[i]) == Class::Space {
            i += 1;
        }
        if i < n {
            let run = class(self.chars[i]);
            while i < n && class(self.chars[i]) == run {
                i += 1;
            }
        }
        self.chars.drain(self.cur..i);
    }

    /// Ctrl+W (unix-word-rubout): whitespace-delimited.
    pub fn kill_word(&mut self) {
        let end = self.cur;
        self.word_left();
        self.chars.drain(self.cur..end);
    }

    pub fn multiline(&self) -> bool {
        self.chars.contains(&'\n')
    }

    /// Older history entry (the draft is kept for [`Self::down`]).
    pub fn up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let i = self.h_index.unwrap_or_else(|| {
            self.draft = self.text();
            self.history.len()
        });
        self.h_index = Some(i);
        if i > 0 {
            self.h_index = Some(i - 1);
            let entry = self.history[i - 1].clone();
            self.set(&entry);
        }
    }

    pub fn down(&mut self) {
        let Some(i) = self.h_index else { return };
        if i + 1 >= self.history.len() {
            self.h_index = None;
            let draft = self.draft.clone();
            self.set(&draft);
        } else {
            self.h_index = Some(i + 1);
            let entry = self.history[i + 1].clone();
            self.set(&entry);
        }
    }

    /// Returns the prompt, remembers it (blank and repeated prompts excepted) and clears.
    pub fn submit(&mut self) -> String {
        let t = self.text();
        if !t.trim_matches(is_js_space).is_empty() && self.history.last() != Some(&t) {
            self.history.push(t.clone());
            if self.history.len() > HISTORY_MAX {
                self.history.remove(0);
            }
        }
        self.clear();
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Translated from Node test/editor.test.js.
    #[test]
    fn backspace_removes_a_decomposed_letter_whole() {
        let mut e = LineEditor::new();
        e.insert("Vie\u{0302}\u{0323}t"); // Việt, NFD
        e.backspace();
        e.backspace();
        assert_eq!(e.text(), "Vi");
    }

    #[test]
    fn bounds_are_code_points_normalized_and_atomic() {
        let mut e = LineEditor::new();
        assert!(e.insert("a\r\nb\tc\0\x1bd"));
        assert_eq!(e.text(), "a\nb  cd");
        e.cur = 2;
        let before = e.text();
        assert!(!e.insert(&"x".repeat(EDITOR_MAX_CODE_POINTS)));
        assert_eq!(e.text(), before);
        assert_eq!(e.cur, 2, "rejected input preserves the cursor too");
        e.clear();
        assert!(e.insert(&"😀".repeat(EDITOR_MAX_CODE_POINTS)));
        assert_eq!(
            e.chars.len(),
            EDITOR_MAX_CODE_POINTS,
            "astral characters count once"
        );
        assert!(!e.insert("x"));
    }

    #[test]
    fn cursor_movement_kills_history() {
        let mut e = LineEditor::new();
        e.insert("hello world");
        e.word_left();
        e.insert("big ");
        assert_eq!(e.text(), "hello big world");
        e.kill_end();
        assert_eq!(e.text(), "hello big ");
        e.kill_word();
        assert_eq!(e.text(), "hello ");
        assert_eq!(e.submit(), "hello ");
        e.insert("draft");
        e.up();
        assert_eq!(e.text(), "hello ");
        e.down();
        assert_eq!(e.text(), "draft");
        e.clear();
        e.insert("a\nb");
        assert!(e.multiline());
    }

    fn ed(text: &str, cur: usize) -> LineEditor {
        let mut e = LineEditor::new();
        e.insert(text);
        e.cur = cur;
        e
    }

    #[test]
    fn alt_backspace_is_readline_backward_kill_word() {
        let mut e = ed("foo bar_baz  ", 13);
        e.kill_word_left(); // trailing spaces, then the word run (underscore is a word char)
        assert_eq!(e.text(), "foo ");
        e.kill_word_left();
        assert_eq!(e.text(), "");
        e.kill_word_left(); // nothing to do at the start
        assert_eq!(e.text(), "");
        // punctuation is its own run, unlike Ctrl+W
        let mut e = ed("cd ~/src/synth-app", 18);
        e.kill_word_left();
        assert_eq!(e.text(), "cd ~/src/synth-");
        e.kill_word_left();
        assert_eq!(e.text(), "cd ~/src/synth");
        let mut w = ed("cd ~/src/synth-app", 18);
        w.kill_word();
        assert_eq!(w.text(), "cd ");
        // cursor in the middle: the tail stays
        let mut e = ed("alpha beta gamma", 10);
        e.kill_word_left();
        assert_eq!((e.text().as_str(), e.cur), ("alpha  gamma", 6));
    }

    #[test]
    fn word_kills_are_unicode_and_multiline_aware() {
        let mut e = ed("xin chào việt nam", 17);
        e.kill_word_left();
        assert_eq!(e.text(), "xin chào việt ");
        let mut e = ed("a\nnaïve cafe\u{301}", 13);
        e.kill_word_left(); // combining accent stays inside the word
        assert_eq!(e.text(), "a\nnaïve ");
        e.kill_word_left();
        e.kill_word_left(); // crosses the newline like readline crosses whitespace
        assert_eq!(e.text(), "");
        let mut e = ed("日本語 text", 3);
        e.kill_word_left();
        assert_eq!((e.text().as_str(), e.cur), (" text", 0));
    }

    #[test]
    fn alt_d_kills_forward() {
        let mut e = ed("foo  bar.baz", 0);
        e.kill_word_right();
        assert_eq!((e.text().as_str(), e.cur), ("  bar.baz", 0));
        e.kill_word_right();
        assert_eq!(e.text(), ".baz");
        e.kill_word_right();
        assert_eq!(e.text(), "baz");
        e.kill_word_right();
        e.kill_word_right();
        assert_eq!(e.text(), "");
        let mut e = ed("one two", 3);
        e.kill_word_right();
        assert_eq!(e.text(), "one");
    }

    #[test]
    fn each_editor_has_its_own_history() {
        let (mut a, mut b) = (LineEditor::new(), LineEditor::new());
        a.insert("only in a");
        a.submit();
        assert!(b.history.is_empty());
        b.up();
        assert_eq!(b.text(), "", "nothing to recall in the other tab");
        a.up();
        assert_eq!(a.text(), "only in a");
    }

    #[test]
    fn line_keys_and_history_cap() {
        let mut e = LineEditor::new();
        e.insert("one\ntwo three");
        e.home();
        assert_eq!(e.cur, 4);
        e.kill_end();
        assert_eq!(e.text(), "one\n");
        e.cur = 4;
        e.insert("x");
        e.kill_start();
        assert_eq!(e.text(), "one\n");
        for i in 0..250 {
            e.insert(&format!("p{i}"));
            e.submit();
        }
        assert_eq!(e.history.len(), HISTORY_MAX);
        assert_eq!(e.history[0], "p50");
        assert_eq!(e.submit(), "");
        assert_eq!(
            e.history.len(),
            HISTORY_MAX,
            "blank prompts are not remembered"
        );
    }
}
