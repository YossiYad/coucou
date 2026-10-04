// What a change looks like before it is made: a diff for text, the affected
// rows and columns for a spreadsheet, the affected paragraphs for a document.
// The island draws these in the work view, whoever makes the change: the chat
// model with its tools, or Claude Code through its hooks.

use serde::Serialize;

/// How a line, row, column, cell or paragraph is affected.
#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Mark {
    Same,
    Removed,
    Added,
    Changed,
    /// Unchanged lines left out between two parts of a change.
    Gap,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub mark: Mark,
    pub text: String,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Head {
    pub label: String,
    pub mark: Mark,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Cell {
    pub text: String,
    pub old: Option<String>,
    pub mark: Mark,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub label: String,
    pub mark: Mark,
    pub cells: Vec<Cell>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub label: String,
    pub mark: Mark,
    pub text: String,
    pub old: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Preview {
    Text { lines: Vec<Line> },
    Table { sheet: String, columns: Vec<Head>, rows: Vec<Row> },
    Doc { blocks: Vec<Block> },
}

/// Unchanged lines kept around each part of a change.
const CONTEXT: usize = 3;
/// Lines shown at most; the rest of a huge change is summed up in one line.
const MAX_LINES: usize = 400;
/// Above this many lines on each side the middle is not diffed line by line.
const MAX_DIFF_CELLS: usize = 4_000_000;
const CELL_CHARS: usize = 28;

/// The changed parts of a text, with a few unchanged lines around each.
pub fn text_diff(old: &str, new: &str) -> Vec<Line> {
    let a: Vec<&str> = if old.is_empty() { Vec::new() } else { old.lines().collect() };
    let b: Vec<&str> = if new.is_empty() { Vec::new() } else { new.lines().collect() };
    let ops = diff_ops(&a, &b);

    // Which ops are worth showing: every change and the context around it.
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, op)| op.mark != Mark::Same).map(|(i, _)| i).collect();
    let mut keep = vec![false; ops.len()];
    for &i in &changed {
        let from = i.saturating_sub(CONTEXT);
        let to = (i + CONTEXT + 1).min(ops.len());
        keep[from..to].iter_mut().for_each(|k| *k = true);
    }

    let mut out = Vec::new();
    let mut skipped = false;
    for (i, op) in ops.iter().enumerate() {
        if !keep[i] {
            skipped = true;
            continue;
        }
        if skipped && !out.is_empty() {
            out.push(Line { old: None, new: None, mark: Mark::Gap, text: String::new() });
        }
        skipped = false;
        if out.len() >= MAX_LINES {
            let left = keep[i..].iter().filter(|k| **k).count();
            out.push(Line { old: None, new: None, mark: Mark::Gap, text: format!("{left} more lines") });
            break;
        }
        out.push(op.clone());
    }
    out
}

/// Every line of both texts as kept, removed or added, in order. The common
/// start and end are matched directly, so an edit in a long file only diffs
/// the part that differs.
fn diff_ops(a: &[&str], b: &[&str]) -> Vec<Line> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (mid_a, mid_b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);

    let mut ops: Vec<Line> = Vec::with_capacity(a.len().max(b.len()) + 8);
    let same = |i: usize, j: usize, text: &str| Line { old: Some(i + 1), new: Some(j + 1), mark: Mark::Same, text: text.to_string() };
    for i in 0..prefix {
        ops.push(same(i, i, a[i]));
    }

    let removed = |i: usize| Line { old: Some(prefix + i + 1), new: None, mark: Mark::Removed, text: mid_a[i].to_string() };
    let added = |j: usize| Line { old: None, new: Some(prefix + j + 1), mark: Mark::Added, text: mid_b[j].to_string() };
    if mid_a.len() * mid_b.len() > MAX_DIFF_CELLS {
        // Too big to match line by line: show it as replaced.
        ops.extend((0..mid_a.len()).map(removed));
        ops.extend((0..mid_b.len()).map(added));
    } else {
        // Longest common subsequence, filled from the end so it reads forwards.
        let (n, m) = (mid_a.len(), mid_b.len());
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * (m + 1) + j] = if mid_a[i] == mid_b[j] {
                    lcs[(i + 1) * (m + 1) + j + 1] + 1
                } else {
                    lcs[(i + 1) * (m + 1) + j].max(lcs[i * (m + 1) + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n || j < m {
            if i < n && j < m && mid_a[i] == mid_b[j] {
                ops.push(same(prefix + i, prefix + j, mid_a[i]));
                i += 1;
                j += 1;
            } else if j < m && (i == n || lcs[i * (m + 1) + j + 1] >= lcs[(i + 1) * (m + 1) + j]) {
                ops.push(added(j));
                j += 1;
            } else {
                ops.push(removed(i));
                i += 1;
            }
        }
        // Removed lines read before the lines that replace them.
        regroup(&mut ops);
    }

    for k in 0..suffix {
        let (i, j) = (a.len() - suffix + k, b.len() - suffix + k);
        ops.push(same(i, j, a[i]));
    }
    ops
}

/// Within each run of changes, removals first, then additions.
fn regroup(ops: &mut [Line]) {
    let mut start = 0;
    while start < ops.len() {
        if ops[start].mark == Mark::Same {
            start += 1;
            continue;
        }
        let end = ops[start..].iter().position(|l| l.mark == Mark::Same).map_or(ops.len(), |p| start + p);
        ops[start..end].sort_by_key(|l| l.mark != Mark::Removed);
        start = end;
    }
}

/// A file's first lines, shown while it is being read.
pub fn excerpt(text: &str, lines: usize) -> Vec<Line> {
    text.lines()
        .take(lines)
        .enumerate()
        .map(|(i, l)| Line { old: Some(i + 1), new: Some(i + 1), mark: Mark::Same, text: l.to_string() })
        .collect()
}

pub fn clip_cell(text: &str) -> String {
    let text = text.replace(['\n', '\r', '\t'], " ");
    if text.chars().count() <= CELL_CHARS {
        text
    } else {
        format!("{}…", text.chars().take(CELL_CHARS - 1).collect::<String>())
    }
}

/// `3` as `C`, `27` as `AA`.
pub fn column_letters(mut col: u32) -> String {
    let mut out = Vec::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        out.push((b'A' + rem as u8) as char);
        col = (col - 1) / 26;
    }
    out.iter().rev().collect()
}

