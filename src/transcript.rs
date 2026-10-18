//! Reading an agent's own transcript of a session (SPEC.md `TRA-2`, SPEC_impl.md "Tools", `session_transcript`), and
//! the directory search the agent adapters use to find transcripts and other per-session files.
//!
//! Both agents keep a transcript as a JSONL file, one JSON value per line, so the reader is agent-neutral: the
//! adapters only say where the file is. A read returns a page of entries, each the line's own JSON with long strings
//! cut, under fixed bounds (`BND-1`). Everything here is plain blocking file IO; callers run it on a blocking thread.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead as _, BufReader, Read as _, Seek as _, SeekFrom};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use rmcp::schemars;
use serde::Serialize;
use serde_json::Value;

use crate::events::{Keep, truncate};

/// Most entries one read returns.
pub const MAX_LIMIT: usize = 50;

/// How many entries a read returns when the caller does not say.
pub const DEFAULT_LIMIT: usize = 20;

/// Strings inside an entry longer than this are cut to it and marked.
const MAX_STRING_BYTES: usize = 4 * 1024;

/// An entry still larger than this once serialized is replaced by its serialized JSON, cut to this size.
const MAX_ENTRY_BYTES: usize = 64 * 1024;

/// A read stops adding entries once they total this much, so a response stays bounded whatever the entries hold.
const MAX_PAGE_BYTES: usize = 256 * 1024;

/// Transcripts larger than this are refused rather than scanned.
const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// Lines longer than this are not parsed: only their start is read, so one enormous line cannot pull a huge buffer
/// into memory. Their entry is the start of the line as a string, marked as truncated.
const MAX_PARSED_LINE_BYTES: u64 = 4 * 1024 * 1024;

/// Ends a string that was cut.
const CUT_MARK: &str = " [cut]";

/// One line of a transcript, as `session_transcript` returns it.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Entry {
    /// The entry's position in the transcript, counting from 0.
    pub index: u64,
    /// The agent's own record for this line, as JSON: unchanged except that strings longer than 4 KiB are cut and end
    /// in ` [cut]`. When `truncated` is true, this is instead the start of the line's text, as a string.
    pub entry: Value,
    /// True when the entry was too large to return as JSON even after cutting its strings (or was not valid JSON and
    /// too long), so `entry` holds only the start of the line's text. Absent when false.
    // Left out when false to keep pages small. `default` is what makes schemars list the field as optional; without
    // it the result schema requires a field the JSON omits, and clients that validate results against the schema
    // (Claude Code does) reject every page.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// A page of a transcript, as `session_transcript` returns it.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct Page {
    /// The entries read, in ascending index order. Without `from` they are the last entries of the transcript.
    pub entries: Vec<Entry>,
    /// How many complete entries the transcript had when the read started.
    pub total: u64,
    /// The index to pass as `from` to continue forwards, when entries follow the last one returned; null otherwise.
    /// To read further back, pass `from` as the first index you already have minus `limit` (never below 0), and follow
    /// `next_from` from there until you reach entries you already have, since a page can hold fewer than `limit`.
    pub next_from: Option<u64>,
}

