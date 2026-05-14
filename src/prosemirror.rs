//! ProseMirror document → markdown.
//!
//! Port of upstream `src/lib/prosemirror.ts` (~75 lines). Pass anything (the
//! `notes` field, `last_viewed_panel.content`, etc.) and get markdown back.

use serde_json::Value;

pub fn to_markdown(doc: &Value) -> String {
    let Some(content) = doc.get("content").and_then(Value::as_array) else {
        return String::new();
    };
    content
        .iter()
        .map(node_to_md)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn node_to_md(node: &Value) -> String {
    let node_type = node.get("type").and_then(Value::as_str).unwrap_or("");
    let content = node.get("content").and_then(Value::as_array);

    match node_type {
        "heading" => {
            let lvl = node
                .pointer("/attrs/level")
                .and_then(Value::as_u64)
                .unwrap_or(1) as usize;
            format!("{} {}", "#".repeat(lvl.clamp(1, 6)), inline_to_md(content))
        }
        "paragraph" => inline_to_md(content),
        "bulletList" => content
            .map(|c| c.iter().map(node_to_md).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default(),
        "orderedList" => content
            .map(|c| {
                c.iter()
                    .enumerate()
                    .map(|(i, n)| {
                        let s = node_to_md(n);
                        if let Some(rest) = s.strip_prefix("- ") {
                            format!("{}. {}", i + 1, rest)
                        } else {
                            s
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "listItem" => {
            let inner = content
                .map(|c| c.iter().map(node_to_md).collect::<Vec<_>>().join("\n  "))
                .unwrap_or_default();
            format!("- {}", inner)
        }
        "blockquote" => content
            .map(|c| {
                c.iter()
                    .map(|n| format!("> {}", node_to_md(n)))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        "codeBlock" => {
            let lang = node
                .pointer("/attrs/language")
                .and_then(Value::as_str)
                .unwrap_or("");
            format!("```{}\n{}\n```", lang, inline_to_md(content))
        }
        "horizontalRule" => "---".to_string(),
        "text" => {
            let text = node.get("text").and_then(Value::as_str).unwrap_or("");
            let marks = node.get("marks").and_then(Value::as_array);
            apply_marks(text, marks)
        }
        _ => content
            .map(|c| c.iter().map(node_to_md).collect::<String>())
            .unwrap_or_default(),
    }
}

fn inline_to_md(content: Option<&Vec<Value>>) -> String {
    content
        .map(|c| c.iter().map(node_to_md).collect::<String>())
        .unwrap_or_default()
}

fn apply_marks(text: &str, marks: Option<&Vec<Value>>) -> String {
    let Some(marks) = marks else {
        return text.to_string();
    };
    let mut out = text.to_string();
    for m in marks {
        match m.get("type").and_then(Value::as_str).unwrap_or("") {
            "bold" | "strong" => out = format!("**{}**", out),
            "italic" | "em" => out = format!("*{}*", out),
            "code" => out = format!("`{}`", out),
            "strike" => out = format!("~~{}~~", out),
            _ => {}
        }
    }
    out
}
