//! Where a text breaks into lines ([`Text::wrap`], and a field of several
//! lines): one rule for every client, each measuring the text with its own
//! fonts and handing the measures here.
//!
//! [`Text::wrap`]: crate::scene::Text::wrap

/// Where `chars`, whose boundaries are at `xs` -- as many as it has
/// characters, and one -- break into lines no wider than `width`: at the
/// last place that keeps one within it, after a space or between the
/// characters of a script written without spaces, or else where it
/// overflows. Only between whole clusters -- a letter and its accents, an
/// emoji's parts, which `xs` gives the cluster's start -- and a line's
/// spaces stay at its end, hanging past the edge.
pub fn lines(chars: &[char], xs: &[f32], width: f32) -> Vec<std::ops::Range<usize>> {
    let count = chars.len();
    let bounds: Vec<usize> = (0..=count)
        .filter(|&at| at == 0 || at == count || xs[at] > xs[at - 1])
        .collect();
    let mut lines = Vec::new();
    let mut from = 0;
    loop {
        let start = bounds[from];
        // A cluster at least, then as many more as fit.
        let mut to = (from + 1).min(bounds.len() - 1);
        while let Some(&next) = bounds.get(to + 1) {
            let hangs = chars[bounds[to]..next].iter().all(|c| c.is_whitespace());
            if !hangs && xs[next] - xs[start] > width {
                break;
            }
            to += 1;
        }
        if bounds[to] < count {
            if let Some(found) = (from + 1..=to)
                .rev()
                .find(|&at| breaks_before(chars, bounds[at]))
            {
                to = found;
            }
        }
        lines.push(start..bounds[to]);
        from = to;
        if bounds[from] >= count {
            return lines;
        }
    }
}

/// `text` in lines of no more than `cols` cells, `wide` being how many each
/// of its characters takes: broken where [`lines`] breaks, and without the
/// spaces a line ends in. For a client that draws in cells, and says in
/// them what it cannot draw.
pub fn cells(text: &str, cols: usize, wide: impl Fn(char) -> usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut xs = vec![0.0];
    for ch in &chars {
        xs.push(xs[xs.len() - 1] + wide(*ch) as f32);
    }
    lines(&chars, &xs, cols as f32)
        .into_iter()
        .map(|line| chars[line].iter().collect::<String>().trim_end().to_string())
        .collect()
}

/// Whether a line may break before character `at` of `chars`: after a
/// space, or where the characters on either side are of a script written
/// without spaces -- but not before the punctuation that closes one.
pub fn breaks_before(chars: &[char], at: usize) -> bool {
    let (Some(before), Some(after)) = (chars.get(at.wrapping_sub(1)), chars.get(at)) else {
        return false;
    };
    if before.is_whitespace() {
        return true;
    }
    let closing = matches!(
        after,
        '、' | '。'
            | '，'
            | '．'
            | '：'
            | '；'
            | '！'
            | '？'
            | '）'
            | '」'
            | '』'
            | '】'
            | '〉'
            | '》'
            | '〕'
            | 'ー'
            | '…'
    );
    (unspaced(*before) || unspaced(*after)) && !after.is_whitespace() && !closing
}

/// A character of a script written without spaces between its words:
/// Chinese, Japanese, Korean, and their punctuation.
fn unspaced(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2E80..=0x2FFF
            | 0x3000..=0x303F
            | 0x3040..=0x30FF
            | 0x3100..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xAC00..=0xD7AF
            | 0xF900..=0xFAFF
            | 0xFF00..=0xFFEF
            | 0x20000..=0x3FFFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_breaks_after_a_space_between_unspaced_characters_or_where_it_overflows() {
        // A column a character, two for Chinese.
        let lines = |text: &str, width: f32| {
            let chars: Vec<char> = text.chars().collect();
            let mut xs = vec![0.0];
            for ch in &chars {
                let wide = if (*ch as u32) >= 0x2E80 { 2.0 } else { 1.0 };
                xs.push(xs.last().unwrap() + wide);
            }
            super::lines(&chars, &xs, width)
                .into_iter()
                .map(|line| {
                    chars[line]
                        .iter()
                        .collect::<String>()
                        .trim_end()
                        .to_string()
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            lines("the quick brown fox", 10.0),
            ["the quick", "brown fox"]
        );
        assert_eq!(
            lines("a b c d", 3.0),
            ["a b", "c d"],
            "a space hangs past the edge"
        );
        assert_eq!(lines("abcdefghijkl", 5.0), ["abcde", "fghij", "kl"]);
        assert_eq!(lines("中文的句子很长", 6.0), ["中文的", "句子很", "长"]);
        assert_eq!(lines("中文。句子", 4.0), ["中", "文。", "句子"]);
        assert_eq!(lines("", 4.0), [""]);

        // In cells, each line without the spaces it ends in.
        let wide = |ch: char| if (ch as u32) >= 0x2E80 { 2 } else { 1 };
        assert_eq!(cells("open it in a browser", 8, wide), ["open it", "in a", "browser"]);
        assert_eq!(cells("在浏览器里打开", 6, wide), ["在浏览", "器里打", "开"]);

        // A letter and its accent are one cluster: its start is theirs.
        let chars: Vec<char> = "ab\u{301}c".chars().collect();
        let broken = super::lines(&chars, &[0.0, 10.0, 10.0, 20.0, 30.0], 10.0);
        assert_eq!(broken, [0..1, 1..3, 3..4]);
    }
}
