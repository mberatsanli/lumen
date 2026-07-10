//! Browser orchestration: sessions, navigation and history.
//!
//! A [`Session`] owns a resource loader and a viewport, fetches documents
//! and runs them through the engine pipeline. Navigation is deliberately
//! separate from rendering: the engine knows nothing about URLs, and this
//! crate knows nothing about painting beyond handing back a [`Page`].

use lumen_engine::{HeuristicMeasurer, Page, Size, TextMeasurer, build_page_with_measurer};
use lumen_platform::{LoadError, ResourceLoader, ResourceRequest, Url, resolve};

/// One browsing context with linear history.
///
/// History entries are re-fetched on `back`/`forward`/`refresh`; there is
/// no document cache yet. All responses are treated as HTML.
pub struct Session<L: ResourceLoader> {
    loader: L,
    viewport: Size,
    measurer: Box<dyn TextMeasurer>,
    history: Vec<Url>,
    /// Index of the current entry in `history`, if any page is loaded.
    index: Option<usize>,
    page: Option<Page>,
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
        }
    }

    /// Replaces the text measurer (e.g. with real font metrics) and
    /// relayouts the current page if one is loaded.
    pub fn set_measurer(&mut self, measurer: Box<dyn TextMeasurer>) -> Result<(), LoadError> {
        self.measurer = measurer;
        if self.index.is_some() {
            self.refresh()?;
        }
        Ok(())
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

    /// Changes the viewport and lays the current page out again.
    pub fn set_viewport(&mut self, viewport: Size) -> Result<(), LoadError> {
        self.viewport = viewport;
        if self.index.is_some() {
            self.refresh()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn page(&self) -> Option<&Page> {
        self.page.as_ref()
    }

    #[must_use]
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
        self.page = Some(build_page_with_measurer(
            &response.text(),
            self.viewport,
            self.measurer.as_ref(),
        ));
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
        pages: HashMap<String, String>,
        loads: RefCell<Vec<String>>,
    }

    impl FakeLoader {
        fn new(pages: &[(&str, &str)]) -> Self {
            Self {
                pages: pages
                    .iter()
                    .map(|(url, html)| ((*url).to_string(), (*html).to_string()))
                    .collect(),
                loads: RefCell::new(Vec::new()),
            }
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
                content_type: Some("text/html".to_string()),
                body: body.clone().into_bytes(),
            })
        }
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
    fn viewport_change_relayouts() {
        let mut session = session();
        session.load(url("https://a.test/")).unwrap();
        session
            .set_viewport(Size {
                width: 400.0,
                height: 300.0,
            })
            .unwrap();
        assert_eq!(session.page().unwrap().viewport.width, 400.0);
    }
}
