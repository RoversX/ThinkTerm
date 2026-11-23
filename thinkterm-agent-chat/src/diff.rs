//! Reading a unified diff line by line, the same way wherever it is
//! counted or shown.

/// What a line of a diff is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    Added,
    Removed,
    Context,
    /// `@@ -1,3 +1,4 @@`, or a bare `@@`.
    Hunk,
    /// A file's header (`--- a/x`, `+++ b/x`, `diff --git`, `index`),
    /// or `\ No newline at end of file`: not a line of the file.
    Header,
}

/// Where the reading is: between hunks, where files' headers are; in a
/// hunk whose header counts its lines; or in a bare `@@` (a diff made up
/// from what an agent was asked), which runs to the next.
enum At {
    Between,
    Counted { old: u64, new: u64 },
    Open,
}

/// The lines of `diff`, each with what it is. A hunk's count is what
/// tells a removed line that reads `--- x` (a `-- x` taken out) from the
/// header of the next file.
pub fn lines(diff: &str) -> Vec<(Line, &str)> {
    let mut at = At::Between;
    let mut out = Vec::new();
    for line in diff.lines() {
        if line.starts_with("@@") {
            at = match counts(line) {
                Some((old, new)) => At::Counted { old, new },
                None => At::Open,
            };
            out.push((Line::Hunk, line));
            continue;
        }
        if line.starts_with('\\') {
            out.push((Line::Header, line));
            continue;
        }
        let kind = match &mut at {
            At::Counted { old, new } => {
                let kind = match line.as_bytes().first() {
                    Some(b'+') => {
                        *new = new.saturating_sub(1);
                        Line::Added
                    }
                    Some(b'-') => {
                        *old = old.saturating_sub(1);
                        Line::Removed
                    }
                    _ => {
                        *old = old.saturating_sub(1);
                        *new = new.saturating_sub(1);
                        Line::Context
                    }
                };
                if *old == 0 && *new == 0 {
                    at = At::Between;
                }
                kind
            }
            At::Open if line.starts_with("diff ") => {
                at = At::Between;
                Line::Header
            }
            At::Open => by_mark(line),
            At::Between if is_header(line) => Line::Header,
            // A diff with no hunks (one shown as lines with a mark each).
            At::Between => by_mark(line),
        };
        out.push((kind, line));
    }
    out
}

fn by_mark(line: &str) -> Line {
    match line.as_bytes().first() {
        Some(b'+') => Line::Added,
        Some(b'-') => Line::Removed,
        _ => Line::Context,
    }
}

fn is_header(line: &str) -> bool {
    const HEADERS: &[&str] = &[
        "---",
        "+++",
        "diff ",
        "index ",
        "new file mode",
        "deleted file mode",
        "old mode",
        "new mode",
        "similarity index",
        "dissimilarity index",
        "rename from",
        "rename to",
        "copy from",
        "copy to",
        "Binary files",
    ];
    HEADERS.iter().any(|header| line.starts_with(header))
}

/// The old and new line counts of `@@ -a[,b] +c[,d] @@`; a count left
/// out is one.
fn counts(line: &str) -> Option<(u64, u64)> {
    let mut words = line.split_whitespace();
    if words.next() != Some("@@") {
        return None;
    }
    let count = |word: Option<&str>, mark: char| -> Option<u64> {
        let range = word?.strip_prefix(mark)?;
        match range.split_once(',') {
            Some((start, count)) => {
                start.parse::<u64>().ok()?;
                count.parse().ok()
            }
            None => range.parse::<u64>().ok().map(|_| 1),
        }
    };
    let old = count(words.next(), '-')?;
    let new = count(words.next(), '+')?;
    Some((old, new))
}

/// Lines added and removed.
pub fn count(diff: &str) -> (u32, u32) {
    let (mut added, mut removed) = (0u32, 0u32);
    for (kind, _) in lines(diff) {
        match kind {
            Line::Added => added = added.saturating_add(1),
            Line::Removed => removed = removed.saturating_add(1),
            _ => {}
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(diff: &str) -> Vec<Line> {
        lines(diff).into_iter().map(|(kind, _)| kind).collect()
    }

    #[test]
    fn a_removed_line_that_reads_like_a_header_is_a_removed_line() {
        // `---` (YAML) and `-- note` (SQL) taken out, then the next file.
        let diff = "--- a/x.yml\n+++ b/x.yml\n@@ -1,3 +1,1 @@\n----\n--- note\n keep\ndiff --git a/y b/y\n--- a/y\n+++ b/y\n@@ -1 +1 @@\n-a\n+b\n";
        use Line::*;
        assert_eq!(
            kinds(diff),
            [Header, Header, Hunk, Removed, Removed, Context, Header, Header, Header, Hunk, Removed, Added]
        );
        assert_eq!(count(diff), (1, 3));
    }

    #[test]
    fn a_bare_hunk_runs_on_and_no_hunk_reads_by_mark() {
        use Line::*;
        // What was asked, made into a diff: `+++i;` is a line written.
        assert_eq!(kinds("@@\n-old\n+++i;\n"), [Hunk, Removed, Added]);
        // OMP's own lines, with no hunk.
        assert_eq!(kinds("-1|alpha\n+1|ALPHA\n"), [Removed, Added]);
        assert_eq!(kinds("@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n"), [Hunk, Removed, Header, Added]);
    }

    #[test]
    fn hunk_counts_are_read() {
        assert_eq!(counts("@@ -10,7 +10,9 @@ fn x()"), Some((7, 9)));
        assert_eq!(counts("@@ -1 +1,0 @@"), Some((1, 0)));
        assert_eq!(counts("@@"), None);
        assert_eq!(counts("@@ x @@"), None);
    }
}
