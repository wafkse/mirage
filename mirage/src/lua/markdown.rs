//! Markdown documents for Luau extensions.
//!
//! See [`Markdown`] for additional information.
use mlua::{Lua, Table};

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};

use super::library::Library;

/// A parser-owned node whose classification cannot be changed by Lua table mutation.
// NOTE(invariant): `code` is true exactly when the node was constructed as a code block.
struct Node {
    /// The semantic table returned to the caller after parsing.
    table: Table,

    /// Whether text events accumulate in the code block literal.
    code: bool,
}

impl Node {
    /// Allocate a semantic node and its ordered children.
    fn new(lua: &Lua, kind: &str) -> mlua::Result<Self> {
        let table = lua.create_table()?;
        let children = lua.create_table()?;
        table.set("type", kind)?;
        table.set("children", children)?;

        let code = kind == "code_block";

        Ok(Self { table, code })
    }

    /// Set an attribute while retaining ownership of the node classification.
    fn set(&self, key: &str, value: impl mlua::IntoLua) -> mlua::Result<()> {
        let Self { table, .. } = self;

        table.set(key, value)
    }

    /// Append a finished child to the ordered semantic children.
    fn append(&self, child: Self) -> mlua::Result<()> {
        let Self { table, .. } = self;
        let Self { table: child, .. } = child;
        let children: Table = table.get("children")?;

        children.raw_set(children.raw_len() + 1, child)
    }

    /// Accumulate literal code without creating text children.
    fn text(&self, value: &str) -> mlua::Result<()> {
        let Self { table, .. } = self;
        let mut literal: String = table.get("literal")?;
        literal.push_str(value);

        table.set("literal", literal)
    }
}

/// Markdown parsing available through `require("@mirage/markdown")`.
///
/// Returns document nodes for templates to present, without generating HTML or reading files.
pub struct Markdown;

impl Library for Markdown {
    #[inline]
    fn build(self, lua: &Lua) -> mlua::Result<Table> {
        let library = lua.create_table()?;
        let parse = lua.create_function(|lua, source: String| parse(lua, &source))?;

        library.set("parse", parse)?;

        Ok(library)
    }
}

impl Node {
    /// Translate a container tag into its semantic node.
    fn start(lua: &Lua, tag: Tag<'_>) -> mlua::Result<Node> {
        let is_image = matches!(&tag, Tag::Image { .. });
        let child = match tag {
            Tag::Paragraph => Node::new(lua, "paragraph")?,
            Tag::Heading { level, id, .. } => {
                let n = Node::new(lua, "heading")?;
                n.set("level", level as u8)?;
                if let Some(id) = id {
                    n.set("id", id.as_ref())?;
                }
                n
            }
            Tag::BlockQuote(_) => Node::new(lua, "blockquote")?,
            Tag::CodeBlock(kind) => {
                let n = Node::new(lua, "code_block")?;
                n.set("literal", "")?;
                n.set(
                    "language",
                    match kind {
                        CodeBlockKind::Fenced(info) => {
                            info.split_whitespace().next().unwrap_or("").to_string()
                        }
                        CodeBlockKind::Indented => String::new(),
                    },
                )?;
                n
            }
            Tag::List(start) => {
                let n = Node::new(lua, "list")?;
                n.set("ordered", start.is_some())?;
                if let Some(start) = start {
                    n.set("start", start)?;
                }
                n
            }
            Tag::Item => Node::new(lua, "list_item")?,
            Tag::Emphasis => Node::new(lua, "emphasis")?,
            Tag::Strong => Node::new(lua, "strong")?,
            Tag::Strikethrough => Node::new(lua, "strikethrough")?,
            Tag::Link {
                dest_url, title, ..
            }
            | Tag::Image {
                dest_url, title, ..
            } => {
                let n = Node::new(lua, if is_image { "image" } else { "link" })?;
                n.set("destination", dest_url.as_ref())?;
                n.set("title", title.as_ref())?;
                n
            }
            Tag::Table(_) => Node::new(lua, "table")?,
            Tag::TableHead => {
                let n = Node::new(lua, "table_row")?;
                n.set("header", true)?;
                n
            }
            Tag::TableRow => {
                let n = Node::new(lua, "table_row")?;
                n.set("header", false)?;
                n
            }
            Tag::TableCell => Node::new(lua, "table_cell")?,
            Tag::FootnoteDefinition(label) => {
                let n = Node::new(lua, "footnote_definition")?;
                n.set("label", label.as_ref())?;
                n
            }
            _ => Node::new(lua, "container")?,
        };
        Ok(child)
    }
}

/// Parse Markdown into a document tree.
fn parse(lua: &Lua, source: &str) -> mlua::Result<Table> {
    let root = Node::new(lua, "document")?;
    let mut stack: Vec<Node> = Vec::new();
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    for event in Parser::new_ext(source, options) {
        match event {
            Event::Start(tag) => {
                let child = Node::start(lua, tag)?;
                stack.push(child);
            }
            Event::End(_) => match stack.pop() {
                Some(child) => stack.last().unwrap_or(&root).append(child)?,
                None => return Err(mlua::Error::runtime("unmatched Markdown end event")),
            },
            event => {
                let parent = stack.last().unwrap_or(&root);
                let kind = match &event {
                    Event::Code(_) => "code",
                    Event::Html(_) | Event::InlineHtml(_) => "html",
                    Event::FootnoteReference(_) => "footnote_reference",
                    _ => "text",
                };
                let child = match event {
                    Event::Text(value) if parent.code => {
                        parent.text(&value)?;
                        continue;
                    }
                    Event::Text(value)
                    | Event::Code(value)
                    | Event::Html(value)
                    | Event::InlineHtml(value)
                    | Event::FootnoteReference(value) => {
                        let n = Node::new(lua, kind)?;
                        n.set("literal", value.as_ref())?;
                        n
                    }
                    Event::SoftBreak => {
                        let n = Node::new(lua, "text")?;
                        n.set("literal", "\n")?;
                        n
                    }
                    Event::HardBreak => Node::new(lua, "hard_break")?,
                    Event::Rule => Node::new(lua, "thematic_break")?,
                    Event::TaskListMarker(checked) => {
                        let n = Node::new(lua, "task_marker")?;
                        n.set("checked", checked)?;
                        n
                    }
                    _ => continue,
                };
                parent.append(child)?;
            }
        }
    }
    let Node { table, .. } = root;

    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commonmark_nodes_preserve_structure_and_literal_content() {
        let lua = Lua::new();
        lua.globals()
            .set("markdown", Markdown.build(&lua).unwrap())
            .unwrap();
        lua.load(r###"
            local doc = markdown.parse("## Hello\n\n1. **bold**\n   - nested\n\n```rust\n<x>\n```\n\n<script>raw</script>")
            assert(doc.type == 'document')
            assert(doc.children[1].level == 2)
            assert(doc.children[2].ordered == true)
            assert(doc.children[2].children[1].children[2].type == 'list')
            assert(doc.children[3].literal == '<x>\n')
            assert(doc.children[4].children[1].type == 'html')
        "###).exec().unwrap();
    }
}
