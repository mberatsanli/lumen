//! Browser orchestration: sessions, navigation and history.
//!
//! A [`Session`] owns a resource loader and a viewport, fetches documents
//! and runs them through the engine pipeline. Navigation is deliberately
//! separate from rendering: the engine knows nothing about URLs, and this
//! crate knows nothing about painting beyond handing back a [`Page`].

use lumen_engine::{
    HeuristicMeasurer, ImageMap, Page, RasterImage, Size, TextMeasurer, collect_author_css,
    collect_image_sources, page_from_document,
};
use lumen_html::NodeId;
use lumen_platform::{LoadError, ResourceLoader, ResourceRequest, Url, resolve};
use std::sync::Arc;

/// One browsing context with linear history.
///
/// History entries are re-fetched on `back`/`forward`/`refresh`; there is
/// no document cache yet. All responses are treated as HTML.
pub struct Session<L: ResourceLoader> {
    loader: L,
    viewport: Size,
    measurer: Box<dyn TextMeasurer + Send>,
    history: Vec<Url>,
    /// Index of the current entry in `history`, if any page is loaded.
    index: Option<usize>,
    page: Option<Page>,
    /// HTML source of the current page, kept so viewport or measurer
    /// changes can relayout locally without hitting the network.
    source: Option<String>,
    /// Author stylesheet (embedded + external), fetched once per page and
    /// shared with relayouts through the `Arc`.
    author: Arc<lumen_css::Stylesheet>,
    /// Decoded images, fetched once per page, shared the same way.
    images: Arc<ImageMap>,
    /// Node currently under the pointer, for `:hover` styling.
    hovered: Option<NodeId>,
    /// How the current stylesheet's hover rules can affect the page —
    /// picks the cheapest reaction to hover changes.
    hover_impact: lumen_engine::HoverImpact,
}

impl<L: ResourceLoader> Session<L> {
    #[must_use]
    pub fn new(loader: L, viewport: Size) -> Self {
        Self {
            loader,
            viewport,
            measurer: Box::new(HeuristicMeasurer),
            history: Vec::new(),
            index: None,
            page: None,
            source: None,
            author: Arc::new(lumen_css::Stylesheet::default()),
            images: Arc::new(ImageMap::new()),
            hovered: None,
            hover_impact: lumen_engine::HoverImpact::Nothing,
        }
    }

    /// Replaces the text measurer (e.g. with real font metrics) and
    /// relayouts the current page if one is loaded. No network access.
    pub fn set_measurer(&mut self, measurer: Box<dyn TextMeasurer + Send>) {
        self.measurer = measurer;
        self.relayout();
    }

