use std::collections::HashMap;

use anyhow::{Result, ensure};
use tree_sitter::{Language, Node, Parser};

pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_CHUNK_BYTES: usize = 4000;
const MIN_CHUNK_BYTES: usize = 400;
const OVERLAP_LINES: usize = 2;

#[derive(Clone, Debug)]
pub struct Chunk {
    pub start: usize,
    pub end: usize,
    pub breadcrumb: String,
}

pub fn language(path: &str) -> &'static str {
    match path
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" | "pyi" => "python",
        "rs" => "rust",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "cs" => "csharp",
        "sh" | "bash" | "zsh" => "shell",
        "rb" => "ruby",
        "php" => "php",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "lua" => "lua",
        "md" | "mdx" => "markdown",
        "json" | "jsonc" => "json",
        "toml" | "yaml" | "yml" | "xml" | "html" | "css" | "scss" | "sql" | "txt" | "rst"
        | "proto" | "graphql" => "text",
        _ => "unknown",
    }
}

fn grammar(language: &str) -> Option<Language> {
    Some(match language {
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "cpp" => tree_sitter_cpp::LANGUAGE.into(),
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "shell" => tree_sitter_bash::LANGUAGE.into(),
        _ => return None,
    })
}

#[derive(Default)]
pub struct Chunker {
    parsers: HashMap<&'static str, Parser>,
}

impl Chunker {
    pub fn split(&mut self, text: &str, language: &'static str) -> Vec<Chunk> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let mut boundaries = Vec::new();
        let mut scopes = Vec::new();
        if let Some(grammar) = grammar(language) {
            let parser = self.parsers.entry(language).or_default();
            if parser.set_language(&grammar).is_ok() {
                // Parse state is local to this worker; ASTs are released after each file.
                if let Some(tree) = parser.parse(text, None) {
                    collect_scopes(tree.root_node(), text, "", 0, &mut boundaries, &mut scopes);
                }
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(i, _)| i + 1))
            .collect();
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let mut limit = (start + MAX_CHUNK_BYTES).min(text.len());
            while !text.is_char_boundary(limit) {
                limit -= 1;
            }
            let end = if limit == text.len() {
                limit
            } else {
                boundaries
                    .iter()
                    .copied()
                    .rfind(|&b| b >= start + MIN_CHUNK_BYTES && b <= limit)
                    .or_else(|| {
                        line_starts
                            .iter()
                            .copied()
                            .rfind(|&b| b >= start + MIN_CHUNK_BYTES && b <= limit)
                    })
                    .unwrap_or(limit)
            };
            let breadcrumb = scopes
                .iter()
                .filter(|(a, b, _)| *a <= start && start < *b)
                .min_by_key(|(a, b, _)| b - a)
                .map(|(_, _, name)| name.clone())
                .unwrap_or_default();
            if !text[start..end].trim().is_empty() {
                chunks.push(Chunk {
                    start,
                    end,
                    breadcrumb,
                });
            }
            if end == text.len() {
                break;
            }
            let next = line_starts
                .iter()
                .copied()
                .filter(|&b| b > start && b < end)
                .rev()
                .nth(OVERLAP_LINES - 1)
                .unwrap_or(end);
            // Tiny overlap windows must not cause unbounded duplication.
            start = if next > start && end - next < MAX_CHUNK_BYTES / 4 {
                next
            } else {
                end
            };
        }
        chunks
    }
}

fn collect_scopes(
    node: Node<'_>,
    text: &str,
    parent: &str,
    depth: usize,
    boundaries: &mut Vec<usize>,
    scopes: &mut Vec<(usize, usize, String)>,
) {
    if depth > 64 {
        return;
    }
    let kind = node.kind();
    let structural = [
        "function",
        "method",
        "class",
        "struct",
        "interface",
        "impl",
        "enum",
        "trait",
    ]
    .iter()
    .any(|name| kind.contains(name));
    let mut scope = parent.to_owned();
    if structural
        && let Some(name) = node
            .child_by_field_name("name")
            .or_else(|| node.child_by_field_name("type"))
    {
        let name = text.get(name.byte_range()).unwrap_or("");
        if !name.is_empty() && name.len() <= 120 {
            scope = if parent.is_empty() {
                name.to_owned()
            } else {
                format!("{parent} > {name}")
            };
            scopes.push((node.start_byte(), node.end_byte(), scope.clone()));
            boundaries.push(node.start_byte());
            boundaries.push(node.end_byte());
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_scopes(child, text, &scope, depth + 1, boundaries, scopes);
    }
}

pub fn decode(bytes: &[u8]) -> Result<String> {
    let (text, errors) = if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        let (text, errors) = encoding.decode_without_bom_handling(&bytes[bom..]);
        (text, errors)
    } else {
        let text = std::str::from_utf8(bytes)
            .map(std::borrow::Cow::Borrowed)
            .map_err(|_| anyhow::anyhow!("unsupported encoding: use UTF-8 or BOM-marked UTF-16"))?;
        (text, false)
    };
    ensure!(
        !errors && !text.contains('\0'),
        "binary data or invalid text encoding"
    );
    ensure!(
        text.len() <= MAX_FILE_BYTES * 3,
        "decoded file exceeds size limit"
    );
    Ok(text.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_crlf_and_long_lines_have_valid_complete_ranges() {
        let text = format!("fn 登录() {{\r\n{}\r\n}}", "中文🦀".repeat(1800));
        let chunks = Chunker::default().split(&text, "rust");
        let mut covered = vec![false; text.len()];
        for chunk in &chunks {
            assert!(text.get(chunk.start..chunk.end).is_some());
            assert!(chunk.end - chunk.start <= MAX_CHUNK_BYTES);
            covered[chunk.start..chunk.end].fill(true);
        }
        assert!(covered.into_iter().all(|v| v));
        assert!(chunks.iter().any(|c| c.breadcrumb.contains("登录")));
    }

    #[test]
    fn unsupported_grammar_uses_text_chunks_and_binary_is_rejected() {
        assert!(!Chunker::default().split("puts 'hello'", "ruby").is_empty());
        assert!(decode(b"abc\0def").is_err());
        assert_eq!(decode(&[0xff, 0xfe, b'A', 0]).unwrap(), "A");
        assert!(decode(&[0xff, 0xfe, 0]).is_err());
    }
}
