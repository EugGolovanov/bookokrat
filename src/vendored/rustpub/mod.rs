// Adapted from RustPub's native parser and EPUB builder (src/parse.rs and src/epub.rs):
// https://github.com/gicrisf/rustpub/tree/eb1753257b8fa76337004f27db7b97639fbfb857

mod chapter;
mod public_images;

use std::{fs::File, time::Duration};

use anyhow::{Context, Result, ensure};
use epub_builder::{EpubBuilder, EpubContent, EpubVersion, ReferenceType, ZipLibrary};
use html5ever::tree_builder::{NodeOrText, TreeSink};
use html5ever::{Attribute, QualName, parse_document, serialize, tendril::TendrilSink};
use markup5ever_rcdom::{Handle, Node, NodeData, RcDom, SerializableHandle};
use ureq::{Agent, ResponseExt};
use url::Url;

const MAX_HTML_BYTES: u64 = 4 * 1024 * 1024;
const ANCHOR_ATTR: &str = "data-bookokrat-anchor";

pub(crate) fn convert(url: &Url, output: &mut File) -> Result<()> {
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .https_only(url.scheme() == "https")
        .max_redirects(5)
        .build()
        .into();
    let mut response = agent
        .get(url.as_str())
        .header("Accept", "text/html, application/xhtml+xml")
        .call()
        .context("fetching website")?;
    ensure!(
        response.status().is_success(),
        "website returned HTTP {}",
        response.status()
    );
    let base = Url::parse(&response.get_uri().to_string()).context("reading final website URL")?;
    let has_content_type = response.body().mime_type().is_some();
    if let Some(mime) = response.body().mime_type() {
        ensure!(
            mime.eq_ignore_ascii_case("text/html")
                || mime.eq_ignore_ascii_case("application/xhtml+xml"),
            "expected an HTML page, got {mime}"
        );
    }
    let html = response
        .body_mut()
        .with_config()
        .limit(MAX_HTML_BYTES)
        .read_to_string()
        .context("reading website HTML (4 MiB limit)")?;
    ensure!(
        has_content_type
            || html
                .trim_start_matches('\u{feff}')
                .trim_start()
                .starts_with('<'),
        "response does not appear to be HTML"
    );
    let (meta, article) = PageMeta::extract(&html, &base)?;
    ensure!(
        !article.text.trim().is_empty(),
        "website has no readable article text"
    );
    let title = meta
        .og_title
        .or(meta.twitter_title)
        .or_else(|| xml_text(&article.title))
        .unwrap_or_else(|| base.host_str().unwrap_or(base.as_str()).to_owned());

    let mut book = EpubBuilder::new(ZipLibrary::new()?)?;
    book.epub_version(EpubVersion::V30)
        .metadata("title", title.as_str())?
        .metadata("lang", meta.language.as_deref().unwrap_or("und"))?
        .metadata("description", format!("Source: {base}"))?
        .metadata("generator", "Bookokrat (RustPub)")?
        .stylesheet("img { max-width: 100%; height: auto; }".as_bytes())?;
    if let Some(author) = meta.og_author.or(meta.author) {
        book.metadata("author", author)?;
    }
    let xhtml = chapter::build(&agent, &base, &title, &article.content, &mut book)?;
    book.add_content(
        EpubContent::new("document.xhtml", xhtml.as_slice())
            .title(title)
            .reftype(ReferenceType::Text),
    )?;
    book.generate(output).context("writing website EPUB")
}

#[derive(Default)]
struct PageMeta {
    og_title: Option<String>,
    twitter_title: Option<String>,
    og_author: Option<String>,
    author: Option<String>,
    language: Option<String>,
    changed_dom: bool,
}

impl PageMeta {
    fn extract(html: &str, base: &Url) -> Result<(Self, readability::extractor::Product)> {
        let mut dom = parse_document(RcDom::default(), Default::default())
            .from_utf8()
            .read_from(&mut html.as_bytes())?;
        let mut meta = Self::default();
        let mut document = dom.document.clone();
        meta.visit(&mut dom, &mut document, 0, false)?;
        drop(dom);
        let mut annotated = Vec::new();
        let mut input = if meta.changed_dom {
            serialize(
                &mut annotated,
                &SerializableHandle::from(document),
                Default::default(),
            )?;
            annotated.as_slice()
        } else {
            html.as_bytes()
        };
        let article = readability::extractor::extract(&mut input, base)
            .context("extracting readable article")?;
        Ok((meta, article))
    }

