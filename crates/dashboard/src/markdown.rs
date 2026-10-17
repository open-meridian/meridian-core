//! A vendor's documentation drawn safely (W6.9, contract v25;
//! plans/a-plugin-carries-its-own-documentation, "The safe subset").
//!
//! CommonMark, parsed by pulldown-cmark with tables on and nothing else, its
//! events drawn by the writer below, which emits the subset's elements and
//! nothing else: headings (a level-one heading drawn as level two, anything
//! deeper than level four drawn as four), paragraphs, emphasis, inline code
//! and code blocks (no highlighting, no class), lists, tables, block quotes
//! and rules. No attribute but an `href` this module wrote, a list's
//! numeric `start`, and its own fixed classes.
//!
//! - **Raw HTML** is shown as the literal text it is.
//! - **An image**, local or remote, is never fetched: its alt text is shown,
//!   marked "image not shown".
//! - **A link** is kept only when it is `https:`, `http:` or `mailto:`, or
//!   names another section of the same bundle (`setup.md`,
//!   `setup.md#heading`), which opens that section at that heading; anything
//!   else (`javascript:`, `data:`, `file:`, a path outside the bundle) is its
//!   text alone. An outside link is drawn with its destination's host beside
//!   its text and a mark saying it leaves the deployment, opens apart, and
//!   sends no referrer.
//! - **Characters that hide or reorder words** (tag, bidirectional,
//!   zero-width and private-use characters, [`meridian_domain::text`]) are
//!   not drawn; [`hidden`] says a section held some, for the page's warning.
//!
//! One screen: a section is cut at each level-one and level-two heading into
//! parts, each a contents entry beside it, one shown at a time.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};

use crate::html::escape;

/// One part of a section: its heading's text (none for what comes before
/// the first), the key the contents and a link name it by, and its HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub heading: Option<String>,
    pub key: String,
    pub html: String,
}

/// The first character a text holds that hides or reorders words, or none.
pub fn hidden(text: &str) -> Option<meridian_domain::text::Refused> {
    // A newline and a tab are kept, and a carriage return is a line's end
    // as a file on another system writes it.
    meridian_domain::text::refused(&text.replace('\r', ""))
}

/// What a text shows: every character that hides or reorders words, and
/// every control character but a newline and a tab, left out.
fn visible(text: &str) -> String {
    text.chars()
        .filter(|c| meridian_domain::text::refused_kind(*c).is_none())
        .collect()
}

/// The text as the screen reads it (W6.20's marking): what is visible, with
/// each tag character read as the letter it shadows, so an instruction
/// spelled in them is matched as one.
pub fn screened_text(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c as u32 {
            0xE0020..=0xE007E => char::from_u32(c as u32 - 0xE0000),
            _ if meridian_domain::text::refused_kind(c).is_some() && c != '\r' => None,
            _ => Some(c),
        })
        .collect()
}

/// Where a link to a section of the same bundle goes, at a part's key.
pub type SectionHref<'a> = &'a dyn Fn(&str, &str) -> String;

/// A section drawn as its parts. `present` names the bundle's sections a
/// link may open; `href` says where one is.
pub fn render(markdown: &str, present: &[&str], href: SectionHref) -> Vec<Part> {
    let mut writer = Writer {
        present,
        href,
        parts: Vec::new(),
        current: Part {
            heading: None,
            key: String::new(),
            html: String::new(),
        },
        sinks: Vec::new(),
        links: Vec::new(),
        image: 0,
        heading: None,
        head_cells: false,
    };
    for event in Parser::new_ext(markdown, Options::ENABLE_TABLES) {
        writer.event(event);
    }
    writer.finish()
}

/// What a link becomes.
enum Target {
    /// Out of the deployment: the address as written back, and its host.
    Outside { href: String, host: String },
    /// Another section of the bundle, at a heading.
    Section { href: String },
    /// Neither: its text alone.
    Plain,
}

struct Writer<'a> {
    present: &'a [&'a str],
    href: SectionHref<'a>,
    parts: Vec<Part>,
    current: Part,
    /// Buffers the inline content of a heading or a link is written into.
    sinks: Vec<String>,
    links: Vec<Target>,
    /// Inside an image's alt text, how deep.
    image: usize,
    /// A level-one or level-two heading's plain text, while inside one.
    heading: Option<(String, &'static str)>,
    head_cells: bool,
}

