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
    /// A fenced block, with the language from its fence (empty when the
    /// fence didn't name one). The language decides the colouring.
    CodeBlock { lang: String, code: String },
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
        Tag::CodeBlock(kind) => MdNode::CodeBlock {
            lang: match kind {
                pulldown_cmark::CodeBlockKind::Fenced(lang) => lang.to_string(),
                pulldown_cmark::CodeBlockKind::Indented => String::new(),
            },
            code: flatten_text(&children),
        },
        Tag::Link { dest_url, .. } => {
            let url = dest_url.to_string();
            if shared::is_web_url(&url) {
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
            MdNode::Text(t) | MdNode::Code(t) => out.push_str(t),
            MdNode::CodeBlock { code, .. } => out.push_str(code),
            MdNode::Break => out.push('\n'),
            MdNode::Bold(c) | MdNode::Italic(c) | MdNode::Strike(c) | MdNode::Para(c)
            | MdNode::Span(c) | MdNode::Quote(c) | MdNode::List { items: c, .. }
            | MdNode::Item(c) | MdNode::Link { children: c, .. } => out.push_str(&flatten_text(c)),
        }
    }
    out
}

/// A permalink into this same server opens the message here rather than in a
/// second tab. Everything else — including another NotDiscord's permalink,
/// which has the same shape and unrelated ids — stays an ordinary link, so
/// the anchor keeps its href and its middle-click.
fn follow_in_app(url: &str) -> bool {
    let Some(open_message) = try_consume_context::<crate::OpenMessage>() else {
        return false;
    };
    let public = try_consume_context::<crate::PublicUrl>().and_then(|p| p.0());
    match shared::parse_message_link(url, &crate::permalink_bases(&public)) {
        Some(target) => {
            let mut signal = open_message.0;
            signal.set(Some(target));
            true
        }
        None => false,
    }
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
        MdNode::CodeBlock { lang, code } => rsx! {
            pre { class: "md-pre",
                for (i, span) in shared::highlight::highlight(&lang, &code).into_iter().enumerate() {
                    span { key: "{i}", class: span.kind.class(), "{span.text}" }
                }
            }
        },
        MdNode::Link { url, children } => rsx! {
            a {
                class: "md-link",
                href: "{url}",
                target: "_blank",
                rel: "noopener",
                onclick: move |e: MouseEvent| {
                    // The whole message body is a button that opens the
                    // actions sheet, and a tapped link must never reach it —
                    // EVERY link, not only the in-app kind. An external one
                    // opened its tab and then left the sheet sitting under
                    // it for you to find on coming back.
                    e.stop_propagation();
                    if follow_in_app(&url) {
                        e.prevent_default();
                    }
                },
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

/// Plain text runs still get bare-URL links, @mention highlights and
/// `:name:` server emojis.
#[component]
fn RichText(text: String) -> Element {
    // try_consume_context, not use_context: this renders inside link cards and
    // anywhere else a message body appears, and an unprovided context panics
    // in wasm, taking the whole page down rather than one emoji.
    let emojis = try_consume_context::<Signal<Vec<shared::CustomEmoji>>>();
    let mut segs: Vec<(u8, String)> = Vec::new(); // 0 plain, 1 url, 2 mention, 3 emoji
    let chars: Vec<char> = text.chars().collect();
    let mut plain = String::new();
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || !chars[i - 1].is_alphanumeric();
        if at_boundary && (chars[i] == 'h' || chars[i] == 'H') {
            let rest: String = chars[i..].iter().take(8).collect();
            if shared::is_web_url(&rest) {
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
        // :emoji_name: — lowercase, digits and underscores, closing colon,
        // and at least two characters, so a bare ":" or a ":)" stays text.
        if chars[i] == ':' {
            let mut j = i + 1;
            while j < chars.len()
                && (chars[j].is_ascii_lowercase() || chars[j].is_ascii_digit() || chars[j] == '_')
            {
                j += 1;
            }
            if j < chars.len() && chars[j] == ':' && j >= i + 3 {
                if !plain.is_empty() {
                    segs.push((0, std::mem::take(&mut plain)));
                }
                segs.push((3, chars[i + 1..j].iter().collect()));
                i = j + 1;
                continue;
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
                a {
                    key: "{i}",
                    class: "md-link",
                    href: "{s}",
                    target: "_blank",
                    rel: "noopener",
                    onclick: {
                        let url = s.clone();
                        move |e: MouseEvent| {
                            // Same rule as MdNode::Link above.
                            e.stop_propagation();
                            if follow_in_app(&url) {
                                e.prevent_default();
                            }
                        }
                    },
                    "{s}"
                }
            } else if kind == 2 {
                span { key: "{i}", class: "md-mention", "{s}" }
            } else if kind == 3 {
                // No emoji by that name (or no list yet) leaves the text as
                // written, so ordinary colon-y prose is never eaten.
                {
                    let found = emojis
                        .and_then(|list| list.read().iter().find(|e| e.name == s).cloned());
                    match found {
                        Some(e) => rsx! {
                            img {
                                key: "{i}",
                                class: "custom-emoji",
                                src: "{e.url}",
                                alt: ":{s}:",
                                title: ":{s}:",
                            }
                        },
                        None => rsx! { span { key: "{i}", ":{s}:" } },
                    }
                }
            } else {
                span { key: "{i}", "{s}" }
            }
        }
    }
}
