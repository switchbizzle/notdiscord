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
    /// A fenced block, with the language from its fence (empty when the
    /// fence didn't name one). The language decides the colouring.
    CodeBlock { lang: String, code: String },
    Link { url: String, children: Vec<MdNode> },
    Break,
    Para(Vec<MdNode>),
    /// Inline children that must not open a block — used where markdown
    /// hands us something we render as plain content mid-sentence.
    Span(Vec<MdNode>),
    Quote(Vec<MdNode>),
    /// `start` is the first number of an ordered list; `None` is a bullet
    /// list. Typing "1. thing" and getting a bullet back loses the number.
    List { start: Option<u64>, items: Vec<MdNode> },
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
        Tag::List(start) => MdNode::List { start, items: children },
        Tag::Item => MdNode::Item(children),
        Tag::CodeBlock(kind) => MdNode::CodeBlock {
            lang: match kind {
                pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                pulldown_cmark::CodeBlockKind::Indented => String::new(),
            },
            code: flatten_text(&children),
        },
        Tag::Link { dest_url, .. } => {
            let url = dest_url.to_string();
            if url.starts_with("http://") || url.starts_with("https://") {
                MdNode::Link { url, children }
            } else {
                // Non-web schemes render as plain content — inline, so a
                // mailto: in the middle of a sentence doesn't break the line.
                MdNode::Span(children)
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
        // Inline: an unhandled tag shouldn't start a new block.
        Some(MdNode::Span(children))
    }
}