/// Finds files and directories under `root` whose name satisfies `matches`: directly in `root`, or anywhere below it
/// when `recursive`. Symbolic links are listed but never followed.
///
/// A missing `root` is a completed search with nothing found, since an agent that never ran has no data directory. Any
/// other error, anywhere in the walk, fails the whole search: callers must be able to tell "found none" from "could
/// not look", because a removal or a resume decision based on a partial search would be wrong.
pub fn find(
    root: &Path,
    recursive: bool,
    matches: &dyn Fn(&str) -> bool,
) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir == root => return Ok(found),
            Err(e) => return Err(e),
        };
        for entry in entries {
            let entry = entry?;
            // Linux names need not be UTF-8. Matching on a lossy conversion keeps such a name visible: the IDs callers
            // look for are ASCII, so they survive it, and skipping the name would let a search miss a file it was
            // asked to find (a removal would then report data deleted that is still there).
            if matches(&entry.file_name().to_string_lossy()) {
                found.push(entry.path());
            }
            if recursive && entry.file_type()?.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Reads a page of the transcript at `path` (SPEC_impl.md, "Tools").
///
/// Without `from`, the page fills from the newest entry backwards, so the end is always included; with it, the page
/// fills forwards from that entry. Either way the entries come back in ascending order, at most `limit` of them and
/// at most [`MAX_PAGE_BYTES`] in total, but always at least one when any exist from that point on. The file is read as
/// it was when the read started (its length is taken first), and a last line without its newline is left out, since
/// the agent may still be writing it.
///
/// Errors are worded for the calling model. A transcript that cannot be opened is an error, never an empty page.
///
/// The file is opened without blocking and must be a regular file: a FIFO or device found where the transcript should
/// be (a symbolic link to one, say) would otherwise block the open, or the reads, with no bound, and its length says
/// nothing. Checking the opened descriptor rather than the path leaves no window for the path to be swapped.
pub fn read(path: &Path, from: Option<u64>, limit: usize) -> Result<Page, String> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("cannot open the transcript {}: {e}", path.display()))?;
    let meta = file
        .metadata()
        .map_err(|e| format!("cannot read the transcript {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file, so devbox-mcp does not read it as the transcript",
            path.display()
        ));
    }
    let len = meta.len();
    if len > MAX_FILE_BYTES {
        return Err(format!(
            "the transcript {} is {len} bytes, more than the 1 GiB devbox-mcp reads; ask the agent about it instead",
            path.display()
        ));
    }
    let (total, spans) = scan_lines(&mut file, len, from, limit)
        .map_err(|e| format!("cannot read the transcript {}: {e}", path.display()))?;

    let mut entries = Vec::new();
    let mut used = 0;
    let mut take = |span: &Span, entries: &mut Vec<Entry>| -> Result<bool, String> {
        let line = read_line(&mut file, span.start, span.end)
            .map_err(|e| format!("cannot read the transcript {}: {e}", path.display()))?;
        let entry = make_entry(span.index, line);
        let size = serde_json::to_string(&entry).map_or(0, |s| s.len());
        if !entries.is_empty() && used + size > MAX_PAGE_BYTES {
            return Ok(false);
        }
        used += size;
        entries.push(entry);
        Ok(true)
    };
    // The scan kept at most `limit` spans, already the ones this page may return.
    match from {
        None => {
            for span in spans.iter().rev() {
                if !take(span, &mut entries)? {
                    break;
                }
            }
            entries.reverse();
        }
        Some(_) => {
            for span in &spans {
                if !take(span, &mut entries)? {
                    break;
                }
            }
        }
    }
    let next_from = entries
        .last()
        .map(|e| e.index + 1)
        .filter(|&next| next < total && from.is_some());
    Ok(Page {
        entries,
        total,
        next_from,
    })
}

/// Where one complete line of a transcript is: bytes `start..end` of the file, without its newline.
struct Span {
    index: u64,
    start: u64,
    end: u64,
}

/// Counts the complete lines in the first `len` bytes of `file` and returns that count with the spans of the lines a
/// page may return: the last `limit` lines without `from`, or lines `from..from + limit` with it. A trailing partial
/// line is not counted.
///
/// Only those spans are kept, never one per line: a 1 GiB file of short lines has hundreds of millions of them, and an
/// index of them all would take gigabytes. The memory used is bounded by `limit` whatever the file holds.
fn scan_lines(
    file: &mut File,
    len: u64,
    from: Option<u64>,
    limit: usize,
) -> std::io::Result<(u64, VecDeque<Span>)> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::with_capacity(1 << 20, (&*file).take(len));
    let mut spans = VecDeque::with_capacity(limit + 1);
    let mut total = 0u64;
    let mut start = 0u64;
    let mut offset = 0u64;
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let n = buf.len();
        for (i, &b) in buf.iter().enumerate() {
            if b != b'\n' {
                continue;
            }
            let end = offset + i as u64;
            let wanted = match from {
                None => true,
                Some(from) => total >= from && total - from < limit as u64,
            };
            if wanted {
                spans.push_back(Span {
                    index: total,
                    start,
                    end,
                });
                if spans.len() > limit {
                    spans.pop_front();
                }
            }
            total += 1;
            start = end + 1;
        }
        offset += n as u64;
        reader.consume(n);
    }
    Ok((total, spans))
}

/// Reads the bytes `start..end` of `file`, or only the first [`MAX_ENTRY_BYTES`] of them when the line is too long
/// to parse (see [`MAX_PARSED_LINE_BYTES`]). Returns the bytes and whether they are the whole line.
fn read_line(file: &mut File, start: u64, end: u64) -> std::io::Result<(Vec<u8>, bool)> {
    let whole = end - start <= MAX_PARSED_LINE_BYTES;
    let len = if whole {
        end - start
    } else {
        MAX_ENTRY_BYTES as u64
    };
    let mut bytes = vec![0; len as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut bytes)?;
    Ok((bytes, whole))
}

/// Turns one line into an entry: its JSON with long strings cut, or, when that is still too large, not JSON, or too
/// long to parse, the start of its text as a string.
fn make_entry(index: u64, (bytes, whole): (Vec<u8>, bool)) -> Entry {
    if whole && let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) {
        cut_strings(&mut value);
        if serde_json::to_string(&value).is_ok_and(|s| s.len() <= MAX_ENTRY_BYTES) {
            return Entry {
                index,
                entry: value,
                truncated: false,
            };
        }
        let text = serde_json::to_string(&value).unwrap_or_default();
        return text_entry(index, &text, true);
    }
    let text = String::from_utf8_lossy(&bytes);
    text_entry(index, &text, !whole)
}

