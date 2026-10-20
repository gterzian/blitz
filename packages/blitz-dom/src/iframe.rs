//! Loading of `<iframe>` elements into sub-documents.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::Sender;

use blitz_traits::navigation::{NavigationOptions, NavigationProvider};
use blitz_traits::net::Url;
use blitz_traits::node_id::NodeId;
use blitz_traits::shell::ShellProvider;

use crate::document::DocumentEvent;
use crate::resource::ResourceRequest;
use crate::{BaseDocument, DocumentConfig, UserAgentStylesheet, local_name};

/// Maximum nesting depth of documents-within-documents. Iframes nested deeper
/// than this are not loaded (this guards against infinitely recursive
/// self-embedding pages).
pub(crate) const MAX_SUBDOCUMENT_DEPTH: usize = 10;

/// Per-iframe load state: the id of the in-flight HTML load (if any), so
/// stale responses can be discarded.
pub(crate) struct IframeLoad {
    pub(crate) request_id: Option<usize>,
}

/// A [`NavigationProvider`] given to iframe sub-documents. It routes
/// navigations (e.g. link clicks within the iframe) back to the parent
/// document, which reloads the iframe's sub-document with the new URL.
struct IframeNavigationProvider {
    node_id: NodeId,
    tx: Sender<DocumentEvent>,
    shell_provider: Arc<dyn ShellProvider>,
}

impl NavigationProvider for IframeNavigationProvider {
    fn navigate_to(&self, options: NavigationOptions) {
        let _ = self.tx.send(DocumentEvent::NavigateIframe {
            node_id: self.node_id,
            url: options.url,
        });
        self.shell_provider.request_redraw();
    }
}

impl BaseDocument {
    /// Build the [`DocumentConfig`] for an iframe sub-document, inheriting
    /// providers and settings from this (parent) document.
    fn iframe_document_config(&self, node_id: NodeId, base_url: Option<String>) -> DocumentConfig {
        DocumentConfig {
            viewport: None,
            base_url,
            ua_stylesheets: vec![UserAgentStylesheet::default_sheet()],
            // A sub-document's subresource requests name its own nodes, so the
            // parent's hooks cannot serve it: an embedder that loads iframe
            // sub-resources instantiates the sub-document itself.
            hooks: None,
            navigation_provider: Some(Arc::new(IframeNavigationProvider {
                node_id,
                tx: self.tx.clone(),
                shell_provider: self.shell_provider.clone(),
            })),
            shell_provider: Some(self.shell_provider.clone()),
            html_parser_provider: Some(self.html_parser_provider.clone()),
            font_ctx: Some(self.font_ctx.lock().unwrap().clone()),
            media_type: Some(self.media_type.clone()),
            style_threading: self.style_threading,
            incremental: Some(self.incremental_layout),
            embedded_images: false,
            subdocument_depth: self.subdocument_depth + 1,
            // A sub-document is a separate devtools target with its own provider
            #[cfg(feature = "devtools")]
            devtools_provider: None,
        }
    }

    /// Start a new generation of the iframe's content, discarding any response
    /// still in flight for the previous one, and return the id of this
    /// generation. `pending_html` marks whether an HTML load is in flight.
    fn new_iframe_generation(&mut self, node_id: NodeId, pending_html: bool) -> usize {
        static REQUEST_ID_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let request_id = REQUEST_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        self.iframe_loads.insert(
            node_id,
            IframeLoad {
                request_id: pending_html.then_some(request_id),
            },
        );
        request_id
    }

    /// Parse `html` and attach the resulting document as `node_id`'s
    /// sub-document.
    pub(crate) fn attach_iframe_document(
        &mut self,
        node_id: NodeId,
        html: &str,
        base_url: Option<String>,
    ) {
        let config = self.iframe_document_config(node_id, base_url);
        let sub_doc = self
            .html_parser_provider
            .clone()
            .parse_document(html, config);
        self.set_sub_document(node_id, sub_doc);
        self.shell_provider.request_redraw();
    }

    /// Attach a sub-document parsed from the iframe's `srcdoc` attribute.
    /// Relative URLs within a `srcdoc` document resolve against the parent
    /// document's base URL.
    pub(crate) fn load_iframe_srcdoc(&mut self, node_id: NodeId, srcdoc: &str) {
        self.new_iframe_generation(node_id, false);
        let base_url = Some(self.url.to_string());
        self.attach_iframe_document(node_id, srcdoc, base_url);
    }

    /// Request HTML for an iframe from `url`. The parsed document is attached
    /// when the response arrives (via [`DocumentEvent::ResourceLoad`]).
    pub(crate) fn start_iframe_load(&mut self, node_id: NodeId, url: Url) {
        let request_id = self.new_iframe_generation(node_id, true);
        self.hooks.resource_request(ResourceRequest::SubDocument {
            node_id,
            request_id,
            url,
        });
    }

    /// Handle a navigation originating from within an iframe's sub-document
    /// (e.g. a link click) by loading the new URL into the iframe.
    pub(crate) fn navigate_iframe(&mut self, node_id: NodeId, url: Url) {
        let Some(node) = self.get_node(node_id) else {
            return;
        };
        if !node.flags.is_in_document() {
            return;
        }
        self.start_iframe_load(node_id, url);
    }

    /// Apply fetched iframe HTML, discarding stale responses (the iframe may
    /// have been removed or re-navigated since the request was issued).
    pub(crate) fn apply_iframe_html(
        &mut self,
        node_id: NodeId,
        request_id: usize,
        resolved_url: Option<String>,
        html: &str,
    ) {
        let is_current = self
            .iframe_loads
            .get(&node_id)
            .is_some_and(|load| load.request_id == Some(request_id));
        if !is_current {
            return;
        }
        let load = self.iframe_loads.get_mut(&node_id).unwrap();
        load.request_id = None;

        let node_is_iframe = self
            .get_node(node_id)
            .and_then(|node| node.element_data())
            .is_some_and(|el| el.name.local == local_name!("iframe"));
        if !node_is_iframe {
            return;
        }

        self.attach_iframe_document(node_id, html, resolved_url);
    }
}
