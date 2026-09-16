//! Model text on its way to a human (spec 04 §untrusted output, spec 11).
//!
//! Model output is untrusted, and one of its failure modes is specific and
//! recurring: when the tool menu is empty the model writes a tool call out as
//! text (`<read_file>…</read_file>`) and calls it an answer. The loop refuses
//! that answer now, but a summary that still carries stray markup must never
//! reach a pull request body or a comment, where it would be read by humans and
//! fed back to the next round as context.

/// Marker of the alternate dialect a model uses when it writes a tool call
/// as text: the provider's DSML tags, whose separators are FULLWIDTH VERTICAL
/// LINE (U+FF5C) rather than ASCII. Anything between two of those pipes and the
/// tag name (`invoke`, `parameter`, `tool_calls`, a tool name) is markup too.
const DSML_MARKER: &str = "\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}";
/// The characters that wrap a tag in that dialect, as escaped constants so this
/// source file never contains the raw sequence.
const TAG_OPEN: char = '\u{3c}';
const TAG_CLOSE: char = '\u{3e}';
const TAG_SLASH: char = '\u{2f}';

/// Tool names whose markup must never be published.
const TOOL_TAGS: &[&str] = &[
    "read_file",
    "grep",
    "glob",
    "list_dir",
    "show_base_file",
    "edit_file",
    "write_file",
    "function_call",
    "tool_call",
    "tool_calls",
];

/// Placeholder for a summary that was nothing but markup.
pub const EMPTY_SUMMARY: &str = "(本轮没有可发布的摘要)";

/// Strip tool-call markup from model TEXT and tidy the result.
pub fn model_text(text: &str) -> String {
    let mut out = strip_dsml(text);
    for tag in TOOL_TAGS {
        out = strip_tag(&out, tag);
    }
    // Collapse the blank space the removals leave behind.
    let mut tidied = String::with_capacity(out.len());
    let mut blank_run = 0usize;
    for line in out.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        tidied.push_str(line.trim_end());
        tidied.push('\n');
    }
    let tidied = tidied.trim().to_string();
    if tidied.is_empty() {
        return EMPTY_SUMMARY.to_string();
    }
    tidied
}

/// Remove every DSML tag: the opening tag, its parameters and the closing tag.
///
/// The dialect carries its own names, and its closing tag repeats the opening
/// name, so the block is recognisable without knowing the tool's schema.
fn strip_dsml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let Some(open) = rest.find(DSML_MARKER) else {
            out.push_str(rest);
            break;
        };
        let Some(name_start) = rest[open + DSML_MARKER.len()..]
            .find(char::is_alphabetic)
            .map(|offset| open + DSML_MARKER.len() + offset)
        else {
            out.push_str(&rest[..open]);
            rest = &rest[open + DSML_MARKER.len()..];
            continue;
        };
        let Some(name_end) = rest[name_start..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
            .map(|offset| name_start + offset)
        else {
            out.push_str(&rest[..open]);
            break;
        };
        let name = &rest[name_start..name_end];
        // A tag's opening angle bracket sits right before the marker; it leaves
        // with the block.
        let cut_start = if rest[..open].ends_with(TAG_OPEN) {
            open - TAG_OPEN.len_utf8()
        } else {
            open
        };
        out.push_str(&rest[..cut_start]);
        // Everything from the opening tag's `<` up to and including its closing
        // tag is markup. An unclosed block runs to the end of the text.
        let after_open = &rest[cut_start..];
        let close = format!("{TAG_SLASH}{DSML_MARKER}{name}{TAG_CLOSE}");
        match after_open.find(&close) {
            Some(end) => rest = &after_open[end + close.len()..],
            None => break,
        }
    }
    out
}

/// Remove every plain-XML tool block, and an unclosed opening tag's tail.
fn strip_tag(text: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(&open) {
        // A tag name must not be a prefix of a longer one (`<grep` vs `<grepper`).
        let after = &rest[start + open.len()..];
        let boundary = after
            .chars()
            .next()
            .is_none_or(|c| c == '>' || c == ' ' || c == '\n' || c == '\t' || c == '/');
        if !boundary {
            out.push_str(&rest[..start + open.len()]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..start]);
        match after.find(&close) {
            Some(end) => rest = &after[end + close.len()..],
            // Unclosed markup runs to the end of the text: everything after it
            // is the failed tool call, not prose.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_markup_is_removed_and_prose_is_kept() {
        let raw = "本轮修好了两个测试。\n\n<grep>\n<path>src/event.rs</path>\n<pattern>mod tests</pattern>\n</grep>\n\n另外补了单测。";
        let clean = model_text(raw);
        assert!(clean.contains("本轮修好了两个测试。"));
        assert!(clean.contains("另外补了单测。"));
        assert!(!clean.contains("<grep"));
        assert!(!clean.contains("src/event.rs"));
    }

    #[test]
    fn fenced_markup_is_removed_too() {
        let raw =
            "看这里：\n\n```\n<read_file>\n<path>a.rs</path>\n</read_file>\n```\n\n结论如上。";
        let clean = model_text(raw);
        assert!(clean.contains("结论如上。"));
        assert!(!clean.contains("read_file"));
    }

    #[test]
    fn an_unclosed_tag_does_not_swallow_real_prose_before_it() {
        let raw = "前一句是真话。\n<tool_call>\n{\"name\": \"grep\"}";
        let clean = model_text(raw);
        assert!(clean.contains("前一句是真话。"));
        assert!(!clean.contains("tool_call"));
    }

    #[test]
    fn a_longer_tag_name_is_not_mistaken_for_a_tool() {
        let clean = model_text("<grepper>not a tool</grepper>");
        assert!(clean.contains("<grepper>not a tool</grepper>"));
    }

    #[test]
    fn the_provider_specific_dsml_dialect_is_removed_too() {
        let lt = '\u{3c}';
        let gt = '\u{3e}';
        let slash = '\u{2f}';
        let pipes = "\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}";
        let raw = format!(
            "本轮无代码改动。\n\n{lt}{pipes}tool_calls{gt}\n{lt}{pipes}invoke name=\"grep\"{gt}\n\
             {lt}{pipes}parameter name=\"path\"{gt}src{lt}{slash}{pipes}parameter{gt}\n\
             {lt}{slash}{pipes}invoke{gt}\n{lt}{slash}{pipes}tool_calls{gt}\n"
        );
        let clean = model_text(&raw);
        assert_eq!(clean, "本轮无代码改动。");
        assert!(!clean.contains("DSML"));
    }

    #[test]
    fn an_unclosed_dsml_block_keeps_the_prose_before_it() {
        let lt = '\u{3c}';
        let gt = '\u{3e}';
        let pipes = "\u{ff5c}\u{ff5c}DSML\u{ff5c}\u{ff5c}";
        let raw = format!(
            "真话在前。\n{lt}{pipes}invoke name=\"read_file\"{gt}\n{lt}{pipes}parameter name=\"path\"{gt}a.rs"
        );
        let clean = model_text(&raw);
        assert!(clean.starts_with("真话在前。"));
        assert!(!clean.contains("DSML"));
    }

    #[test]
    fn plain_text_and_json_are_untouched() {
        let json = "{\"findings\": [{\"title\": \"x\", \"path\": \"a.rs\"}]}";
        assert_eq!(model_text(json), json);
        assert_eq!(model_text("  正常的一句话  "), "正常的一句话");
        assert_eq!(
            model_text("\n\n<glob><pattern>x</pattern></glob>\n\n"),
            EMPTY_SUMMARY
        );
    }
}
