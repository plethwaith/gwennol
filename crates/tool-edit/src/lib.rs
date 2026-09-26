//! The bundled `edit` tool's guest code: exact-string replacement.
//!
//! Two halves of one plugin. The manifest, `plugins/tools/edit.json`,
//! owns everything that reaches outside the sandbox — a `host_fs.read`
//! of the whole file, then a `host_fs.write` of it when this crate says
//! to write, each branched on as data. This crate owns the one thing
//! that needs code — deciding what, if anything, to write — and it
//! reaches nothing itself: no `invoke`, no stream, no host call of any
//! kind. [`decide`] is pure and unit-tested on the host target; the
//! crate's one entry point is the glue that reaches it from the
//! manifest's resolution context.
//!
//! The manifest does the read before this runs and the write after, so
//! a call is approved as the read and the write of one path — an
//! access a person or a rule already knows how to judge — rather than
//! as an opaque script step.

use gwennol_guest::{Args, entrypoints};
use serde_json::{Value, json};

/// The plugin this module ships inside — its manifest `name` and the
/// `language` selector of its one script step.
pub const PLUGIN_NAME: &str = "tool-edit";

/// Entry-point name the manifest's `decide` step selects via `source`.
pub const ENTRY_REPLACE: &str = "replace";

/// The step id of the manifest's `host_fs.read`, whose result `replace`
/// reads.
pub const READ_STEP: &str = "read";

/// The most of a file `edit` reads; the manifest's `max_bytes`, pinned
/// equal.
pub const READ_MAX_BYTES: u64 = 4 << 20;

/// What [`decide`] settled: write `content` and report `message`, or
/// refuse and report `message` — the text the manifest hands the model
/// either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The read was usable and `old_string` occurred exactly the number
    /// of times asked for: `content` is the file's new text.
    Write { content: String, message: String },
    /// Nothing is written; `message` says why.
    Refuse(String),
}

/// Decide what an `edit` call does to a file already read as `read` (the
/// manifest's `host_fs.read` result), replacing `old` with `new`.
///
/// In order: an empty `old` refuses (a `write` creates text, `edit`
/// replaces it); `old == new` refuses as nothing to change; a read that
/// did not come back `ok` refuses with its own message, verbatim; a
/// `truncated` read refuses — writing a prefix back would cut the file;
/// a `lossy` read refuses — writing it back would replace bytes that are
/// not UTF-8 with bytes that are. Then the occurrences of `old` are
/// counted: without `replace_all`, every position it starts at,
/// overlapping ones included (`"aa"` in `"aaa"` is two); with it, one
/// left-to-right non-overlapping pass. Zero refuses either way; more
/// than one without `replace_all` refuses naming the count.
///
/// `Err` only for a context that is not the manifest's own: `read`
/// missing a field this depends on (`outcome`, or — once it is `ok` —
/// `content`, `truncated` or `lossy`), each named. Malformed like this,
/// nothing this function does is a step the manifest could have
/// produced, so the caller fails the step rather than answer as data.
pub fn decide(
    path: &str,
    read: &Value,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<Decision, String> {
    if old.is_empty() {
        return Ok(Decision::Refuse(
            "old_string is empty; edit replaces text that is already in the file (write creates one)"
                .to_string(),
        ));
    }
    if old == new {
        return Ok(Decision::Refuse(
            "old_string and new_string are the same; there is nothing to change".to_string(),
        ));
    }
    let outcome = read
        .get("outcome")
        .and_then(Value::as_str)
        .ok_or_else(|| "the read step's result has no 'outcome' string".to_string())?;
    if outcome != "ok" {
        let message = read
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| "the read step's result has no 'message' string".to_string())?;
        return Ok(Decision::Refuse(message.to_string()));
    }
    let content = read
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "the read step's result has no 'content' string".to_string())?;
    let truncated = read
        .get("truncated")
        .and_then(Value::as_bool)
        .ok_or_else(|| "the read step's result has no 'truncated' boolean".to_string())?;
    let lossy = read
        .get("lossy")
        .and_then(Value::as_bool)
        .ok_or_else(|| "the read step's result has no 'lossy' boolean".to_string())?;
    if truncated {
        let size = read
            .get("size")
            .and_then(Value::as_u64)
            .map_or_else(|| "?".to_string(), |n| n.to_string());
        return Ok(Decision::Refuse(format!(
            "{path} is {size} bytes, more than the {READ_MAX_BYTES} edit reads whole; change it with write"
        )));
    }
    if lossy {
        return Ok(Decision::Refuse(format!(
            "{path} is not valid UTF-8 throughout, and edit would rewrite the bytes that are not; it changes UTF-8 text only"
        )));
    }
    let n = if replace_all {
        content.matches(old).count()
    } else {
        overlapping_count(content, old)
    };
    if n == 0 {
        return Ok(Decision::Refuse(format!(
            "old_string does not occur in {path}"
        )));
    }
    if !replace_all && n > 1 {
        return Ok(Decision::Refuse(format!(
            "old_string occurs {n} times in {path}; include more of the surrounding text to pick one, or set replace_all to change them all"
        )));
    }
    let new_content = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
    let message = if n == 1 {
        format!("replaced 1 occurrence in {path}")
    } else {
        format!("replaced {n} occurrences in {path}")
    };
    Ok(Decision::Write {
        content: new_content,
        message,
    })
}