    /// Navigates to `url`: fetches, renders, pushes a history entry and
    /// drops any forward entries.
    pub fn load(&mut self, url: Url) -> Result<&Page, LoadError> {
        let final_url = self.fetch_and_render(url)?;
        if let Some(index) = self.index {
            self.history.truncate(index + 1);
        }
        self.history.push(final_url);
        self.index = Some(self.history.len() - 1);
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Re-fetches the current entry.
    pub fn refresh(&mut self) -> Result<&Page, LoadError> {
        let url = self.require_current()?;
        self.fetch_and_render(url)?;
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Goes one entry back, if possible.
    pub fn back(&mut self) -> Result<&Page, LoadError> {
        let index = self
            .index
            .filter(|index| *index > 0)
            .ok_or_else(|| LoadError::InvalidUrl("no earlier history entry".to_string()))?;
        self.fetch_and_render(self.history[index - 1].clone())?;
        self.index = Some(index - 1);
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Goes one entry forward, if possible.
    pub fn forward(&mut self) -> Result<&Page, LoadError> {
        let index = self
            .index
            .filter(|index| index + 1 < self.history.len())
            .ok_or_else(|| LoadError::InvalidUrl("no later history entry".to_string()))?;
        self.fetch_and_render(self.history[index + 1].clone())?;
        self.index = Some(index + 1);
        Ok(self.page.as_ref().expect("fetch_and_render set the page"))
    }

    /// Resolves a (possibly relative) `href` against the current page and
    /// navigates to it.
    pub fn follow(&mut self, href: &str) -> Result<&Page, LoadError> {
        let base = self.require_current()?;
        let url = resolve(&base, href)?;
        self.load(url)
    }

    /// Changes the viewport and lays the current page out again from the
    /// cached source. No network access, so it is safe to call on every
    /// window resize event.
    pub fn set_viewport(&mut self, viewport: Size) {
        self.viewport = viewport;
        self.relayout();
    }

    /// Updates the hovered node for `:hover` styling. Returns whether the
    /// page was restyled (only when the hovered node actually changed).
    /// No network access.
    pub fn set_hovered(&mut self, node: Option<NodeId>) -> bool {
        if self.hovered == node {
            return false;
        }
        self.hovered = node;
        match self.hover_impact {
            // No hover rules: styles cannot change, nothing to redraw.
            lumen_engine::HoverImpact::Nothing => false,
            // Paint-only hover rules: swap styles + rebuild the display
            // list on the existing layout — no relayout.
            lumen_engine::HoverImpact::PaintOnly => match &mut self.page {
                Some(page) => {
                    lumen_engine::repaint_page_for_hover(page, self.hovered);
                    true
                }
                None => false,
            },
            lumen_engine::HoverImpact::Layout => {
                self.relayout();
                true
            }
        }
    }

    /// The nearest `<a href>` at or above `node`, for link hit testing.
    #[must_use]
    pub fn link_target(&self, node: NodeId) -> Option<String> {
        let document = &self.page.as_ref()?.document;
        std::iter::once(node)
            .chain(document.ancestors(node))
            .find_map(|candidate| {
                let element = document.element(candidate)?;
                if element.tag_name == "a" {
                    element.attributes.get("href").map(str::to_string)
                } else {
                    None
                }
            })
    }

    fn relayout(&mut self) {
        // Reuse the page's document: it already carries materialized
        // ::before/::after nodes, so hover ids (which may point at
        // generated content) stay valid. Fall back to reparsing the
        // cached source when there is no page yet.
        let document = match self.page.take() {
            Some(page) => page.document,
            None => match &self.source {
                Some(source) => lumen_html::parse_document(source),
                None => return,
            },
        };
        self.page = Some(page_from_document(
            document,
            self.author.clone(),
            self.images.clone(),
            self.viewport,
            self.measurer.as_ref(),
            self.hovered,
        ));
    }

    #[must_use]
    pub fn page(&self) -> Option<&Page> {
        self.page.as_ref()
    }

    /// The text of the page's `<title>` element, when present.
    #[must_use]
    pub fn title(&self) -> Option<String> {
        let page = self.page()?;
        let document = &page.document;
        let title = document.descendants(document.root()).find(|id| {
            document
                .element(*id)
                .is_some_and(|element| element.tag_name == "title")
        })?;
        let text: String = document
            .children(title)
            .iter()
            .filter_map(|child| match &document.node(*child).kind {
                lumen_html::NodeKind::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let text = text.trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    pub fn current_url(&self) -> Option<&Url> {
        self.index.map(|index| &self.history[index])
    }

    #[must_use]
    pub fn can_go_back(&self) -> bool {
        self.index.is_some_and(|index| index > 0)
    }

    #[must_use]
    pub fn can_go_forward(&self) -> bool {
        self.index
            .is_some_and(|index| index + 1 < self.history.len())
    }

    fn require_current(&self) -> Result<Url, LoadError> {
        self.current_url()
            .cloned()
            .ok_or_else(|| LoadError::InvalidUrl("no page loaded".to_string()))
    }

    /// Fetches `url` and rebuilds the page. Returns the final URL after
    /// redirects (which is what history should record).
    fn fetch_and_render(&mut self, url: Url) -> Result<Url, LoadError> {
        let response = self.loader.load(&ResourceRequest { url })?;
        let source = response.text();
        let document = lumen_html::parse_document(&source);

        // External stylesheets: resolved against the final URL, fetched in
        // document order; failures skip that sheet without failing the page.
        let base = response.final_url.clone();
        let author_css = collect_author_css(&document, |href| {
            let url = resolve(&base, href).ok()?;
            self.loader
                .load(&ResourceRequest { url })
                .ok()
                .map(|response| response.text())
        });
        self.author = Arc::new(lumen_css::parse_stylesheet(&author_css));
        self.hover_impact = lumen_engine::hover_impact(&self.author);

        // Images: fetched once per page; failures leave a placeholder box.
        // Capped so image-heavy pages cannot stall navigation for minutes.
        const MAX_IMAGES_PER_PAGE: usize = 32;
        let mut images = ImageMap::new();
        // CSS background images need computed styles to discover; this
        // extra style pass runs at load only.
        let mut sources = collect_image_sources(&document);
        let styles = lumen_engine::compute_styles(&document, &self.author);
        let mut backgrounds: Vec<(NodeId, String)> = styles
            .by_node
            .iter()
            .filter_map(|(node, style)| match &style.background_image {
                Some(lumen_engine::BackgroundImage::Url(src)) => Some((*node, src.clone())),
                _ => None,
            })
            .collect();
        backgrounds.sort_unstable();
        sources.extend(backgrounds);
        for (node, src) in sources.into_iter().take(MAX_IMAGES_PER_PAGE) {
            let Ok(url) = resolve(&base, &src) else {
                continue;
            };
            if let Ok(response) = self.loader.load(&ResourceRequest { url })
                && let Some(image) = RasterImage::decode(&response.body)
            {
                images.insert(node, Arc::new(image));
            }
        }
        self.images = Arc::new(images);

        self.hovered = None; // New document, new node ids.
        self.page = Some(page_from_document(
            document,
            self.author.clone(),
            self.images.clone(),
            self.viewport,
            self.measurer.as_ref(),
            None,
        ));
        self.source = Some(source);
        Ok(response.final_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_platform::ResourceResponse;
    use std::cell::RefCell;
    use std::collections::HashMap;

    struct FakeLoader {
        pages: HashMap<String, Vec<u8>>,
        loads: RefCell<Vec<String>>,
    }

    impl FakeLoader {
        fn new(pages: &[(&str, &str)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(url, html)| ((*url).to_string(), html.as_bytes().to_vec()))
                    .collect(),
                loads: RefCell::new(Vec::new()),
            }
        }

        fn with_bytes(mut self, url: &str, bytes: &[u8]) -> Self {
            self.pages.insert(url.to_string(), bytes.to_vec());
            self
        }
    }

    impl ResourceLoader for FakeLoader {
        fn load(&self, request: &ResourceRequest) -> Result<ResourceResponse, LoadError> {
            self.loads.borrow_mut().push(request.url.to_string());
            let body = self
                .pages
                .get(request.url.as_str())
                .ok_or_else(|| LoadError::Http(format!("404: {}", request.url)))?;
            Ok(ResourceResponse {
                final_url: request.url.clone(),
                content_type: None,
                body: body.clone(),
            })
        }
    }

    /// Encodes a 1×1 red PNG in memory.
    fn red_pixel_png() -> Vec<u8> {
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .expect("in-memory png encode");
        bytes.into_inner()
    }

    const VIEWPORT: Size = Size {
        width: 800.0,
        height: 600.0,
    };

    fn url(value: &str) -> Url {
        Url::parse(value).unwrap()
    }

    fn session() -> Session<FakeLoader> {
        Session::new(
            FakeLoader::new(&[
                ("https://a.test/", "<h1>A</h1>"),
                ("https://a.test/two", "<h1>B</h1>"),
                ("https://a.test/three", "<h1>C</h1>"),
            ]),
            VIEWPORT,
        )
    }

    fn heading(page: &Page) -> String {
        page.document.text_content(page.document.root())
    }

    #[test]
    fn load_renders_and_records_history() {
        let mut session = session();
        let page = session.load(url("https://a.test/")).unwrap();
        assert_eq!(heading(page), "A");
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
        assert!(!session.can_go_back());
    }

    #[test]
    fn back_and_forward_walk_history() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.load(url("https://a.test/two")).unwrap();
        assert!(session.can_go_back());

        let page = session.back().unwrap();
        assert_eq!(heading(page), "A");
        assert!(session.can_go_forward());

        let page = session.forward().unwrap();
        assert_eq!(heading(page), "B");
        assert!(!session.can_go_forward());
    }

    #[test]
    fn navigating_after_back_drops_forward_entries() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.load(url("https://a.test/two")).unwrap();
        session.back().unwrap();
        session.load(url("https://a.test/three")).unwrap();
        assert!(!session.can_go_forward());
        assert!(session.back().is_ok());
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
    }

    #[test]
    fn refresh_refetches_current_url() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.refresh().unwrap();
        assert_eq!(
            *session.loader.loads.borrow(),
            vec!["https://a.test/", "https://a.test/"]
        );
    }

    #[test]
    fn follow_resolves_relative_links() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        let page = session.follow("two").unwrap();
        assert_eq!(heading(page), "B");
        assert_eq!(
            session.current_url().unwrap().as_str(),
            "https://a.test/two"
        );
    }

    #[test]
    fn errors_do_not_corrupt_history() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        assert!(session.load(url("https://a.test/missing")).is_err());
        // The failed load did not become the current entry.
        assert_eq!(session.current_url().unwrap().as_str(), "https://a.test/");
        assert!(session.back().is_err());
    }

    #[test]
    fn hover_restyles_locally_and_reports_changes() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p:hover { color: #ff0000; }</style><p>hi</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        let p = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();

        assert!(session.set_hovered(Some(p)));
        assert!(!session.set_hovered(Some(p))); // unchanged
        let hovered_style = &session.page().unwrap().styles.by_node[&p];
        assert_eq!(hovered_style.color, lumen_css::Color::rgb(255, 0, 0));
        assert!(session.set_hovered(None));
        // Initial load only; hover never touched the network.
        assert_eq!(session.loader.loads.borrow().len(), 1);
    }

    #[test]
    fn link_target_finds_nearest_anchor() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<div><a href='two'><span>go</span></a></div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        let span = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "span")
            })
            .unwrap();
        assert_eq!(session.link_target(span).as_deref(), Some("two"));
        let div = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "div")
            })
            .unwrap();
        assert_eq!(session.link_target(div), None);
    }

    #[test]
    fn external_stylesheets_load_resolve_and_apply() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/docs/page",
                    "<link rel='stylesheet' href='theme.css'>\
                     <link rel=\"STYLESHEET\" href=\"/root.css\">\
                     <link rel='icon' href='favicon.ico'>\
                     <p>hi</p>",
                ),
                ("https://a.test/docs/theme.css", "p { color: #ff0000; }"),
                ("https://a.test/root.css", "p { font-size: 20px; }"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/docs/page")).unwrap();
        let page = session.page().unwrap();
        let p = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();
        let style = &page.styles.by_node[&p];
        assert_eq!(style.color, lumen_css::Color::rgb(255, 0, 0));
        assert_eq!(style.font_size, 20.0);
        // Page + 2 stylesheets; the icon link was not fetched.
        assert_eq!(session.loader.loads.borrow().len(), 3);
    }

    #[test]
    fn missing_external_stylesheet_does_not_break_the_page() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<link rel='stylesheet' href='gone.css'>\
                 <style>p { color: #00ff00; }</style><p>hi</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        assert!(
            page.document
                .text_content(page.document.root())
                .contains("hi")
        );
        let p = page
            .document
            .descendants(page.document.root())
            .find(|id| {
                page.document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();
        assert_eq!(
            page.styles.by_node[&p].color,
            lumen_css::Color::rgb(0, 255, 0)
        );
    }

    #[test]
    fn hover_and_resize_do_not_refetch_external_css() {
        let mut session = Session::new(
            FakeLoader::new(&[
                (
                    "https://a.test/",
                    "<link rel='stylesheet' href='a.css'><p>hi</p>",
                ),
                ("https://a.test/a.css", "p { color: red; }"),
            ]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        session.set_viewport(Size {
            width: 500.0,
            height: 400.0,
        });
        session.set_hovered(Some(1));
        assert_eq!(session.loader.loads.borrow().len(), 2);
    }

    #[test]
    fn paint_only_hover_repaints_without_relayout() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p { color: #111111; } p:hover { color: #ff0000; }</style>\
                 <p>hover target</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let paragraph = session
            .page()
            .unwrap()
            .document
            .descendants(session.page().unwrap().document.root())
            .find(|id| {
                session
                    .page()
                    .unwrap()
                    .document
                    .element(*id)
                    .is_some_and(|element| element.tag_name == "p")
            })
            .unwrap();
        let layout_before = session.page().unwrap().layout.clone();
        assert!(session.set_hovered(Some(paragraph)));
        let page = session.page().unwrap();
        // Geometry identical (styles inside differ, boxes do not move).
        assert_eq!(
            page.layout.children[0].border_box(),
            layout_before.children[0].border_box()
        );
        // The repaint shows the hover color.
        let hovered_text = page.display_list.iter().find_map(|command| match command {
            lumen_engine::DisplayCommand::DrawText { color, .. } => Some(color.to_string()),
            _ => None,
        });
        assert_eq!(hovered_text.as_deref(), Some("#ff0000"));
    }

    #[test]
    fn hover_without_hover_rules_is_free() {
        let mut session = Session::new(
            FakeLoader::new(&[("https://a.test/", "<p>plain</p>")]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let node = session.page().unwrap().layout.children[0].node_id;
        // Reports "nothing changed": no restyle, no repaint needed.
        assert!(!session.set_hovered(Some(node)));
    }

    #[test]
    fn hovering_generated_content_does_not_panic() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>p::before { content: \"* \"; } p:hover { color: #ff0000; }</style>\
                 <p>hover me</p>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        // The generated node has the highest id — exactly what a hit test
        // over the ::before text would return.
        let generated = session.page().unwrap().document.nodes().len() - 1;
        assert!(session.set_hovered(Some(generated)));
        assert!(session.page().is_some());
        // And a viewport change (full relayout) with the hover still set.
        session.set_viewport(Size {
            width: 640.0,
            height: 480.0,
        });
        assert!(session.page().is_some());
    }

    #[test]
    fn media_queries_restyle_on_viewport_change() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<style>div { height: 10px; background-color: #111111; }\
                 @media (max-width: 600px) { div { background-color: #222222; } }</style>\
                 <div></div>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let fill_colors = |session: &Session<FakeLoader>| -> Vec<String> {
            session
                .page()
                .unwrap()
                .display_list
                .iter()
                .filter_map(|command| match command {
                    lumen_engine::DisplayCommand::FillRect { color, .. } => Some(color.to_string()),
                    _ => None,
                })
                .collect()
        };
        assert!(fill_colors(&session).contains(&"#111111".to_string()));
        session.set_viewport(Size {
            width: 500.0,
            height: 600.0,
        });
        assert!(fill_colors(&session).contains(&"#222222".to_string()));
    }

    #[test]
    fn title_comes_from_the_title_element() {
        let mut session = Session::new(
            FakeLoader::new(&[(
                "https://a.test/",
                "<html><head><title>  My Page </title></head><body>x</body></html>",
            )]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        assert_eq!(session.title().as_deref(), Some("My Page"));
    }

    #[test]
    fn images_load_decode_and_lay_out() {
        let mut session = Session::new(
            FakeLoader::new(&[("https://a.test/", "<img src='red.png' width='40'>")])
                .with_bytes("https://a.test/red.png", &red_pixel_png()),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        assert_eq!(page.images.len(), 1);
        let image_command = page.display_list.iter().find_map(|command| match command {
            lumen_engine::DisplayCommand::DrawImage { rect, image, .. } => {
                Some((rect.width, rect.height, image.width))
            }
            _ => None,
        });
        // width attr 40, square intrinsic ratio → 40x40.
        assert_eq!(image_command, Some((40.0, 40.0, 1)));
        // The SVG backend embeds the original bytes as a data URI.
        let svg = lumen_engine::render_svg(page);
        assert!(svg.contains("data:image/png;base64,"));
    }

    #[test]
    fn broken_image_leaves_page_intact() {
        let mut session = Session::new(
            FakeLoader::new(&[("https://a.test/", "<img src='gone.png'><p>still here</p>")]),
            VIEWPORT,
        );
        session.load(url("https://a.test/")).unwrap();
        let page = session.page().unwrap();
        assert!(page.images.is_empty());
        assert!(
            page.document
                .text_content(page.document.root())
                .contains("still here")
        );
    }

    #[test]
    fn viewport_change_relayouts_without_refetching() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session.set_viewport(Size {
            width: 400.0,
            height: 300.0,
        });
        assert_eq!(session.page().unwrap().viewport.width, 400.0);
        // Only the initial load hit the loader.
        assert_eq!(session.loader.loads.borrow().len(), 1);
    }
}
