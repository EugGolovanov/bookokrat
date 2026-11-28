// Adapted from Rustpub's img.rs and templates/introduction.html (MIT).
use std::collections::hash_map::{Entry, HashMap};
use std::fmt::Write;
use std::io::{Cursor, Read};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use epub_builder::{EpubBuilder, ZipLibrary};
use html5ever::tendril::TendrilSink;
use html5ever::{Attribute, LocalName, parse_document};
use image::{ImageFormat, ImageReader};
use markup5ever_rcdom::{Handle, NodeData, RcDom};
use url::Url;

const IMAGE_REQUESTS: usize = 24;
const IMAGE_BYTES: usize = 4 * 1024 * 1024;
const TOTAL_IMAGE_BYTES: usize = 24 * 1024 * 1024;

fn web_url(base: &Url, value: &str) -> Option<Url> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let url = base.join(value).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none())
    .then_some(url)
}

struct Images<'a> {
    agent: &'a ureq::Agent,
    book: &'a mut EpubBuilder<ZipLibrary>,
    paths: HashMap<Url, Option<String>>,
    bytes_left: usize,
    deadline: Instant,
}

impl Images<'_> {
    fn local_path(&mut self, base: &Url, src: &str) -> Result<Option<&str>> {
        let Some(mut url) = web_url(base, src) else {
            return Ok(None);
        };
        // Article markup must not direct background requests to other services.
        if url.scheme() != base.scheme()
            || url.host_str() != base.host_str()
            || url.port() != base.port()
        {
            return Ok(None);
        }
        url.set_fragment(None);
        let n = self.paths.len();
        let path = match self.paths.entry(url) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let left = self.deadline.saturating_duration_since(Instant::now());
                if n == IMAGE_REQUESTS || self.bytes_left == 0 || left.is_zero() {
                    return Ok(None);
                }
                let image = download_image(self.agent, entry.key(), left, &mut self.bytes_left);
                let path = if let Some((bytes, ext, mime)) = image {
                    let path = format!("images/{n}.{ext}");
                    self.book.add_resource(&path, bytes.as_slice(), mime)?;
                    Some(path)
                } else {
                    None
                };
                entry.insert(path)
            }
        };
        Ok(path.as_deref())
    }
}

fn download_image(
    agent: &ureq::Agent,
    url: &Url,
    time_left: Duration,
    bytes_left: &mut usize,
) -> Option<(Vec<u8>, &'static str, &'static str)> {
    let mut response = agent
        .get(url.as_str())
        .config()
        .max_redirects(0)
        .timeout_global(Some(time_left.min(Duration::from_secs(5))))
        .build()
        .call()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    // Charge even rejected and failed reads against the article's download budget.
    let limit = (*bytes_left).min(IMAGE_BYTES + 1);
    let mut bytes = Vec::new();
    let result = response
        .body_mut()
        .as_reader()
        .take(limit as u64)
        .read_to_end(&mut bytes);
    *bytes_left -= bytes.len();
    result.ok()?;
    if bytes.len() == limit {
        return None;
    }
    let format = image::guess_format(&bytes).ok()?;
    let ext = match format {
        ImageFormat::Png => "png",
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Gif => "gif",
        ImageFormat::WebP => "webp",
        _ => return None,
    };
    let (w, h) = ImageReader::with_format(Cursor::new(&bytes), format)
        .into_dimensions()
        .ok()?;
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > 32_000_000 {
        return None;
    }
    Some((bytes, ext, format.to_mime_type()))
}

fn attr<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|a| a.name.ns.is_empty() && a.name.local.as_ref() == name)
        .map(|a| a.value.as_ref())
}

// XML 1.0 excludes some characters that an HTML parser may return as text.
fn escape(value: &str, out: &mut String) -> bool {
    let mut has_text = false;
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' | ' '..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'.. => {
                out.push(c)
            }
            _ => continue,
        }
        has_text = has_text || !c.is_whitespace();
    }
    has_text
}

