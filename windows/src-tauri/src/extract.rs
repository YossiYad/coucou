// Turns files a model cannot read as they are into text it can: spreadsheets
// into tab-separated tables, Word / PowerPoint / LibreOffice documents into
// their words, and PDFs into their text for models that take no PDFs.

use std::io::Read;

use calamine::{Data, Reader};

/// One sheet, or one document, in text the model can read.
pub fn spreadsheet(path: &str) -> Option<String> {
    let mut workbook = calamine::open_workbook_auto(path).ok()?;
    let mut out = String::new();
    for name in workbook.sheet_names().to_owned() {
        let Ok(range) = workbook.worksheet_range(&name) else { continue };
        out.push_str(&format!("Sheet: {name}\n"));
        for row in range.rows() {
            let mut cells: Vec<String> = row.iter().map(cell_text).collect();
            while cells.last().is_some_and(|c| c.is_empty()) {
                cells.pop();
            }
            if cells.is_empty() {
                continue;
            }
            out.push_str(&cells.join("\t"));
            out.push('\n');
        }
        out.push('\n');
    }
    let out = out.trim_end().to_string();
    (!out.is_empty()).then_some(out)
}

/// Dates are stored as day counts since 1899; shown as dates, not numbers.
fn cell_text(cell: &Data) -> String {
    if let Data::DateTime(dt) = cell {
        if let Some(t) = dt.as_datetime() {
            let date = t.format("%Y-%m-%d").to_string();
            let time = t.format("%H:%M").to_string();
            return match (date.as_str(), time.as_str()) {
                (_, "00:00") => date,
                ("1899-12-30" | "1899-12-31", _) => time, // a time of day with no date
                _ => format!("{date} {time}"),
            };
        }
    }
    cell.to_string().replace(['\t', '\n', '\r'], " ")
}

/// Word (.docx), PowerPoint (.pptx) and LibreOffice (.odt, .odp) documents.
pub fn office(path: &str, ext: &str) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let text = match ext {
        "docx" => xml_text(&entry(&mut archive, "word/document.xml")?, XmlKind::Word),
        "pptx" => {
            let mut slides: Vec<(u32, String)> = archive
                .file_names()
                .filter_map(|n| {
                    let num = n.strip_prefix("ppt/slides/slide")?.strip_suffix(".xml")?.parse().ok()?;
                    Some((num, n.to_string()))
                })
                .collect();
            slides.sort();
            let mut out = String::new();
            for (num, name) in slides {
                let body = xml_text(&entry(&mut archive, &name)?, XmlKind::Slide);
                out.push_str(&format!("Slide {num}:\n{body}\n\n"));
            }
            out
        }
        "odt" | "odp" => xml_text(&entry(&mut archive, "content.xml")?, XmlKind::OpenDocument),
        _ => return None,
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn entry(archive: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<String> {
    let mut xml = String::new();
    archive.by_name(name).ok()?.read_to_string(&mut xml).ok()?;
    Some(xml)
}

#[derive(Clone, Copy)]
enum XmlKind {
    /// Text lives in <w:t>, paragraphs end with </w:p>.
    Word,
    /// Text lives in <a:t>, paragraphs end with </a:p>.
    Slide,
    /// Every text node inside <office:body> counts.
    OpenDocument,
}

/// The words of a document's XML, with paragraph breaks kept.
fn xml_text(xml: &str, kind: XmlKind) -> String {
    let xml = match kind {
        XmlKind::OpenDocument => xml.find("<office:body").map_or(xml, |i| &xml[i..]),
        _ => xml,
    };
    let text_tag = match kind {
        XmlKind::Word => Some("w:t"),
        XmlKind::Slide => Some("a:t"),
        XmlKind::OpenDocument => None,
    };
    let mut out = String::new();
    let mut inside = text_tag.is_none();
    let mut rest = xml;
    while let Some(open) = rest.find('<') {
        if inside {
            out.push_str(&unescape(&rest[..open]));
        }
        let Some(close) = rest[open..].find('>') else { break };
        let tag = &rest[open + 1..open + close];
        let name = tag.trim_start_matches('/').split([' ', '/']).next().unwrap_or("");
        let closing = tag.starts_with('/');
        let empty = tag.ends_with('/');
        if let Some(t) = text_tag {
            if name == t && !empty {
                inside = !closing;
            }
        }
        match (name, closing || empty) {
            ("w:p" | "a:p" | "text:p" | "text:h", true) | ("w:br" | "text:line-break", _) => out.push('\n'),
            ("w:tab" | "text:tab", _) => out.push('\t'),
            ("text:s", _) => out.push(' '),
            _ => {}
        }
        rest = &rest[open + close + 1..];
    }
    // Collapse the empty paragraphs documents are full of.
    let mut lines: Vec<&str> = Vec::new();
    for line in out.lines() {
        let blank = line.trim().is_empty();
        if blank && lines.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => out.push(c),
            None => out.push_str(&after[..=semi]),
        }
        rest = &after[semi + 1..];
    }
    out.push_str(rest);
    out
}

/// The text of a PDF, for models that cannot take the file itself. Scanned
/// PDFs have no text layer and come back None.
pub fn pdf_text(bytes: &[u8]) -> Option<String> {
    // The parser can panic on a malformed file: a bad PDF must not take the
    // island down with it.
    let text = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes).ok())
        .ok()
        .flatten()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// A PDF's text page by page, for reading long ones a page at a time.