/// Which of `1..=total` to show: the first, the ones touched and one either
/// side of them, with `None` where a stretch is left out. Everything when it
/// fits in `room`.
pub fn pick(total: u32, touched: &[u32], room: usize) -> Vec<Option<u32>> {
    if total as usize <= room {
        return (1..=total).map(Some).collect();
    }
    let mut wanted: Vec<u32> = vec![1];
    if touched.is_empty() {
        wanted.extend(2..=(room as u32).min(total));
    }
    for &t in touched {
        for n in t.saturating_sub(1)..=t + 1 {
            if (1..=total).contains(&n) {
                wanted.push(n);
            }
        }
    }
    wanted.sort_unstable();
    wanted.dedup();
    let mut out = Vec::new();
    let mut last = 0;
    for n in wanted {
        if n > last + 1 && last > 0 {
            out.push(None);
        }
        out.push(Some(n));
        last = n;
    }
    out
}

/// A new table, every row added.
pub fn new_table(sheet: &str, rows: &[Vec<String>], max_rows: usize) -> Preview {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0) as u32;
    let columns = (1..=width).map(|c| Head { label: column_letters(c), mark: Mark::Added }).collect();
    let mut out: Vec<Row> = rows
        .iter()
        .take(max_rows)
        .enumerate()
        .map(|(i, r)| Row {
            label: (i + 1).to_string(),
            mark: Mark::Added,
            cells: (0..width as usize)
                .map(|c| Cell { text: clip_cell(r.get(c).map_or("", String::as_str)), old: None, mark: Mark::Added })
                .collect(),
        })
        .collect();
    if rows.len() > max_rows {
        out.push(Row { label: String::new(), mark: Mark::Gap, cells: Vec::new() });
    }
    Preview::Table { sheet: sheet.to_string(), columns, rows: out }
}

/// Files larger than this are not read to show a Claude Code edit in context.
const MAX_HOOK_FILE: u64 = 2 * 1024 * 1024;

/// Whether any text in a hook payload was cut short by the relay.
fn is_cut(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(s) => s.len() >= 2_000 && s.ends_with('…'),
        serde_json::Value::Array(items) => items.iter().any(is_cut),
        serde_json::Value::Object(map) => map.values().any(is_cut),
        _ => false,
    }
}