fn flatten_text(nodes: &[MdNode]) -> String {
    let mut out = String::new();
    for node in nodes {
        match node {
            MdNode::Text(t) | MdNode::Code(t) => out.push_str(t),
            MdNode::CodeBlock { code, .. } => out.push_str(code),
            MdNode::Break => out.push('\n'),
            MdNode::Bold(c) | MdNode::Italic(c) | MdNode::Strike(c) | MdNode::Para(c)
            | MdNode::Span(c) | MdNode::Quote(c) | MdNode::List { items: c, .. }
            | MdNode::Item(c) | MdNode::Link { children: c, .. } => {
                out.push_str(&flatten_text(c))
            }
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

#[derive(Clone, PartialEq)]
enum Seg {
    Plain(String),
    Mention(String),
    Url(String),
    /// `:name:` — rendered as the server emoji's image when it exists.
    Emoji(String),
}

/// Split text into plain runs, `@mention` tokens, bare `http(s)://` URLs, and
/// `:emoji:` names.
fn rich_segments(text: &str) -> Vec<Seg> {
    let mut segments: Vec<Seg> = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;

    let flush_plain = |plain: &mut String, segments: &mut Vec<Seg>| {
        if !plain.is_empty() {
            segments.push(Seg::Plain(std::mem::take(plain)));
        }
    };

    while i < chars.len() {
        let at_boundary = i == 0 || !chars[i - 1].is_alphanumeric();

        // Bare URL detection.
        if at_boundary && chars[i] == 'h' {
            let rest: String = chars[i..].iter().take(8).collect();
            if rest.starts_with("http://") || rest.starts_with("https://") {
                let mut j = i;
                while j < chars.len() && !chars[j].is_whitespace() && !matches!(chars[j], '<' | '>' | '"') {
                    j += 1;
                }
                // Trailing punctuation belongs to the sentence, not the URL.
                while j > i && matches!(chars[j - 1], '.' | ',' | ')' | '!' | '?' | ';' | ':' | '\'') {
                    j -= 1;
                }
                if j > i + 8 {
                    flush_plain(&mut plain, &mut segments);
                    segments.push(Seg::Url(chars[i..j].iter().collect()));
                    i = j;
                    continue;
                }
            }
        }

        // :emoji_name: detection.
        if chars[i] == ':' {
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].is_ascii_lowercase() || chars[j].is_ascii_digit() || chars[j] == '_')
            {
                j += 1;
            }
            // Needs a closing colon and at least two name characters.
            if j < chars.len() && chars[j] == ':' && j >= i + 3 {
                flush_plain(&mut plain, &mut segments);
                segments.push(Seg::Emoji(chars[i + 1..j].iter().collect()));
                i = j + 1;
                continue;
            }
        }

        // @mention detection.
        if chars[i] == '@' && at_boundary {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j > i + 1 {
                flush_plain(&mut plain, &mut segments);
                segments.push(Seg::Mention(chars[i..j].iter().collect()));
                i = j;
                continue;
            }
        }

        plain.push(chars[i]);
        i += 1;
    }
    flush_plain(&mut plain, &mut segments);
    segments
}

/// `:name:` renders as the server emoji's image, or stays literal text when
/// no emoji by that name exists (so ordinary colon-y text is untouched).
#[component]
fn CustomEmoji(name: String) -> Element {
    let emojis = use_context::<Signal<Vec<shared::CustomEmoji>>>();
    let found = emojis().into_iter().find(|e| e.name == name);
    match found {
        Some(emoji) => rsx! {
            img { class: "custom-emoji", src: "{emoji.url}", alt: ":{name}:", title: ":{name}:" }
        },
        None => rsx! { span { ":{name}:" } },
    }
}

/// Copy link / Open link — the only two things a link can do.
fn link_menu(ctx_menu: crate::menu::MenuSignal, url: String) -> impl FnMut(MouseEvent) {
    move |e: MouseEvent| {
        let url = url.clone();
        crate::menu::open(ctx_menu, &e, vec![
            crate::menu::item("Open link", "external-link", {
                let url = url.clone();
                move || {
                    let _ = open::that(&url);
                }
            }),
            crate::menu::item("Copy link", "link", move || {
                crate::menu::copy_to_clipboard(url.clone())
            }),
        ]);
    }
}

#[component]
fn MdOne(node: MdNode) -> Element {
    let ctx_menu = use_context::<crate::menu::MenuSignal>();
    match node {
        MdNode::Text(t) => rsx! {
            for (i, seg) in rich_segments(&t).into_iter().enumerate() {
                match seg {
                    Seg::Plain(s) => rsx! { span { key: "{i}", "{s}" } },
                    Seg::Mention(s) => rsx! { span { key: "{i}", class: "mention", "{s}" } },
                    Seg::Emoji(name) => rsx! { CustomEmoji { key: "{i}", name } },
                    Seg::Url(url) => {
                        let title = url.clone();
                        let menu_url = url.clone();
                        rsx! {
                            span {
                                key: "{i}",
                                class: "md-link",
                                title: "{title}",
                                onclick: move |_| {
                                    let _ = open::that(&url);
                                },
                                oncontextmenu: link_menu(ctx_menu, menu_url),
                                "{title}"
                            }
                        }
                    }
                }
            }
        },
        MdNode::Bold(c) => rsx! { strong { Md { nodes: c } } },
        MdNode::Italic(c) => rsx! { em { Md { nodes: c } } },
        MdNode::Strike(c) => rsx! { del { Md { nodes: c } } },
        MdNode::Code(t) => rsx! { code { class: "md-code", "{t}" } },
        MdNode::CodeBlock { lang, code } => rsx! {
            pre { class: "md-pre",
                code {
                    for (i, span) in shared::highlight::highlight(&lang, &code).into_iter().enumerate() {
                        span { key: "{i}", class: span.kind.class(), "{span.text}" }
                    }
                }
            }
        },
        MdNode::Break => rsx! { br {} },
        MdNode::Para(c) => rsx! { p { class: "md-p", Md { nodes: c } } },
        MdNode::Span(c) => rsx! { span { Md { nodes: c } } },
        MdNode::Quote(c) => rsx! { blockquote { class: "md-quote", Md { nodes: c } } },
        // "1. thing" keeps its numbers, and keeps the number it started at.
        MdNode::List { start: Some(first), items } => rsx! {
            ol { class: "md-list", start: "{first}", Md { nodes: items } }
        },
        MdNode::List { start: None, items } => rsx! {
            ul { class: "md-list", Md { nodes: items } }
        },
        MdNode::Item(c) => rsx! { li { class: "md-item", Md { nodes: c } } },
        MdNode::Link { url, children } => {
            let title = url.clone();
            let menu_url = url.clone();
            rsx! {
                span {
                    class: "md-link",
                    title: "{title}",
                    onclick: move |_| {
                        let _ = open::that(&url);
                    },
                    oncontextmenu: link_menu(ctx_menu, menu_url),
                    Md { nodes: children }
                }
            }
        }
    }
}
