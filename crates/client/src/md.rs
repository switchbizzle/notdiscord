//! Markdown for chat messages: parsed with pulldown-cmark into a small node
//! tree, rendered as real Dioxus elements. Raw/inline HTML is shown as
//! literal text, so message content can never inject markup.

use dioxus::prelude::*;
use pulldown_cmark::{Event, Options, Parser, Tag};

#[derive(Debug, Clone, PartialEq)]
pub enum MdNode {
    Text(String),
    Bold(Vec<MdNode>),
    Italic(Vec<MdNode>),
    Strike(Vec<MdNode>),
    Code(String),
    CodeBlock(String),
    Link { url: String, children: Vec<MdNode> },
    Break,
    Para(Vec<MdNode>),
    Quote(Vec<MdNode>),
    List(Vec<MdNode>),
    Item(Vec<MdNode>),
}

pub fn parse_markdown(src: &str) -> Vec<MdNode> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);

    // Stack of in-progress child lists; `tags` mirrors the open Start tags.
    let mut stack: Vec<Vec<MdNode>> = vec![Vec::new()];
    let mut tags: Vec<Tag> = Vec::new();

    for event in Parser::new_ext(src, options) {
        match event {
            Event::Start(tag) => {
                tags.push(tag);
                stack.push(Vec::new());
            }
            Event::End(_) => {
                let children = stack.pop().unwrap_or_default();
                let Some(tag) = tags.pop() else { continue };
                let node = close_tag(tag, children);
                if let Some(node) = node {
                    if let Some(top) = stack.last_mut() {
                        top.push(node);
                    }
                }
            }
            Event::Text(t) => push_text(&mut stack, t.to_string()),
            Event::Code(t) => push(&mut stack, MdNode::Code(t.to_string())),
            Event::Html(h) | Event::InlineHtml(h) => push_text(&mut stack, h.to_string()),
            Event::SoftBreak | Event::HardBreak => push(&mut stack, MdNode::Break),
            Event::Rule => push(&mut stack, MdNode::Break),
            Event::FootnoteReference(_) | Event::TaskListMarker(_) => {}
            Event::InlineMath(t) | Event::DisplayMath(t) => push_text(&mut stack, t.to_string()),
        }
    }
    stack.pop().unwrap_or_default()
}

fn push(stack: &mut [Vec<MdNode>], node: MdNode) {
    if let Some(top) = stack.last_mut() {
        top.push(node);
    }
}

fn push_text(stack: &mut [Vec<MdNode>], text: String) {
    push(stack, MdNode::Text(text));
}

fn close_tag(tag: Tag, children: Vec<MdNode>) -> Option<MdNode> {
    Some(match tag {
        Tag::Paragraph => MdNode::Para(children),
        Tag::Strong => MdNode::Bold(children),
        Tag::Emphasis => MdNode::Italic(children),
        Tag::Strikethrough => MdNode::Strike(children),
        Tag::Heading { .. } => MdNode::Para(vec![MdNode::Bold(children)]),
        Tag::BlockQuote(_) => MdNode::Quote(children),
        Tag::List(_) => MdNode::List(children),
        Tag::Item => MdNode::Item(children),
        Tag::CodeBlock(_) => MdNode::CodeBlock(flatten_text(&children)),
        Tag::Link { dest_url, .. } => {
            let url = dest_url.to_string();
            if url.starts_with("http://") || url.starts_with("https://") {
                MdNode::Link { url, children }
            } else {
                // Non-web schemes render as plain content.
                MdNode::Para(children)
            }
        }
        // Images: show the alt text, don't fetch anything.
        Tag::Image { .. } => MdNode::Italic(children),
        _ => return children_or_none(children),
    })
}

fn children_or_none(children: Vec<MdNode>) -> Option<MdNode> {
    if children.is_empty() {
        None
    } else {
        Some(MdNode::Para(children))
    }
}

fn flatten_text(nodes: &[MdNode]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            MdNode::Text(t) | MdNode::Code(t) | MdNode::CodeBlock(t) => out.push_str(t),
            MdNode::Break => out.push('\n'),
            MdNode::Bold(c) | MdNode::Italic(c) | MdNode::Strike(c) | MdNode::Para(c)
            | MdNode::Quote(c) | MdNode::List(c) | MdNode::Item(c)
            | MdNode::Link { children: c, .. } => out.push_str(&flatten_text(c)),
        }
    }
    out
}

#[component]
pub fn Md(nodes: Vec<MdNode>) -> Element {
    rsx! {
        for (i, node) in nodes.into_iter().enumerate() {
            MdOne { key: "{i}", node }
        }
    }
}

#[component]
fn MdOne(node: MdNode) -> Element {
    match node {
        MdNode::Text(t) => rsx! { "{t}" },
        MdNode::Bold(c) => rsx! { strong { Md { nodes: c } } },
        MdNode::Italic(c) => rsx! { em { Md { nodes: c } } },
        MdNode::Strike(c) => rsx! { del { Md { nodes: c } } },
        MdNode::Code(t) => rsx! { code { class: "md-code", "{t}" } },
        MdNode::CodeBlock(t) => rsx! { pre { class: "md-pre", code { "{t}" } } },
        MdNode::Break => rsx! { br {} },
        MdNode::Para(c) => rsx! { p { class: "md-p", Md { nodes: c } } },
        MdNode::Quote(c) => rsx! { blockquote { class: "md-quote", Md { nodes: c } } },
        MdNode::List(c) => rsx! { ul { class: "md-list", Md { nodes: c } } },
        MdNode::Item(c) => rsx! { li { Md { nodes: c } } },
        MdNode::Link { url, children } => {
            let title = url.clone();
            rsx! {
                span {
                    class: "md-link",
                    title: "{title}",
                    onclick: move |_| {
                        let _ = open::that(&url);
                    },
                    Md { nodes: children }
                }
            }
        }
    }
}
