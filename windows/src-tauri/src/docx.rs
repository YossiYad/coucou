// Word documents (.docx) as numbered paragraphs and table rows, changed in
// place: only the XML of what changes is touched, so styles, lists, images,
// headers and the rest of the file stay exactly as they were.

use std::io::{Read, Write};
use std::path::Path;

use crate::extract;
use crate::preview::{Block, Mark, Preview};

const DOCUMENT: &str = "word/document.xml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    start: usize,
    end: usize,
}

#[derive(Debug)]
enum Item {
    Paragraph(Span),
    Table { span: Span, rows: Vec<Span> },
    Other(Span),
}

pub struct Document {
    xml: String,
    items: Vec<Item>,
}

/// What the model asked for, with paragraphs and rows numbered as
/// `numbered_text` shows them.
#[derive(Debug, Default)]
pub struct Edits {
    pub delete_paragraphs: Vec<u32>,
    /// (table, row), both from 1.
    pub delete_rows: Vec<(u32, u32)>,
    /// (only in this paragraph, find, replace)
    pub replace: Vec<(Option<u32>, String, String)>,
    /// (after this paragraph, 0 for the very start; the new paragraphs' lines)
    pub insert: Vec<(u32, Vec<String>)>,
}

pub struct Plan {
    pub xml: String,
    pub preview: Preview,
}

pub fn open(path: &Path) -> Result<Document, String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| "This is not a Word document that can be opened.".to_string())?;
    let mut xml = String::new();
    archive
        .by_name(DOCUMENT)
        .map_err(|_| "This Word document has no body.".to_string())?
        .read_to_string(&mut xml)
        .map_err(|e| e.to_string())?;
    parse(xml)
}

fn parse(xml: String) -> Result<Document, String> {
    let bad = || "The document's body could not be read.".to_string();
    let open = xml.find("<w:body").ok_or_else(bad)?;
    let from = open + xml[open..].find('>').ok_or_else(bad)? + 1;
    let to = xml.rfind("</w:body>").ok_or_else(bad)?;
    let items = children(&xml, from, to)
        .into_iter()
        .map(|(name, span)| match name.as_str() {
            "w:p" => Item::Paragraph(span),
            "w:tbl" => {
                let (a, b) = inner(&xml, span);
                let rows = children(&xml, a, b).into_iter().filter(|(n, _)| n == "w:tr").map(|(_, s)| s).collect();
                Item::Table { span, rows }
            }
            _ => Item::Other(span),
        })
        .collect();
    Ok(Document { xml, items })
}

/// The direct child elements between `from` and `to`, with where each starts and ends.
fn children(xml: &str, from: usize, to: usize) -> Vec<(String, Span)> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let (mut open_at, mut open_name) = (0, String::new());
    let mut i = from;
    while i < to {
        let Some(lt) = xml[i..to].find('<').map(|p| i + p) else { break };
        let Some(gt) = xml[lt..to].find('>').map(|p| lt + p) else { break };
        let tag = &xml[lt + 1..gt];
        i = gt + 1;
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        let name = tag.trim_start_matches('/').split(|c: char| c.is_whitespace() || c == '/').next().unwrap_or("");
        if tag.starts_with('/') {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                out.push((open_name.clone(), Span { start: open_at, end: i }));
            }
        } else if tag.ends_with('/') {
            if depth == 0 {
                out.push((name.to_string(), Span { start: lt, end: i }));
            }
        } else {
            if depth == 0 {
                open_at = lt;
                open_name = name.to_string();
            }
            depth += 1;
        }
    }
    out
}

/// What lies between an element's opening and closing tags.
fn inner(xml: &str, span: Span) -> (usize, usize) {
    let text = &xml[span.start..span.end];
    if text.ends_with("/>") && !text.contains("</") {
        return (span.end, span.end);
    }
    let open_end = span.start + text.find('>').map_or(0, |p| p + 1);
    let close = span.start + text.rfind("</").unwrap_or(text.len());
    (open_end, close.max(open_end))
}