pub fn pdf_pages(bytes: &[u8]) -> Option<Vec<String>> {
    let pages = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem_by_pages(bytes).ok())
        .ok()
        .flatten()?;
    pages.iter().any(|p| !p.trim().is_empty()).then_some(pages)
}

/// Long files are cut at a character boundary, saying so.
pub fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_string(),
        Some((cut, _)) => format!("{}\n[... cut here: the file is longer than shown]", &text[..cut]),
    }
}

/// Writes tab-separated rows as a spreadsheet, the same shape `spreadsheet`
/// reads: a line "Sheet: name" starts a sheet, numbers stay numbers.
pub fn write_xlsx(path: &std::path::Path, content: &str) -> Result<(), String> {
    let mut book = rust_xlsxwriter::Workbook::new();
    let mut sheets: Vec<(String, Vec<&str>)> = Vec::new();
    for line in content.lines() {
        match line.strip_prefix("Sheet:") {
            Some(name) => sheets.push((name.trim().to_string(), Vec::new())),
            None => {
                if sheets.is_empty() {
                    sheets.push(("Sheet1".into(), Vec::new()));
                }
                sheets.last_mut().unwrap().1.push(line);
            }
        }
    }
    for (name, rows) in sheets {
        let sheet = book.add_worksheet();
        if !name.is_empty() {
            // Excel limits sheet names to 31 characters and a few symbols.
            let safe: String = name.chars().filter(|c| !"[]:*?/\\".contains(*c)).take(31).collect();
            let _ = sheet.set_name(safe);
        }
        for (r, row) in rows.iter().enumerate() {
            for (c, cell) in row.split('\t').enumerate() {
                let (r, c) = (r as u32, c as u16);
                let cell = cell.trim();
                if cell.is_empty() {
                    continue;
                }
                // "007" is a code, not seven.
                let leading_zero = cell.len() > 1 && cell.starts_with('0') && !cell.starts_with("0.");
                let written = match cell.parse::<f64>() {
                    Ok(n) if !leading_zero => sheet.write_number(r, c, n).map(|_| ()),
                    _ => sheet.write_string(r, c, cell).map(|_| ()),
                };
                written.map_err(|e| e.to_string())?;
            }
        }
    }
    book.save(path).map_err(|e| format!("Could not save the spreadsheet: {e}"))
}