/// An entry holding `text` as a string whose JSON form, quotes and escapes included, is at most [`MAX_ENTRY_BYTES`].
/// `truncated` is set when it was cut here or `cut` says it already was, and then the string ends in [`CUT_MARK`].
///
/// The bound is on the serialized form because that is what the response carries: escaping can make a string up to
/// six times its raw size (a control character becomes `\u0000`), so cutting the raw text would let one entry exceed
/// the entry bound and, since a page always holds at least one entry, the page bound too.
fn text_entry(index: u64, text: &str, cut: bool) -> Entry {
    let room = MAX_ENTRY_BYTES - 2;
    let (text, truncated) = if !cut && json_len(text) <= room {
        (text.to_owned(), false)
    } else {
        let mut budget = room - CUT_MARK.len();
        let end = text
            .char_indices()
            .find(|&(_, c)| match budget.checked_sub(json_char_len(c)) {
                Some(left) => {
                    budget = left;
                    false
                }
                None => true,
            })
            .map_or(text.len(), |(i, _)| i);
        (format!("{}{CUT_MARK}", &text[..end]), true)
    };
    Entry {
        index,
        entry: Value::String(text),
        truncated,
    }
}

/// The length of `text` inside a JSON string as serde_json writes it, without the quotes.
fn json_len(text: &str) -> usize {
    text.chars().map(json_char_len).sum()
}

