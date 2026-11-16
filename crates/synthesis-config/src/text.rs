/// Characters that draw nothing (or only blank space) yet are not ASCII space: controls, bidi,
/// tag, format and variation selectors, fillers, NBSP and the Unicode space separators. One list
/// for every display sanitiser (so none can drift behind another).
pub fn is_invisible_or_blank(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{a0}'
                | '\u{ad}'
                | '\u{34f}'
                | '\u{600}'..='\u{605}'
                | '\u{61c}'
                | '\u{6dd}'
                | '\u{70f}'
                | '\u{115f}'
                | '\u{1160}'
                | '\u{1680}'
                | '\u{17b4}'
                | '\u{17b5}'
                | '\u{180e}'
                | '\u{2000}'..='\u{200f}'
                | '\u{2028}'..='\u{202f}'
                | '\u{205f}'..='\u{2069}'
                | '\u{2800}'
                | '\u{3000}'
                | '\u{3164}'
                | '\u{fe00}'..='\u{fe0f}'
                | '\u{feff}'
                | '\u{ffa0}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{e0000}'..='\u{e007f}'
                | '\u{e0100}'..='\u{e01ef}'
        )
}

#[cfg(test)]
mod tests {
    use super::is_invisible_or_blank;

    #[test]
    fn covers_format_tag_filler_and_selector_characters() {
        for c in [
            '\u{e0041}',
            '\u{2028}',
            '\u{2029}',
            '\u{600}',
            '\u{605}',
            '\u{6dd}',
            '\u{70f}',
            '\u{34f}',
            '\u{115f}',
            '\u{1160}',
            '\u{3164}',
            '\u{ffa0}',
            '\u{fe0f}',
            '\u{e0100}',
            '\u{e01ef}',
        ] {
            assert!(is_invisible_or_blank(c), "{:x}", c as u32);
        }
        assert!(!is_invisible_or_blank('a') && !is_invisible_or_blank('\u{606}'));
    }
}