pub(super) fn build(
    agent: &ureq::Agent,
    base: &Url,
    title: &str,
    content: &str,
    book: &mut EpubBuilder<ZipLibrary>,
) -> Result<Vec<u8>> {
    let dom = parse_document(RcDom::default(), Default::default()).one(content);
    let mut images = Images {
        agent,
        book,
        paths: HashMap::new(),
        bytes_left: TOTAL_IMAGE_BYTES,
        deadline: Instant::now() + Duration::from_secs(30),
    };
    let template = include_str!("introduction.xhtml");
    let (head, body) = template
        .split_once("{{ title }}")
        .expect("Rustpub title slot");
    let (body, end) = body
        .split_once("{{ content }}")
        .expect("Rustpub content slot");
    let mut out = String::with_capacity(template.len() + title.len() + content.len());
    out.push_str(head);
    escape(title, &mut out);
    out.push_str(body);

    enum Event {
        Node(Handle),
        Close(LocalName),
    }
    let mut stack = vec![Event::Node(dom.document)];
    let mut has_body_text = false;
    while let Some(event) = stack.pop() {
        let node = match event {
            Event::Node(node) => node,
            Event::Close(name) => {
                out.push_str("</");
                out.push_str(&name);
                out.push('>');
                continue;
            }
        };
        match &node.data {
            NodeData::Document => (),
            NodeData::Text { contents } => {
                has_body_text |= escape(&contents.borrow(), &mut out);
            }
            NodeData::Element { name, attrs, .. } => {
                let attrs = attrs.borrow();
                if name.ns.as_ref() != "http://www.w3.org/1999/xhtml"
                    || attr(&attrs, "hidden").is_some()
                {
                    continue;
                }
                let tag = name.local.as_ref();
                match tag {
                    "head" | "title" | "script" | "style" | "template" | "noscript" | "iframe"
                    | "object" | "embed" | "canvas" | "form" | "button" | "textarea" | "select"
                    | "audio" | "video" => continue,
                    "img" => {
                        let alt = attr(&attrs, "alt").unwrap_or_default();
                        if let Some(src) =
                            images.local_path(base, attr(&attrs, "src").unwrap_or_default())?
                        {
                            out.push_str("<img src=\"");
                            out.push_str(src);
                            out.push_str("\" alt=\"");
                            has_body_text |= escape(alt, &mut out);
                            out.push_str("\" />");
                        } else {
                            has_body_text |= escape(alt, &mut out);
                        }
                        continue;
                    }
                    "br" | "hr" | "wbr" => {
                        out.push('<');
                        out.push_str(tag);
                        out.push_str(" />");
                    }
                    "a" | "abbr" | "article" | "aside" | "b" | "blockquote" | "caption"
                    | "cite" | "code" | "dd" | "del" | "div" | "dl" | "dt" | "em"
                    | "figcaption" | "figure" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "i"
                    | "ins" | "kbd" | "li" | "mark" | "ol" | "p" | "pre" | "q" | "s" | "samp"
                    | "section" | "small" | "span" | "strong" | "sub" | "sup" | "table"
                    | "tbody" | "td" | "th" | "thead" | "tfoot" | "time" | "tr" | "u" | "ul"
                    | "var" => {
                        out.push('<');
                        out.push_str(tag);
                        if let Some(id) = attr(&attrs, super::ANCHOR_ATTR)
                            .or_else(|| (tag == "a").then(|| attr(&attrs, "name")).flatten())
                            .filter(|id| !id.is_empty() && !id.chars().any(char::is_whitespace))
                        {
                            out.push_str(" id=\"");
                            escape(id, &mut out);
                            out.push('"');
                        }
                        if matches!(tag, "th" | "td") {
                            for key in ["rowspan", "colspan"] {
                                if let Some(n) =
                                    attr(&attrs, key).and_then(|s| s.parse::<u16>().ok())
                                    && n <= 1000
                                    && (n > 0 || key == "rowspan")
                                {
                                    write!(out, " {key}=\"{n}\"")?;
                                }
                            }
                        }
                        if tag == "a"
                            && let Some(url) = attr(&attrs, "href").and_then(|s| web_url(base, s))
                        {
                            out.push_str(" href=\"");
                            let href = url
                                .as_str()
                                .strip_prefix(base.as_str())
                                .filter(|suffix| suffix.starts_with('#'))
                                .unwrap_or(url.as_str());
                            escape(href, &mut out);
                            out.push('"');
                        }
                        out.push('>');
                        stack.push(Event::Close(name.local.clone()));
                    }
                    _ => (),
                }
            }
            _ => continue,
        }
        stack.extend(node.children.take().into_iter().rev().map(Event::Node));
    }
    ensure!(has_body_text, "article contains no readable text");
    out.push_str(end);
    Ok(out.into_bytes())
}