    fn visit(
        &mut self,
        dom: &mut RcDom,
        node: &mut Handle,
        depth: usize,
        mut in_figure: bool,
    ) -> Result<()> {
        // Readability recursively traverses the DOM; reject excessive nesting before extraction.
        ensure!(depth <= 128, "website HTML is too deeply nested");
        if let NodeData::Element { name, attrs, .. } = &node.data {
            if name.local.as_ref() == "img" && name.ns.as_ref() == "http://www.w3.org/1999/xhtml" {
                self.changed_dom |= prepare_image(dom, node, in_figure);
                return Ok(());
            }
            let mut attrs = attrs.borrow_mut();
            let attr = |key: &str| attr(&attrs, key);
            match name.local.as_ref() {
                "html" => self.language = attr("lang").and_then(xml_text),
                "meta" => {
                    let slot = match (attr("property"), attr("name")) {
                        (Some("og:title"), _) => Some(&mut self.og_title),
                        (_, Some("twitter:title")) => Some(&mut self.twitter_title),
                        (Some("og:author"), _) => Some(&mut self.og_author),
                        (_, Some("author")) => Some(&mut self.author),
                        _ => None,
                    };
                    if let Some(slot) = slot
                        && slot.is_none()
                    {
                        *slot = attr("content").and_then(xml_text);
                    }
                }
                "figure" => in_figure = true,
                "p" | "figcaption" => in_figure = false,
                _ => {}
            }
            // Readability uses IDs for scoring, then strips them; retain offline link targets.
            if let Some(mut anchor) = attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "id")
                .cloned()
            {
                anchor.name.local = ANCHOR_ATTR.into();
                attrs.retain(|a| a.name.local.as_ref() != ANCHOR_ATTR);
                attrs.push(anchor);
                self.changed_dom = true;
            }
        }
        let mut children = node.children.borrow_mut();
        children.retain(|child| {
            let skip = excluded(child);
            self.changed_dom |= skip;
            !skip
        });
        for child in children.iter_mut() {
            self.visit(dom, child, depth + 1, in_figure)?;
        }
        Ok(())
    }
}

fn attr<'a>(attrs: &'a [Attribute], key: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|a| a.name.local.as_ref() == key)
        .map(|a| a.value.as_ref())
}

fn excluded(node: &Handle) -> bool {
    let NodeData::Element { name, attrs, .. } = &node.data else {
        return false;
    };
    matches!(
        name.local.as_ref(),
        "nav" | "script" | "template" | "noscript" | "style" | "button" | "form"
    ) || attrs.borrow().iter().any(|a| match a.name.local.as_ref() {
        "hidden" => true,
        "role" => a.value.eq_ignore_ascii_case("navigation"),
        "class" => a.value.split_ascii_whitespace().any(|c| c == "hidden"),
        "style" => a
            .value
            .split(';')
            .filter_map(|s| s.split_once(':'))
            .any(|(k, v)| {
                let value = v.split('!').next().unwrap_or_default().trim();
                (k.trim().eq_ignore_ascii_case("display") && value.eq_ignore_ascii_case("none"))
                    || (k.trim().eq_ignore_ascii_case("visibility")
                        && value.eq_ignore_ascii_case("hidden"))
            }),
        _ => false,
    })
}

fn prepare_image(dom: &mut RcDom, node: &mut Handle, in_figure: bool) -> bool {
    let NodeData::Element { name, attrs, .. } = &node.data else {
        return false;
    };
    let mut attrs = attrs.borrow_mut();
    let lazy = ["data-src", "data-original"].iter().find_map(|key| {
        attrs
            .iter()
            .position(|a| a.name.local.as_ref() == *key && !a.value.trim().is_empty())
    });
    if let Some(i) = lazy {
        // Keep the publisher's equation text without spending a figure download on a GIF.
        if attr(&attrs, "role").is_some_and(|v| v.eq_ignore_ascii_case("math"))
            && let Some(alt) = attrs
                .iter()
                .find(|a| a.name.local.as_ref() == "alt" && !a.value.trim().is_empty())
        {
            let text = Node::new(NodeData::Text {
                contents: alt.value.clone().into(),
            });
            drop(attrs);
            text.parent.set(node.parent.take());
            *node = text;
            return true;
        }
        let mut src = attrs.swap_remove(i);
        src.name.local = "src".into();
        attrs.retain(|a| a.name.local.as_ref() != "src");
        attrs.push(src);
    }
    if in_figure {
        // Readability drops image-only divs when their images outnumber their paragraphs.
        let paragraph = dom.create_element(
            QualName::new(None, name.ns.clone(), "p".into()),
            Vec::new(),
            Default::default(),
        );
        drop(attrs);
        paragraph.parent.set(node.parent.take());
        let image = std::mem::replace(node, paragraph);
        dom.append(node, NodeOrText::AppendNode(image));
        return true;
    }
    lazy.is_some()
}

fn xml_text(value: &str) -> Option<String> {
    let mut text = value.trim().to_owned();
    text.retain(|c| matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}'));
    (!text.is_empty()).then_some(text)
}