pub struct WebPage {
    pub title: String,
    pub text: String,
    pub links: Vec<(String, String)>,
}

/// The readable text of an HTML page, without scripts and styles, and its links
/// made absolute so they can be followed.
pub fn html_page(html: &str, base: &reqwest::Url) -> WebPage {
    let lower = html.to_lowercase();
    let title = between(html, &lower, "<title", "</title>")
        .map(|t| unescape(t.split_once('>').map_or(t, |(_, rest)| rest).trim()))
        .unwrap_or_default();
    let mut text = String::new();
    let mut links = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut pos = 0;
    let mut link_href: Option<String> = None;
    let mut link_text = String::new();
    while let Some(rel) = html[pos..].find('<') {
        let open = pos + rel;
        let chunk = unescape(&html[pos..open]);
        text.push_str(&chunk);
        if link_href.is_some() {
            link_text.push_str(&chunk);
        }
        let Some(close_rel) = html[open..].find('>') else { break };
        let tag = &html[open + 1..open + close_rel];
        let tag_lower = tag.to_lowercase();
        let name = tag_lower.trim_start_matches('/').split([' ', '\n', '\t', '/']).next().unwrap_or("");
        pos = open + close_rel + 1;
        // Skip the contents of tags that are not text at all.
        if !tag_lower.starts_with('/') && matches!(name, "script" | "style" | "noscript" | "svg" | "template" | "head") {
            let end = format!("</{name}");
            match lower[pos..].find(&end) {
                Some(e) => pos += e,
                None => break,
            }
            continue;
        }
        match name {
            "a" if !tag_lower.starts_with('/') => {
                link_href = attribute(tag, "href");
                link_text.clear();
            }
            "a" => {
                if let Some(href) = link_href.take() {
                    let label = link_text.split_whitespace().collect::<Vec<_>>().join(" ");
                    if let Ok(url) = base.join(&href) {
                        if matches!(url.scheme(), "http" | "https") && !label.is_empty() && seen.insert(url.to_string()) {
                            links.push((label, url.to_string()));
                        }
                    }
                }
            }
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" | "article"
            | "header" | "footer" | "table" | "ul" | "ol" | "blockquote" | "pre" => text.push('\n'),
            "td" | "th" => text.push('\t'),
            _ => {}
        }
    }
    // Whitespace as a reader sees it: one space inside lines, no blank runs.
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() && lines.last().is_none_or(|l: &String| l.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    WebPage { title, text: lines.join("\n").trim().to_string(), links }
}

fn between<'a>(text: &'a str, lower: &str, start: &str, end: &str) -> Option<&'a str> {
    let s = lower.find(start)?;
    let e = lower[s..].find(end)? + s;
    Some(&text[s + start.len()..e])
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let at = lower.find(&format!("{name}="))? + name.len() + 1;
    let rest = &tag[at..];
    let value = match rest.chars().next()? {
        q @ ('"' | '\'') => rest[1..].split(q).next()?,
        _ => rest.split([' ', '>']).next()?,
    };
    Some(unescape(value))
}

