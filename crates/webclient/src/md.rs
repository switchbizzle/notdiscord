//! Markdown for the web app: the same pulldown-cmark grammar as the desktop
//! client (crates/client/src/md.rs), with a leaner renderer — links open in
//! the browser, mentions get a highlight, no context menus. Raw HTML renders
//! as literal text so messages can never inject markup.

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
    Span(Vec<MdNode>),
    Quote(Vec<MdNode>),
    List { start: Option<u64>, items: Vec<MdNode> },
    Item(Vec<MdNode>),
}

pub fn parse_markdown(src: &str) -> Vec<MdNode> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
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
                if let Some(node) = close_tag(tag, children) {
                    if let Some(top) = stack.last_mut() {
                        top.push(node);
                    }
                }
            }
            Event::Text(t) => push(&mut stack, MdNode::Text(t.to_string())),
            Event::Code(t) => push(&mut stack, MdNode::Code(t.to_string())),
            Event::Html(h) | Event::InlineHtml(h) => push(&mut stack, MdNode::Text(h.to_string())),
            Event::SoftBreak | Event::HardBreak | Event::Rule => push(&mut stack, MdNode::Break),
            Event::FootnoteReference(_) | Event::TaskListMarker(_) => {}
            Event::InlineMath(t) | Event::DisplayMath(t) => push(&mut stack, MdNode::Text(t.to_string())),
        }
    }
    stack.pop().unwrap_or_default()
}

fn push(stack: &mut [Vec<MdNode>], node: MdNode) {
    if let Some(top) = stack.last_mut() {
        top.push(node);
    }
}

fn close_tag(tag: Tag, children: Vec<MdNode>) -> Option<MdNode> {
    Some(match tag {
        Tag::Paragraph => MdNode::Para(children),
        Tag::Strong => MdNode::Bold(children),
        Tag::Emphasis => MdNode::Italic(children),
        Tag::Strikethrough => MdNode::Strike(children),
        Tag::Heading { .. } => MdNode::Para(vec![MdNode::Bold(children)]),
        Tag::BlockQuote(_) => MdNode::Quote(children),
        Tag::List(start) => MdNode::List { start, items: children },
        Tag::Item => MdNode::Item(children),
        Tag::CodeBlock(_) => MdNode::CodeBlock(flatten_text(&children)),
        Tag::Link { dest_url, .. } => {
            let url = dest_url.to_string();
            if url.starts_with("http://") || url.starts_with("https://") {
                MdNode::Link { url, children }
            } else {
                MdNode::Span(children)
            }
        }
        Tag::Image { .. } => MdNode::Italic(children),
        _ => {
            if children.is_empty() {
                return None;
            }
            MdNode::Span(children)
        }
    })
}

fn flatten_text(nodes: &[MdNode]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            MdNode::Text(t) | MdNode::Code(t) | MdNode::CodeBlock(t) => out.push_str(t),
            MdNode::Break => out.push('\n'),
            MdNode::Bold(c) | MdNode::Italic(c) | MdNode::Strike(c) | MdNode::Para(c)
            | MdNode::Span(c) | MdNode::Quote(c) | MdNode::List { items: c, .. }
            | MdNode::Item(c) | MdNode::Link { children: c, .. } => out.push_str(&flatten_text(c)),
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
        MdNode::Text(t) => rsx! { RichText { text: t } },
        MdNode::Bold(c) => rsx! { strong { Md { nodes: c } } },
        MdNode::Italic(c) => rsx! { em { Md { nodes: c } } },
        MdNode::Strike(c) => rsx! { del { Md { nodes: c } } },
        MdNode::Code(t) => rsx! { code { class: "md-code", "{t}" } },
        MdNode::CodeBlock(t) => rsx! { pre { class: "md-pre", "{t}" } },
        MdNode::Link { url, children } => rsx! {
            a { class: "md-link", href: "{url}", target: "_blank", rel: "noopener",
                Md { nodes: children }
            }
        },
        MdNode::Break => rsx! { br {} },
        MdNode::Para(c) => rsx! { p { class: "md-para", Md { nodes: c } } },
        MdNode::Span(c) => rsx! { span { Md { nodes: c } } },
        MdNode::Quote(c) => rsx! { blockquote { class: "md-quote", Md { nodes: c } } },
        MdNode::List { start, items } => match start {
            Some(first) => rsx! { ol { class: "md-list", start: "{first}", Md { nodes: items } } },
            None => rsx! { ul { class: "md-list", Md { nodes: items } } },
        },
        MdNode::Item(c) => rsx! { li { Md { nodes: c } } },
    }
}

/// Plain text runs still get bare-URL links and @mention highlights.
#[component]
fn RichText(text: String) -> Element {
    let mut segs: Vec<(u8, String)> = Vec::new(); // 0 plain, 1 url, 2 mention
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        if at_boundary && chars[i] == 'h' {
            let rest: String = chars[i..].iter().take(8).collect();
            if rest.starts_with("http://") || rest.starts_with("https://") {
                let mut j = i;
                while j < chars.len() && !chars[j].is_whitespace() && !matches!(chars[j], '<' | '>' | '"') {
                    j += 1;
                }
                while j > i && matches!(chars[j - 1], '.' | ',' | ')' | '!' | '?' | ';' | ':' | '\'') {
                    j -= 1;
                }
                if j > i + 8 {
                    if !plain.is_empty() {
                        segs.push((0, std::mem::take(&mut plain)));
                    }
                    segs.push((1, chars[i..j].iter().collect()));
                    i = j;
                    continue;
                }
            }
        }
        if chars[i] == '@' && at_boundary {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j > i + 1 {
                if !plain.is_empty() {
                    segs.push((0, std::mem::take(&mut plain)));
                }
                segs.push((2, chars[i..j].iter().collect()));
                i = j;
                continue;
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    if !plain.is_empty() {
        segs.push((0, plain));
    }

    rsx! {
        for (i, (kind, s)) in segs.into_iter().enumerate() {
            if kind == 1 {
                a { key: "{i}", class: "md-link", href: "{s}", target: "_blank", rel: "noopener", "{s}" }
            } else if kind == 2 {
                span { key: "{i}", class: "md-mention", "{s}" }
            } else {
                span { key: "{i}", "{s}" }
            }
        }
    }
}