fn text_of(xml: &str, span: Span) -> String {
    extract::xml_text_word(&xml[span.start..span.end]).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn row_text(xml: &str, row: Span) -> String {
    let (a, b) = inner(xml, row);
    children(xml, a, b)
        .into_iter()
        .filter(|(n, _)| n == "w:tc")
        .map(|(_, cell)| text_of(xml, cell))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Place {
    Paragraph(u32),
    Row(u32, u32),
    Other,
}

struct Entry {
    place: Place,
    span: Span,
    text: String,
}

impl Document {
    /// Every paragraph and table row in order, numbered from 1.
    fn entries(&self) -> Vec<Entry> {
        let (mut p, mut t) = (0, 0);
        let mut out = Vec::new();
        for item in &self.items {
            match item {
                Item::Paragraph(span) => {
                    p += 1;
                    out.push(Entry { place: Place::Paragraph(p), span: *span, text: text_of(&self.xml, *span) });
                }
                Item::Table { rows, .. } => {
                    t += 1;
                    for (r, row) in rows.iter().enumerate() {
                        out.push(Entry { place: Place::Row(t, r as u32 + 1), span: *row, text: row_text(&self.xml, *row) });
                    }
                }
                Item::Other(span) => out.push(Entry { place: Place::Other, span: *span, text: text_of(&self.xml, *span) }),
            }
        }
        out
    }

    /// The document as the model reads it: `[¶3] text`, `[table 1 row 2] a | b`.
    pub fn numbered_text(&self) -> String {
        self.entries()
            .into_iter()
            .filter(|e| !e.text.is_empty())
            .map(|e| match e.place {
                Place::Paragraph(n) => format!("[¶{n}] {}", e.text),
                Place::Row(t, r) => format!("[table {t} row {r}] {}", e.text),
                Place::Other => e.text,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn paragraph(&self, n: u32) -> Result<Span, String> {
        self.entries()
            .into_iter()
            .find(|e| e.place == Place::Paragraph(n))
            .map(|e| e.span)
            .ok_or_else(|| format!("There is no paragraph {n}. Read the document again for the numbers."))
    }

    /// Works out the new XML and what the change looks like, without writing.
    pub fn plan(&self, edits: &Edits) -> Result<Plan, String> {
        let entries = self.entries();
        let mut splices: Vec<(Span, String)> = Vec::new();
        // Display: per entry index, (mark, new text); and new blocks after an entry.
        let mut marks: Vec<Option<(Mark, Option<String>)>> = vec![None; entries.len()];
        let mut added: Vec<(Option<usize>, Vec<String>)> = Vec::new();

        for &n in &edits.delete_paragraphs {
            let i = entries
                .iter()
                .position(|e| e.place == Place::Paragraph(n))
                .ok_or_else(|| format!("There is no paragraph {n}. Read the document again for the numbers."))?;
            splices.push((entries[i].span, String::new()));
            marks[i] = Some((Mark::Removed, None));
        }

        let mut tables: Vec<u32> = edits.delete_rows.iter().map(|(t, _)| *t).collect();
        tables.sort_unstable();
        tables.dedup();
        for t in tables {
            let rows: Vec<u32> = edits.delete_rows.iter().filter(|(x, _)| *x == t).map(|(_, r)| *r).collect();
            let table_rows: Vec<usize> =
                entries.iter().enumerate().filter(|(_, e)| matches!(e.place, Place::Row(x, _) if x == t)).map(|(i, _)| i).collect();
            if table_rows.is_empty() {
                return Err(format!("There is no table {t}."));
            }
            for &r in &rows {
                if r == 0 || r as usize > table_rows.len() {
                    return Err(format!("Table {t} has {} rows.", table_rows.len()));
                }
            }
            let all_gone = (1..=table_rows.len() as u32).all(|r| rows.contains(&r));
            if all_gone {
                // A table with no rows is not a valid document: remove the table.
                let span = self
                    .items
                    .iter()
                    .filter_map(|it| if let Item::Table { span, .. } = it { Some(*span) } else { None })
                    .nth(t as usize - 1)
                    .ok_or_else(|| format!("There is no table {t}."))?;
                splices.push((span, String::new()));
            }
            for &r in &rows {
                let i = table_rows[r as usize - 1];
                if !all_gone {
                    splices.push((entries[i].span, String::new()));
                }
                marks[i] = Some((Mark::Removed, None));
                }
        }

        for (only, find, replace) in &edits.replace {
            if find.is_empty() {
                return Err("Say which text to replace.".into());
            }
            let found: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.place != Place::Other)
                .filter(|(_, e)| only.is_none_or(|n| e.place == Place::Paragraph(n)))
                .filter(|(_, e)| run_text(&self.xml, e.span).contains(find.as_str()))
                .map(|(i, _)| i)
                .collect();
            let i = match found.as_slice() {
                [i] => *i,
                [] => return Err(format!("“{}” is not in the document. Copy the exact text from read_file.", extract::clip(find, 60))),
                _ => return Err(format!("“{}” is in more than one place; say which paragraph.", extract::clip(find, 60))),
            };
            let xml = replace_in(&self.xml, entries[i].span, find, replace)?;
            splices.push((entries[i].span, xml));
            let new_text = entries[i].text.replacen(find.as_str(), replace, 1);
            marks[i] = Some((Mark::Changed, Some(new_text)));
        }

        for (after, lines) in &edits.insert {
            let lines: Vec<String> = lines.iter().map(|l| l.trim_end().to_string()).collect();
            if lines.is_empty() {
                continue;
            }
            let (at, model, index) = if *after == 0 {
                let first = self.paragraph(1)?;
                (first.start, first, None)
            } else {
                let span = self.paragraph(*after)?;
                let i = entries.iter().position(|e| e.place == Place::Paragraph(*after));
                (span.end, span, i)
            };
            let xml: String = lines.iter().map(|l| paragraph_like(&self.xml, model, l)).collect();
            splices.push((Span { start: at, end: at }, xml));
            added.push((index, lines));
        }

        if splices.is_empty() {
            return Err("Say what to change: delete_paragraphs, delete_table_rows, replace or insert.".into());
        }
        let xml = apply(&self.xml, splices)?;
        Ok(Plan { xml, preview: self.preview(&entries, &marks, &added) })
    }

    fn preview(&self, entries: &[Entry], marks: &[Option<(Mark, Option<String>)>], added: &[(Option<usize>, Vec<String>)]) -> Preview {
        let label = |p: Place| match p {
            Place::Paragraph(n) => format!("¶{n}"),
            Place::Row(t, r) => format!("T{t}·{r}"),
            Place::Other => String::new(),
        };
        // Everything in order, new paragraphs after their anchor.
        let mut all: Vec<(Block, bool)> = Vec::new();
        for (_, lines) in added.iter().filter(|(i, _)| i.is_none()) {
            all.extend(lines.iter().map(|l| (Block { label: "+".into(), mark: Mark::Added, text: l.clone(), old: None }, true)));
        }
        for (i, e) in entries.iter().enumerate() {
            let block = match &marks[i] {
                Some((Mark::Changed, new)) => {
                    Block { label: label(e.place), mark: Mark::Changed, text: new.clone().unwrap_or_default(), old: Some(e.text.clone()) }
                }
                Some((mark, _)) => Block { label: label(e.place), mark: *mark, text: e.text.clone(), old: None },
                None => Block { label: label(e.place), mark: Mark::Same, text: e.text.clone(), old: None },
            };
            let touched = marks[i].is_some();
            if touched || !e.text.is_empty() {
                all.push((block, touched));
            }
            for (_, lines) in added.iter().filter(|(a, _)| *a == Some(i)) {
                all.extend(lines.iter().map(|l| (Block { label: "+".into(), mark: Mark::Added, text: l.clone(), old: None }, true)));
            }
        }
        // The changed blocks and one either side, a gap where text is left out.
        let keep: Vec<bool> = (0..all.len())
            .map(|i| (i.saturating_sub(1)..=(i + 1).min(all.len().saturating_sub(1))).any(|j| all[j].1))
            .collect();
        let mut blocks = Vec::new();
        let mut skipped = false;
        for (i, (block, _)) in all.into_iter().enumerate() {
            if !keep[i] {
                skipped = true;
                continue;
            }
            if skipped && !blocks.is_empty() {
                blocks.push(Block { label: String::new(), mark: Mark::Gap, text: String::new(), old: None });
            }
            skipped = false;
            blocks.push(block);
        }
        Preview::Doc { blocks }
    }
}

/// The text Word stores in a paragraph's runs, joined, which is what a
/// replacement has to match.
fn run_text(xml: &str, span: Span) -> String {
    texts(xml, span).into_iter().map(|t| t.text).collect()
}

struct Text {
    /// Where `<w:t` starts, and where its content starts and ends.
    tag: usize,
    from: usize,
    to: usize,
    text: String,
}

fn texts(xml: &str, span: Span) -> Vec<Text> {
    let mut out = Vec::new();
    let mut i = span.start;
    while let Some(p) = xml[i..span.end].find("<w:t") {
        let tag = i + p;
        let after = xml.as_bytes().get(tag + 4).copied().unwrap_or(b' ');
        let Some(gt) = xml[tag..span.end].find('>').map(|g| tag + g) else { break };
        i = gt + 1;
        // <w:tab>, <w:tbl>, <w:tc>... and an empty <w:t/> are not text.
        if !(after == b'>' || after == b' ') || xml[..gt].ends_with('/') {
            continue;
        }
        let Some(close) = xml[gt..span.end].find("</w:t>").map(|c| gt + c) else { break };
        out.push(Text { tag, from: gt + 1, to: close, text: extract::unescape_xml(&xml[gt + 1..close]) });
        i = close;
    }
    out
}

/// The span's XML with `find` replaced once. Text inside one run keeps that
/// run's formatting; text across runs takes the first run's.
fn replace_in(xml: &str, span: Span, find: &str, replace: &str) -> Result<String, String> {
    let pieces = texts(xml, span);
    let joined: String = pieces.iter().map(|t| t.text.as_str()).collect();
    match joined.matches(find).count() {
        1 => {}
        0 => return Err("That text is split in a way that can't be matched; replace the whole paragraph instead.".into()),
        n => return Err(format!("That text appears {n} times in the paragraph; include more around it.")),
    }
    let at = joined.find(find).unwrap();
    let end = at + find.len();
    // New content for each piece the match touches.
    let mut changes: Vec<(usize, String)> = Vec::new();
    let mut offset = 0;
    let mut first = true;
    for (k, piece) in pieces.iter().enumerate() {
        let (a, b) = (offset, offset + piece.text.len());
        offset = b;
        if b <= at || a >= end {
            continue;
        }
        let keep_before = &piece.text[..at.saturating_sub(a).min(piece.text.len())];
        let keep_after = if end < b { &piece.text[end - a..] } else { "" };
        let text = if first { format!("{keep_before}{replace}{keep_after}") } else { keep_after.to_string() };
        first = false;
        changes.push((k, text));
    }
    let mut out = xml[span.start..span.end].to_string();
    for (k, text) in changes.into_iter().rev() {
        let piece = &pieces[k];
        let (from, to, tag) = (piece.from - span.start, piece.to - span.start, piece.tag - span.start);
        out.replace_range(from..to, &escape(&text));
        let open = &out[tag..from];
        if !open.contains("xml:space") {
            out.replace_range(tag..tag + 4, "<w:t xml:space=\"preserve\"");
        }
    }
    Ok(out)
}

/// A new paragraph that looks like `model`: same paragraph style and list, and
/// the formatting of its first run.
fn paragraph_like(xml: &str, model: Span, line: &str) -> String {
    let body = &xml[model.start..model.end];
    let ppr = section(body, "<w:pPr>", "<w:pPr ", "</w:pPr>").filter(|p| !p.contains("<w:sectPr"));
    let rpr = body.find("<w:r>").or_else(|| body.find("<w:r ")).and_then(|r| section(&body[r..], "<w:rPr>", "<w:rPr ", "</w:rPr>"));
    format!(
        "<w:p>{}<w:r>{}<w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>",
        ppr.unwrap_or(""),
        rpr.unwrap_or(""),
        escape(line)
    )
}

fn section<'a>(text: &'a str, open: &str, open_attr: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open).or_else(|| text.find(open_attr))?;
    let end = start + text[start..].find(close)? + close.len();
    Some(&text[start..end])
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Applies the changes back to front, so each one's position still holds.
fn apply(xml: &str, mut splices: Vec<(Span, String)>) -> Result<String, String> {
    splices.sort_by_key(|(s, _)| (std::cmp::Reverse(s.start), std::cmp::Reverse(s.end)));
    let mut out = xml.to_string();
    let mut limit = usize::MAX;
    for (span, text) in splices {
        if span.end > limit {
            return Err("Two of the changes touch the same paragraph; make them one at a time.".into());
        }
        out.replace_range(span.start..span.end, &text);
        limit = span.start;
    }
    Ok(out)
}

/// Writes the document with its new body; every other part is copied as is.
pub fn write(path: &Path, xml: &str) -> Result<(), String> {
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    {
        let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        for i in 0..archive.len() {
            let mut part = archive.by_index(i).map_err(|e| e.to_string())?;
            let mut bytes = Vec::new();
            part.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            entries.push((part.name().to_string(), bytes));
        }
    }
    let temp = path.with_extension("docx.coucou-tmp");
    {
        let file = std::fs::File::create(&temp).map_err(|e| e.to_string())?;
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in &entries {
            writer.start_file(name.as_str(), options).map_err(|e| e.to_string())?;
            let bytes = if name == DOCUMENT { xml.as_bytes() } else { bytes.as_slice() };
            writer.write_all(bytes).map_err(|e| e.to_string())?;
        }
        writer.finish().map_err(|e| e.to_string())?;
    }
    std::fs::rename(&temp, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(body: &str) -> Document {
        parse(format!("<?xml version=\"1.0\"?><w:document><w:body>{body}<w:sectPr/></w:body></w:document>")).unwrap()
    }

    fn p(text: &str) -> String {
        format!("<w:p><w:pPr><w:pStyle w:val=\"Body\"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t>{text}</w:t></w:r></w:p>")
    }

    fn table(rows: &[&[&str]]) -> String {
        let rows: String = rows
            .iter()
            .map(|r| format!("<w:tr>{}</w:tr>", r.iter().map(|c| format!("<w:tc>{}</w:tc>", p(c))).collect::<String>()))
            .collect();
        format!("<w:tbl><w:tblPr/>{rows}</w:tbl>")
    }

    #[test]
    fn paragraphs_and_table_rows_are_numbered() {
        let d = doc(&format!("{}{}{}", p("Title"), table(&[&["Name", "Price"], &["Tea", "3"]]), p("End")));
        assert_eq!(d.numbered_text(), "[¶1] Title\n[table 1 row 1] Name | Price\n[table 1 row 2] Tea | 3\n[¶2] End");
    }

    #[test]
    fn deleting_a_paragraph_and_a_row_removes_only_those() {
        let d = doc(&format!("{}{}{}", p("Keep"), p("Drop"), table(&[&["A"], &["B"], &["C"]])));
        let plan = d.plan(&Edits { delete_paragraphs: vec![2], delete_rows: vec![(1, 2)], ..Default::default() }).unwrap();
        let after = parse(plan.xml).unwrap();
        assert_eq!(after.numbered_text(), "[¶1] Keep\n[table 1 row 1] A\n[table 1 row 2] C");
    }

    #[test]
    fn deleting_every_row_removes_the_table() {
        let d = doc(&format!("{}{}", p("x"), table(&[&["A"], &["B"]])));
        let plan = d.plan(&Edits { delete_rows: vec![(1, 1), (1, 2)], ..Default::default() }).unwrap();
        assert!(!plan.xml.contains("<w:tbl>"));
    }

    #[test]
    fn a_replacement_keeps_the_run_formatting() {
        let d = doc(&p("Price: 10 euros"));
        let plan = d.plan(&Edits { replace: vec![(None, "10".into(), "12 & up".into())], ..Default::default() }).unwrap();
        assert!(plan.xml.contains("<w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">Price: 12 &amp; up euros</w:t>"));
    }

    #[test]
    fn a_replacement_across_runs_joins_them() {
        let d = doc("<w:p><w:r><w:t>Hel</w:t></w:r><w:r><w:t xml:space=\"preserve\">lo world</w:t></w:r></w:p>");
        let plan = d.plan(&Edits { replace: vec![(None, "Hello".into(), "Bye".into())], ..Default::default() }).unwrap();
        assert_eq!(parse(plan.xml).unwrap().numbered_text(), "[¶1] Bye world");
    }

    #[test]
    fn inserted_paragraphs_copy_the_style_of_their_neighbour() {
        let d = doc(&format!("{}{}", p("One"), p("Three")));
        let plan = d.plan(&Edits { insert: vec![(1, vec!["Two".into()])], ..Default::default() }).unwrap();
        assert!(plan.xml.contains("<w:p><w:pPr><w:pStyle w:val=\"Body\"/></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">Two</w:t>"));
        assert_eq!(parse(plan.xml).unwrap().numbered_text(), "[¶1] One\n[¶2] Two\n[¶3] Three");
    }

    #[test]
    fn unknown_numbers_and_missing_text_are_explained() {
        let d = doc(&p("Only"));
        assert!(d.plan(&Edits { delete_paragraphs: vec![4], ..Default::default() }).is_err());
        assert!(d.plan(&Edits { replace: vec![(None, "nope".into(), "x".into())], ..Default::default() }).is_err());
        assert!(d.plan(&Edits::default()).is_err());
    }

    #[test]
    fn the_preview_marks_the_change_with_its_neighbours() {
        let body: String = (1..=10).map(|n| p(&format!("line {n}"))).collect();
        let plan = doc(&body).plan(&Edits { delete_paragraphs: vec![5], ..Default::default() }).unwrap();
        let Preview::Doc { blocks } = plan.preview else { panic!() };
        let labels: Vec<_> = blocks.iter().map(|b| (b.label.as_str(), b.mark)).collect();
        assert_eq!(labels, vec![("¶4", Mark::Same), ("¶5", Mark::Removed), ("¶6", Mark::Same)]);
    }
}