impl Writer<'_> {
    fn out(&mut self) -> &mut String {
        match self.sinks.last_mut() {
            Some(sink) => sink,
            None => &mut self.current.html,
        }
    }

    fn text(&mut self, text: &str) {
        let shown = escape(&visible(text));
        if let Some((plain, _)) = self.heading.as_mut() {
            plain.push_str(&visible(text));
        }
        self.out().push_str(&shown);
    }

    fn event(&mut self, event: Event) {
        if self.image > 0 {
            // An image's alt text, as text alone.
            match event {
                Event::Start(Tag::Image { .. }) => self.image += 1,
                Event::End(TagEnd::Image) => {
                    self.image -= 1;
                    if self.image == 0 {
                        let alt = self.sinks.pop().unwrap_or_default();
                        let alt = alt.trim();
                        let said = if alt.is_empty() {
                            "[image not shown]".to_string()
                        } else {
                            format!("[image not shown: {alt}]")
                        };
                        self.out().push_str(&format!(
                            "<span class=\"doc-image\" title=\"Images are never fetched\">{said}</span>"
                        ));
                    }
                }
                Event::Text(text) | Event::Code(text) => {
                    let shown = escape(&visible(&text));
                    self.out().push_str(&shown);
                }
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => {
                let shown = format!("<code>{}</code>", escape(&visible(&text)));
                if let Some((plain, _)) = self.heading.as_mut() {
                    plain.push_str(&visible(&text));
                }
                self.out().push_str(&shown);
            }
            // Raw HTML, shown as the literal text it is.
            Event::Html(text) | Event::InlineHtml(text) => self.text(&text),
            Event::SoftBreak => self.out().push('\n'),
            Event::HardBreak => self.out().push_str("<br>"),
            Event::Rule => self.out().push_str("<hr>"),
            // Not enabled; written as text if ever met.
            Event::FootnoteReference(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.text(&text)
            }
            Event::TaskListMarker(done) => self.text(if done { "[x] " } else { "[ ] " }),
        }
    }

    fn start(&mut self, tag: Tag) {
        let opened = match tag {
            Tag::Paragraph => "<p>".to_string(),
            Tag::Heading { level, .. } => match level {
                HeadingLevel::H1 | HeadingLevel::H2 => {
                    self.cut();
                    self.heading = Some((String::new(), "h2"));
                    self.sinks.push(String::new());
                    return;
                }
                HeadingLevel::H3 => "<h3>".into(),
                _ => "<h4>".into(),
            },
            Tag::BlockQuote(_) => "<blockquote>".into(),
            Tag::CodeBlock(CodeBlockKind::Fenced(_) | CodeBlockKind::Indented) => {
                "<pre><code>".into()
            }
            Tag::HtmlBlock => "<p class=\"doc-raw\">".into(),
            Tag::List(Some(start)) => format!("<ol start=\"{start}\">"),
            Tag::List(None) => "<ul>".into(),
            Tag::Item => "<li>".into(),
            Tag::Table(_) => "<div class=\"doc-table\"><table>".into(),
            Tag::TableHead => {
                self.head_cells = true;
                "<thead><tr>".into()
            }
            Tag::TableRow => "<tr>".into(),
            Tag::TableCell => {
                if self.head_cells {
                    "<th>".into()
                } else {
                    "<td>".into()
                }
            }
            Tag::Emphasis => "<em>".into(),
            Tag::Strong => "<strong>".into(),
            Tag::Link {
                link_type,
                dest_url,
                ..
            } => {
                let target = self.target(link_type, &dest_url);
                self.links.push(target);
                self.sinks.push(String::new());
                return;
            }
            Tag::Image { .. } => {
                self.image = 1;
                self.sinks.push(String::new());
                return;
            }
            // Not enabled: their content is drawn, the element is not.
            _ => return,
        };
        self.out().push_str(&opened);
    }

    fn end(&mut self, tag: TagEnd) {
        let closed = match tag {
            TagEnd::Paragraph => "</p>",
            TagEnd::Heading(level) => match level {
                HeadingLevel::H1 | HeadingLevel::H2 => {
                    let inner = self.sinks.pop().unwrap_or_default();
                    let (plain, element) = self.heading.take().unwrap_or_default();
                    let plain = plain.split_whitespace().collect::<Vec<_>>().join(" ");
                    self.current.key = self.key(&plain);
                    self.current
                        .html
                        .push_str(&format!("<{element}>{inner}</{element}>"));
                    self.current.heading = Some(plain);
                    return;
                }
                HeadingLevel::H3 => "</h3>",
                _ => "</h4>",
            },
            TagEnd::BlockQuote(_) => "</blockquote>",
            TagEnd::CodeBlock => "</code></pre>",
            TagEnd::HtmlBlock => "</p>",
            TagEnd::List(true) => "</ol>",
            TagEnd::List(false) => "</ul>",
            TagEnd::Item => "</li>",
            TagEnd::Table => "</tbody></table></div>",
            TagEnd::TableHead => {
                self.head_cells = false;
                "</tr></thead><tbody>"
            }
            TagEnd::TableRow => "</tr>",
            TagEnd::TableCell => {
                if self.head_cells {
                    "</th>"
                } else {
                    "</td>"
                }
            }
            TagEnd::Emphasis => "</em>",
            TagEnd::Strong => "</strong>",
            TagEnd::Link => {
                let inner = self.sinks.pop().unwrap_or_default();
                let drawn = match self.links.pop().unwrap_or(Target::Plain) {
                    Target::Outside { href, host } => format!(
                        "<a class=\"doc-out\" href=\"{href}\" target=\"_blank\" \
                         rel=\"noopener noreferrer nofollow\">{inner}</a>\
                         <span class=\"doc-host\"> ({host})</span>\
                         <span class=\"doc-leaves\" role=\"img\" aria-label=\"leaves this deployment\" \
                         title=\"Leaves this deployment\">&#8599;</span>",
                        href = escape(&href),
                        host = escape(&host),
                    ),
                    Target::Section { href } => {
                        format!("<a class=\"doc-section\" href=\"{}\">{inner}</a>", escape(&href))
                    }
                    Target::Plain => inner,
                };
                self.out().push_str(&drawn);
                return;
            }
            _ => return,
        };
        self.out().push_str(closed);
    }

    /// The part so far ends here, at a level-one or level-two heading; one
    /// with nothing in it before the first heading is dropped.
    fn cut(&mut self) {
        let next = Part {
            heading: None,
            key: String::new(),
            html: String::new(),
        };
        let done = std::mem::replace(&mut self.current, next);
        if done.heading.is_some() || !done.html.trim().is_empty() {
            self.parts.push(done);
        }
    }

    /// A heading's key: its slug, unique in the section.
    fn key(&self, plain: &str) -> String {
        let key = crate::area::slug(plain);
        let taken = |k: &str| self.parts.iter().any(|part| part.key == k);
        if !taken(&key) {
            return key;
        }
        (2..)
            .map(|n| format!("{key}-{n}"))
            .find(|k| !taken(k))
            .expect("an unused key")
    }

    fn finish(mut self) -> Vec<Part> {
        self.cut();
        self.parts
    }

    fn target(&self, link_type: LinkType, dest: &str) -> Target {
        // An email autolink names the address alone.
        let dest = if link_type == LinkType::Email {
            format!("mailto:{dest}")
        } else {
            dest.to_string()
        };
        // What a browser would read the scheme as: no whitespace or control
        // character, whatever case.
        let bare: String = dest
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_control())
            .collect::<String>()
            .to_ascii_lowercase();
        let outside = ["https:", "http:", "mailto:"]
            .iter()
            .any(|scheme| bare.starts_with(scheme));
        if outside {
            return match reqwest::Url::parse(dest.trim()) {
                Ok(url) if url.scheme() == "mailto" => {
                    let address = url.path().to_string();
                    let host = address.rsplit('@').next().unwrap_or_default().to_string();
                    if host.is_empty() || address == host {
                        Target::Plain
                    } else {
                        Target::Outside {
                            href: url.to_string(),
                            host,
                        }
                    }
                }
                Ok(url) if matches!(url.scheme(), "https" | "http") => match url.host_str() {
                    Some(host) if !host.is_empty() => Target::Outside {
                        host: host.to_string(),
                        href: url.to_string(),
                    },
                    _ => Target::Plain,
                },
                _ => Target::Plain,
            };
        }
        // Another section of the bundle: `setup.md`, `./setup.md`, with or
        // without `#heading`; nothing else relative is kept.
        let relative = dest.trim().trim_start_matches("./");
        let (file, heading) = relative.split_once('#').unwrap_or((relative, ""));
        match file.strip_suffix(".md") {
            Some(section) if self.present.contains(&section) => {
                let key = if heading.is_empty() {
                    String::new()
                } else {
                    crate::area::slug(heading)
                };
                Target::Section {
                    href: (self.href)(section, &key),
                }
            }
            _ => Target::Plain,
        }
    }
}

#[cfg(test)]
mod tests;