/// The length of one character inside a JSON string as serde_json writes it: two bytes for the short escapes, six
/// for other control characters (`\u001f`), and its UTF-8 length otherwise.
fn json_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Cuts every string in `value` longer than [`MAX_STRING_BYTES`], keeping its start and marking it.
fn cut_strings(value: &mut Value) {
    match value {
        Value::String(s) if s.len() > MAX_STRING_BYTES => {
            let (mut cut, _) = truncate(s, MAX_STRING_BYTES, Keep::Head);
            cut.push_str(CUT_MARK);
            *s = cut;
        }
        Value::Array(items) => items.iter_mut().for_each(cut_strings),
        Value::Object(map) => map.values_mut().for_each(cut_strings),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    /// A transcript file with the given lines, each followed by a newline, plus `tail` written without one.
    fn transcript(lines: &[String], tail: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        write!(file, "{tail}").unwrap();
        file
    }

    fn numbered(n: u64) -> Vec<String> {
        (0..n).map(|i| format!(r#"{{"n":{i}}}"#)).collect()
    }

    fn indexes(page: &Page) -> Vec<u64> {
        page.entries.iter().map(|e| e.index).collect()
    }

    /// Without `from`, a read returns the last entries, ascending, so the end is always there; with `from`, it pages
    /// forwards and says where to continue.
    #[test]
    fn pages_from_the_end_and_forwards() {
        let file = transcript(&numbered(30), "");
        let tail = read(file.path(), None, 5).unwrap();
        assert_eq!(indexes(&tail), [25, 26, 27, 28, 29]);
        assert_eq!(tail.total, 30);
        assert_eq!(tail.next_from, None);
        assert_eq!(tail.entries[0].entry, serde_json::json!({ "n": 25 }));

        let forward = read(file.path(), Some(10), 5).unwrap();
        assert_eq!(indexes(&forward), [10, 11, 12, 13, 14]);
        assert_eq!(forward.next_from, Some(15));
        let last = read(file.path(), Some(28), 5).unwrap();
        assert_eq!(indexes(&last), [28, 29]);
        assert_eq!(last.next_from, None);
        assert!(read(file.path(), Some(30), 5).unwrap().entries.is_empty());
    }

    /// A last line without its newline is being written by the agent and is left out.
    #[test]
    fn leaves_out_a_partial_last_line() {
        let file = transcript(&numbered(3), r#"{"n":3"#);
        let page = read(file.path(), None, 50).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(indexes(&page), [0, 1, 2]);
    }

    /// Long strings are cut and marked inside an otherwise intact JSON entry; an entry still too large becomes a cut
    /// string with `truncated` set; a line that is not JSON comes back as a string.
    #[test]
    fn bounds_each_entry() {
        let long = "x".repeat(10_000);
        let huge = serde_json::to_string(&vec![1; 40_000]).unwrap();
        let file = transcript(
            &[
                format!(r#"{{"text":"{long}","k":1}}"#),
                huge,
                "not json".to_owned(),
            ],
            "",
        );
        let page = read(file.path(), Some(0), 50).unwrap();
        let text = page.entries[0].entry["text"].as_str().unwrap();
        assert!(text.ends_with(CUT_MARK) && text.len() == MAX_STRING_BYTES + CUT_MARK.len());
        assert_eq!(page.entries[0].entry["k"], 1);
        assert!(!page.entries[0].truncated);
        assert!(page.entries[1].truncated);
        assert!(page.entries[1].entry.as_str().unwrap().ends_with(CUT_MARK));
        assert_eq!(page.entries[2].entry, Value::String("not json".to_owned()));
        assert!(!page.entries[2].truncated);
        for entry in &page.entries {
            assert!(serde_json::to_string(&entry.entry).unwrap().len() <= MAX_ENTRY_BYTES);
        }
    }

    /// The entry bound holds for the serialized entry, escapes included: a line of control characters (each six bytes
    /// once escaped) or of quotes inside a too-large JSON value must not come back several times over the bound, which
    /// would also break the page bound, since a page always holds at least one entry.
    #[test]
    fn bounds_entries_after_escaping() {
        let controls = "\u{1}".repeat(70_000);
        let quotes = serde_json::to_string(&vec!["\"\"\""; 20_000]).unwrap();
        let file = transcript(&[controls, quotes], "");
        let page = read(file.path(), Some(0), 50).unwrap();
        assert_eq!(page.entries.len(), 2);
        for entry in &page.entries {
            assert!(entry.truncated);
            let size = serde_json::to_string(&entry.entry).unwrap().len();
            assert!(size <= MAX_ENTRY_BYTES, "{size}");
            assert!(
                size > MAX_ENTRY_BYTES - 6,
                "cut much shorter than needed: {size}"
            );
            assert!(entry.entry.as_str().unwrap().ends_with(CUT_MARK));
        }
    }

    /// Something other than a regular file where the transcript should be is refused at once: a FIFO would otherwise
    /// block the read until some process opened it for writing, holding up every later tool call.
    #[test]
    fn refuses_a_fifo() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("t.jsonl");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let error = read(&path, None, 5).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
    }

    /// Paging works the same in a file of many short lines, where only the page's lines are kept while scanning.
    #[test]
    fn pages_through_many_short_lines() {
        let lines: Vec<String> = (0..100_000).map(|i| i.to_string()).collect();
        let file = transcript(&lines, "");
        let tail = read(file.path(), None, 3).unwrap();
        assert_eq!(tail.total, 100_000);
        assert_eq!(indexes(&tail), [99_997, 99_998, 99_999]);
        assert_eq!(tail.entries[2].entry, serde_json::json!(99_999));
        let middle = read(file.path(), Some(50_000), 2).unwrap();
        assert_eq!(indexes(&middle), [50_000, 50_001]);
        assert_eq!(middle.entries[0].entry, serde_json::json!(50_000));
        assert_eq!(middle.next_from, Some(50_002));
    }

    /// A page stops at its byte budget, newest first when reading from the end, but always returns at least one entry,
    /// so paging never stalls.
    #[test]
    fn stops_at_the_page_budget() {
        let big: Vec<String> = (0..20)
            .map(|i| {
                let parts: Vec<String> = (0..12).map(|_| "y".repeat(4000)).collect();
                serde_json::to_string(&serde_json::json!({ "n": i, "parts": parts })).unwrap()
            })
            .collect();
        let file = transcript(&big, "");
        let tail = read(file.path(), None, 50).unwrap();
        assert!(tail.entries.len() < 20);
        assert_eq!(tail.entries.last().unwrap().index, 19);
        let forward = read(file.path(), Some(0), 50).unwrap();
        assert_eq!(forward.entries[0].index, 0);
        assert_eq!(forward.next_from, Some(forward.entries.len() as u64));
    }

    /// A transcript that cannot be opened is an error, never an empty page.
    #[test]
    fn a_missing_file_is_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read(&dir.path().join("nope.jsonl"), None, 5).is_err());
    }

    /// The search tells "found none" (including a missing root) from "could not look", and finds names at any depth
    /// when recursive.
    #[test]
    fn find_searches_by_name() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/x-id1.jsonl"), "").unwrap();
        std::fs::write(dir.path().join("y-id1.jsonl"), "").unwrap();
        let ends = |n: &str| n.ends_with("-id1.jsonl");
        assert_eq!(find(dir.path(), true, &ends).unwrap().len(), 2);
        // A name that is not valid UTF-8 but contains the ID is still found.
        use std::os::unix::ffi::OsStrExt as _;
        let odd = std::ffi::OsStr::from_bytes(b"z-id1-\xff");
        std::fs::write(dir.path().join("a").join(odd), "").unwrap();
        assert_eq!(
            find(dir.path(), true, &|n: &str| n.contains("id1"))
                .unwrap()
                .len(),
            3
        );
        assert_eq!(find(dir.path(), false, &ends).unwrap().len(), 1);
        assert!(
            find(&dir.path().join("missing"), true, &ends)
                .unwrap()
                .is_empty()
        );
    }
}