/// What a Claude Code Edit, MultiEdit or Write is about to do to its file,
/// worked out from the hook payload and the file as it is now.
pub fn for_hook(tool: &str, input: &serde_json::Value) -> Option<Preview> {
    let text = |v: &serde_json::Value, k: &str| v.get(k).and_then(serde_json::Value::as_str).map(str::to_string);
    let path = text(input, "file_path")?;
    // The relay cuts very long fields ("…" at the end): a diff of a cut
    // change would show text being deleted that is not.
    if is_cut(input) {
        return None;
    }
    let old = std::fs::metadata(&path)
        .ok()
        .filter(|m| m.is_file() && m.len() <= MAX_HOOK_FILE)
        .and_then(|_| std::fs::read_to_string(&path).ok())
        .unwrap_or_default();
    let edit = |current: &str, e: &serde_json::Value| -> Option<String> {
        let (find, replace) = (text(e, "old_string")?, text(e, "new_string")?);
        let all = e.get("replace_all").and_then(serde_json::Value::as_bool).unwrap_or(false);
        if find.is_empty() || !current.contains(&find) {
            return None;
        }
        Some(if all { current.replace(&find, &replace) } else { current.replacen(&find, &replace, 1) })
    };
    let lines = match tool {
        "Write" => text_diff(&old, &text(input, "content")?),
        "Edit" => match edit(&old, input) {
            Some(new) => text_diff(&old, &new),
            // Not in the file as read: show the replacement on its own.
            None => text_diff(&text(input, "old_string")?, &text(input, "new_string")?),
        },
        "MultiEdit" => {
            let mut current = old.clone();
            for e in input.get("edits")?.as_array()? {
                current = edit(&current, e)?;
            }
            text_diff(&old, &current)
        }
        _ => return None,
    };
    Some(Preview::Text { lines })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| match l.mark {
                Mark::Same => ' ',
                Mark::Removed => '-',
                Mark::Added => '+',
                Mark::Changed => '~',
                Mark::Gap => '…',
            })
            .collect()
    }

    #[test]
    fn a_one_line_edit_shows_the_line_out_and_in_with_context() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\n";
        let new = "a\nb\nc\nd\nE\nf\ng\nh\ni\n";
        let diff = text_diff(old, new);
        assert_eq!(marks(&diff), "   -+   ");
        let removed = &diff[3];
        assert_eq!((removed.old, removed.new, removed.text.as_str()), (Some(5), None, "e"));
        assert_eq!((diff[4].old, diff[4].new, diff[4].text.as_str()), (None, Some(5), "E"));
    }

    #[test]
    fn far_apart_changes_are_split_by_a_gap() {
        let old: String = (1..=30).map(|n| format!("{n}\n")).collect();
        let new = old.replace("\n2\n", "\ntwo\n").replace("\n28\n", "\n");
        let diff = text_diff(&old, &new);
        assert!(marks(&diff).contains('…'));
        assert_eq!(diff.iter().filter(|l| l.mark == Mark::Removed).count(), 2);
        assert_eq!(diff.iter().filter(|l| l.mark == Mark::Added).count(), 1);
    }

    #[test]
    fn a_new_file_is_all_added() {
        let diff = text_diff("", "one\ntwo");
        assert_eq!(marks(&diff), "++");
        assert_eq!(diff[1].new, Some(2));
    }

    #[test]
    fn rows_and_columns_to_show_keep_the_header_and_the_neighbours() {
        assert_eq!(pick(5, &[3], 8), vec![Some(1), Some(2), Some(3), Some(4), Some(5)]);
        assert_eq!(pick(40, &[20], 8), vec![Some(1), None, Some(19), Some(20), Some(21)]);
        assert_eq!(pick(40, &[2], 8), vec![Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn a_claude_code_edit_is_shown_in_its_file() {
        let dir = std::env::temp_dir().join(format!("coucou-preview-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("invoice.ts");
        std::fs::write(&file, "import x\n\nconst TVA = 0.196\n\nexport total\n").unwrap();
        let input = serde_json::json!({
            "file_path": file.to_string_lossy(),
            "old_string": "const TVA = 0.196",
            "new_string": "const TVA = 0.20",
        });
        let Some(Preview::Text { lines }) = for_hook("Edit", &input) else { panic!("no preview") };
        let removed = lines.iter().find(|l| l.mark == Mark::Removed).unwrap();
        assert_eq!((removed.old, removed.text.as_str()), (Some(3), "const TVA = 0.196"));
        let added = lines.iter().find(|l| l.mark == Mark::Added).unwrap();
        assert_eq!((added.new, added.text.as_str()), (Some(3), "const TVA = 0.20"));
        assert!(for_hook("Bash", &serde_json::json!({ "command": "ls" })).is_none());
        let cut = serde_json::json!({ "file_path": file.to_string_lossy(), "content": format!("{}…", "x".repeat(2_000)) });
        assert!(for_hook("Write", &cut).is_none(), "a cut Write must not be shown as the new file");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn column_letters_count_like_excel() {
        assert_eq!(column_letters(1), "A");
        assert_eq!(column_letters(26), "Z");
        assert_eq!(column_letters(27), "AA");
        assert_eq!(column_letters(703), "AAA");
    }
}
