pub fn strip_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_code_block = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // switch the state of the fenced code block (``` or ~~~)
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code_block = !in_code_block;
            out.push(' ');
            continue;
        }
        if in_code_block {
            continue;
        }

        // skip table separators like |---|---| and horizontal lines --- / ***
        if !trimmed.is_empty()
            && trimmed
                .chars()
                .all(|c| matches!(c, '|' | '-' | ':' | '=' | '*' | '_' | ' '))
        {
            continue;
        }

        // remove headings (#), quotes (>) and list markers (-, *, +, 1.) at the beginning of the line
        let mut s = trimmed.trim_start_matches(['#', '>', ' ']);
        if let Some(rest) = s
            .strip_prefix("- ")
            .or_else(|| s.strip_prefix("* "))
            .or_else(|| s.strip_prefix("+ "))
        {
            s = rest;
        } else if let Some(dot_pos) = s.find(". ") {
            if dot_pos <= 3 && s[..dot_pos].chars().all(|c| c.is_ascii_digit()) {
                s = &s[dot_pos + 2..];
            }
        }

        // clean up inline markup: [text](url) -> text, **bold**, *italic*, `code`, table cells |
        let mut chars = s.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '[' => {
                    let mut label = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == ']' {
                            closed = true;
                            break;
                        }
                        label.push(c);
                    }
                    out.push_str(&label);
                    if closed && chars.peek() == Some(&'(') {
                        chars.next();
                        for c in chars.by_ref() {
                            if c == ')' {
                                break;
                            }
                        }
                    }
                }
                '*' | '_' | '`' | '~' => {}
                '|' => out.push(' '),
                _ => out.push(ch),
            }
        }

        out.push(' ');
    }

    out
}

pub fn truncate_inline(text: &str, max_chars: usize) -> String {
    let cleaned = strip_markdown(text);
    let normalized = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut chars = normalized.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}...")
    } else {
        head
    }
}