/// Every position `needle` starts at in `haystack`, overlapping matches
/// counted separately: `"aa"` in `"aaa"` is two, at byte offsets 0 and 1.
fn overlapping_count(haystack: &str, needle: &str) -> usize {
    let mut count = 0;
    let mut idx = 0;
    while idx < haystack.len() {
        let Some(pos) = haystack[idx..].find(needle) else {
            break;
        };
        count += 1;
        let at = idx + pos;
        // Advance by one character from the match's start, not by the
        // needle's length, so an overlapping next match is still found.
        let step = haystack[at..].chars().next().map_or(1, char::len_utf8);
        idx = at + step;
    }
    count
}

/// The manifest's `decide` step: read `path`, `old_string`, `new_string`,
/// `replace_all` and the prior `host_fs.read` step's result from the
/// resolution context, and answer with [`decide`]'s result as the JSON
/// the manifest branches on (`{"write": bool, "content"?, "message"}`).
///
/// `Err` for a context that is not the manifest's own: a required field
/// missing or the wrong type, named.
fn replace(args: Args) -> Result<Value, String> {
    let path = args
        .field("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing string field 'path'".to_string())?;
    let old = args
        .field("old_string")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing string field 'old_string'".to_string())?;
    let new = args
        .field("new_string")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing string field 'new_string'".to_string())?;
    let replace_all = args
        .field("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let read = args
        .step_result(READ_STEP)
        .ok_or_else(|| format!("missing step result '{READ_STEP}'"))?;
    match decide(path, read, old, new, replace_all)? {
        Decision::Write { content, message } => Ok(json!({
            "write": true,
            "content": content,
            "message": message,
        })),
        Decision::Refuse(message) => Ok(json!({
            "write": false,
            "message": message,
        })),
    }
}

entrypoints! {
    ENTRY_REPLACE => replace,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_read(content: &str) -> Value {
        json!({"outcome": "ok", "content": content, "truncated": false, "lossy": false, "size": content.len()})
    }

    #[test]
    fn one_occurrence_is_replaced_and_the_rest_kept() {
        let content = "line one\nélan\nline three\n";
        let read = ok_read(content);
        let d = decide("p", &read, "élan", "vital", false).unwrap();
        assert_eq!(
            d,
            Decision::Write {
                content: "line one\nvital\nline three\n".to_string(),
                message: "replaced 1 occurrence in p".to_string(),
            }
        );
    }

    #[test]
    fn none_or_several_is_refused_and_replace_all_takes_each_once() {
        let zero = ok_read("nothing here");
        assert_eq!(
            decide("p", &zero, "zzz", "y", false).unwrap(),
            Decision::Refuse("old_string does not occur in p".to_string())
        );

        let two = ok_read("x y x");
        assert_eq!(
            decide("p", &two, "x", "z", false).unwrap(),
            Decision::Refuse(
                "old_string occurs 2 times in p; include more of the surrounding text to pick one, or set replace_all to change them all".to_string()
            )
        );

        let overlap = ok_read("aaa");
        assert_eq!(
            decide("p", &overlap, "aa", "b", false).unwrap(),
            Decision::Refuse(
                "old_string occurs 2 times in p; include more of the surrounding text to pick one, or set replace_all to change them all".to_string()
            )
        );

        assert_eq!(
            decide("p", &two, "x", "z", true).unwrap(),
            Decision::Write {
                content: "z y z".to_string(),
                message: "replaced 2 occurrences in p".to_string(),
            }
        );

        assert_eq!(
            decide("p", &zero, "zzz", "y", true).unwrap(),
            Decision::Refuse("old_string does not occur in p".to_string())
        );
    }

    #[test]
    fn an_empty_or_unchanged_replacement_is_refused() {
        let read = ok_read("anything");
        assert_eq!(
            decide("p", &read, "", "y", false).unwrap(),
            Decision::Refuse(
                "old_string is empty; edit replaces text that is already in the file (write creates one)".to_string()
            )
        );
        assert_eq!(
            decide("p", &read, "", "y", true).unwrap(),
            Decision::Refuse(
                "old_string is empty; edit replaces text that is already in the file (write creates one)".to_string()
            )
        );
        assert_eq!(
            decide("p", &read, "same", "same", false).unwrap(),
            Decision::Refuse(
                "old_string and new_string are the same; there is nothing to change".to_string()
            )
        );
    }

    #[test]
    fn replace_all_makes_one_pass() {
        let read = ok_read("a a");
        assert_eq!(
            decide("p", &read, "a", "aa", true).unwrap(),
            Decision::Write {
                content: "aa aa".to_string(),
                message: "replaced 2 occurrences in p".to_string(),
            }
        );
    }

    #[test]
    fn a_cut_lossy_or_missed_read_is_refused() {
        let mut cut = ok_read("x");
        cut["truncated"] = json!(true);
        cut["size"] = json!(READ_MAX_BYTES + 1);
        assert_eq!(
            decide("p", &cut, "x", "y", false).unwrap(),
            Decision::Refuse(format!(
                "p is {} bytes, more than the {READ_MAX_BYTES} edit reads whole; change it with write",
                READ_MAX_BYTES + 1
            ))
        );

        let mut lossy = ok_read("a\u{FFFD}b");
        lossy["lossy"] = json!(true);
        assert_eq!(
            decide("p", &lossy, "a", "z", false).unwrap(),
            Decision::Refuse(
                "p is not valid UTF-8 throughout, and edit would rewrite the bytes that are not; it changes UTF-8 text only".to_string()
            )
        );

        let miss = json!({"outcome": "not_found", "message": "no such file or directory: p"});
        assert_eq!(
            decide("p", &miss, "a", "b", false).unwrap(),
            Decision::Refuse("no such file or directory: p".to_string())
        );

        // Accepted gap: a CRLF file and an old_string written with a
        // bare \n do not match, and that is "not found", not a special
        // case.
        let crlf = ok_read("a\r\nb");
        assert_eq!(
            decide("p", &crlf, "a\nb", "x", false).unwrap(),
            Decision::Refuse("old_string does not occur in p".to_string())
        );
    }

    #[test]
    fn a_context_that_is_not_the_manifests_is_an_error() {
        let no_lossy = json!({"outcome": "ok", "content": "x", "truncated": false});
        assert!(decide("p", &no_lossy, "x", "y", false).is_err());
    }
}