/// Results from DuckDuckGo's plain HTML page: title, link and snippet.
pub fn duckduckgo_results(html: &str) -> Vec<(String, String, String)> {
    let mut results = Vec::new();
    for block in html.split("class=\"result__a\"").skip(1) {
        let Some(href) = attribute(&format!("x {}", block.split('>').next().unwrap_or("")), "href") else { continue };
        let title_html = block.split_once('>').map(|(_, r)| r).unwrap_or("").split("</a>").next().unwrap_or("");
        let title = strip_tags(title_html);
        let snippet = block
            .split("class=\"result__snippet\"")
            .nth(1)
            .and_then(|s| s.split_once('>'))
            .map(|(_, r)| strip_tags(r.split("</a>").next().unwrap_or("")))
            .unwrap_or_default();
        // DuckDuckGo wraps results as //duckduckgo.com/l/?uddg=<real link>.
        let link = href
            .split("uddg=")
            .nth(1)
            .map(|enc| enc.split('&').next().unwrap_or(enc))
            .map(percent_decode)
            .unwrap_or(href);
        if link.starts_with("http") && !title.is_empty() {
            results.push((title, link, snippet));
        }
    }
    results
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in html.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    unescape(&out).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_text_keeps_paragraphs_and_skips_markup() {
        let xml = r#"<w:body><w:p><w:r><w:t>שלום</w:t></w:r><w:r><w:t xml:space="preserve"> עולם</w:t></w:r></w:p>
            <w:p/><w:p><w:r><w:instrText>PAGE</w:instrText><w:t>A &amp; B</w:t><w:tab/><w:t>C</w:t></w:r></w:p></w:body>"#;
        // The empty paragraph stays as one blank line; runs of them would collapse.
        assert_eq!(xml_text(xml, XmlKind::Word), "שלום עולם\n\nA & B\tC");
    }

    #[test]
    fn open_document_text_ignores_styles_before_the_body() {
        let xml = r#"<office:automatic-styles><style:style>junk</style:style></office:automatic-styles>
            <office:body><text:h>Title</text:h><text:p>One<text:s/>two</text:p></office:body>"#;
        assert_eq!(xml_text(xml, XmlKind::OpenDocument).trim(), "Title\nOne two");
    }

    #[test]
    fn entities_decode_including_numeric_ones() {
        assert_eq!(unescape("a &lt;b&gt; &#1513; &#x5DC; &unknown;"), "a <b> ש ל &unknown;");
    }

    #[test]
    fn a_web_page_reads_as_text_with_absolute_links() {
        let base = reqwest::Url::parse("https://example.com/news/today").unwrap();
        let html = r#"<html><head><title>Daily &amp; News</title><style>p{}</style></head>
            <body><script>var x = "<p>no</p>";</script><h1>Headline</h1><p>First <b>para</b>.</p>
            <a href="/about">About us</a> <a href="story.html">Read more</a> <a href="mailto:a@b.c">Mail</a></body></html>"#;
        let page = html_page(html, &base);
        assert_eq!(page.title, "Daily & News");
        let lines: Vec<&str> = page.text.lines().collect();
        assert!(lines.contains(&"Headline") && lines.contains(&"First para."), "{}", page.text);
        assert!(!page.text.contains("var x"));
        assert_eq!(page.links, vec![
            ("About us".to_string(), "https://example.com/about".to_string()),
            ("Read more".to_string(), "https://example.com/news/story.html".to_string()),
        ]);
    }

    #[test]
    fn duckduckgo_links_are_unwrapped() {
        let html = r#"<a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust-lang.org%2F&amp;rut=x">The <b>Rust</b> Language</a>
            <a class="result__snippet" href="x">A language <b>empowering</b> everyone.</a>"#;
        let results = duckduckgo_results(html);
        assert_eq!(results, vec![(
            "The Rust Language".to_string(),
            "https://www.rust-lang.org/".to_string(),
            "A language empowering everyone.".to_string(),
        )]);
    }

    #[test]
    fn a_spreadsheet_round_trips_through_write_and_read() {
        let path = std::env::temp_dir().join(format!("coucou-xlsx-{}.xlsx", std::process::id()));
        write_xlsx(&path, "Sheet: מנויים\nשירות\tמחיר\nנטפליקס\t49.9\nקוד\t007").unwrap();
        let back = spreadsheet(path.to_str().unwrap()).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(back, "Sheet: מנויים\nשירות\tמחיר\nנטפליקס\t49.9\nקוד\t007");
    }

    #[test]
    fn clipping_counts_characters_not_bytes() {
        assert_eq!(clip("אבגד", 10), "אבגד");
        assert!(clip("אבגד", 2).starts_with("אב\n[... cut here"));
    }
}

