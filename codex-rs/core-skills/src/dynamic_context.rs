#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineCommandPlaceholder {
    pub start: usize,
    pub end: usize,
    pub command: String,
}

/// Finds Claude-style dynamic context placeholders of the form `!`command``.
///
/// Multiline placeholders are ignored so a missing closing backtick does not
/// accidentally consume a large section of the skill body.
pub fn collect_inline_command_placeholders(contents: &str) -> Vec<InlineCommandPlaceholder> {
    let bytes = contents.as_bytes();
    let mut placeholders = Vec::new();
    let mut cursor = 0;

    while cursor + 2 <= bytes.len() {
        let Some(relative_start) = bytes[cursor..]
            .windows(2)
            .position(|window| window == b"!`")
        else {
            break;
        };
        let start = cursor + relative_start;
        let command_start = start + 2;
        let mut scan = command_start;
        let mut found_end = None;
        let mut multiline = false;

        while scan < bytes.len() {
            match bytes[scan] {
                b'`' => {
                    found_end = Some(scan);
                    break;
                }
                b'\n' | b'\r' => {
                    multiline = true;
                    break;
                }
                _ => scan += 1,
            }
        }

        let Some(command_end) = found_end else {
            cursor = if multiline { scan + 1 } else { command_start };
            continue;
        };
        let command = &contents[command_start..command_end];
        if !command.is_empty() {
            placeholders.push(InlineCommandPlaceholder {
                start,
                end: command_end + 1,
                command: command.to_string(),
            });
        }
        cursor = command_end + 1;
    }

    placeholders
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_inline_command_placeholders() {
        let placeholders = collect_inline_command_placeholders(
            "- Diff: !`gh pr diff`\n- Files: !`gh pr diff --name-only`",
        );

        assert_eq!(
            placeholders
                .into_iter()
                .map(|placeholder| placeholder.command)
                .collect::<Vec<_>>(),
            vec!["gh pr diff", "gh pr diff --name-only"]
        );
    }

    #[test]
    fn ignores_multiline_placeholders() {
        let placeholders = collect_inline_command_placeholders("before !`unterminated\nafter`");

        assert!(placeholders.is_empty());
    }
}
