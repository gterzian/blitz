use crate::HtmlParserProvider;
use crate::resource::DocumentHooks;
use blitz_traits::{
    navigation::NavigationProvider,
    shell::{ShellProvider, Viewport},
};
use parley::FontContext;
use std::sync::Arc;
use style::media_queries::MediaType;

/// Strategy for Stylo's style traversal during `resolve`.
///
/// Two `Document`s resolving on [`StyleThreading::Parallel`] concurrently
/// share Stylo's global thread pool and can panic with
/// `already mutably borrowed` — see
/// <https://github.com/DioxusLabs/blitz/issues/430>. Set
/// [`StyleThreading::Sequential`] on documents that may resolve from a
/// user thread while another `Parallel` resolve is in flight.
#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
pub enum StyleThreading {
    /// Use Stylo's parallel traversal via its global rayon thread pool.
    /// Fastest for a single document; panics if another `Parallel` resolve
    /// is in flight on a different thread.
    Parallel,
    /// Run style traversal sequentially on the calling thread, bypassing
    /// the global pool. Safe to use from many user threads concurrently.
    #[default]
    Sequential,
}

/// Options used when constructing a [`BaseDocument`](crate::BaseDocument)
#[derive(Default)]
pub struct DocumentConfig {
    /// The initial `Viewport`
    pub viewport: Option<Viewport>,
    /// The base url which relative URLs are resolved against
    pub base_url: Option<String>,
    /// User Agent stylesheets
    pub ua_stylesheets: Option<Vec<String>>,
    /// Hooks that receive the document's subresource requests and DOM tree
    /// mutation notifications.
    pub hooks: Option<Arc<dyn DocumentHooks>>,
    /// Navigation provider to handle link clicks and form submissions
    pub navigation_provider: Option<Arc<dyn NavigationProvider>>,
    /// Shell provider to redraw requests, clipboard, etc
    pub shell_provider: Option<Arc<dyn ShellProvider>>,
    /// HTML parser provider. Used to parse HTML for setInnerHTML
    pub html_parser_provider: Option<Arc<dyn HtmlParserProvider>>,
    /// Parley `FontContext`
    pub font_ctx: Option<FontContext>,
    /// The CSS media type used to evaluate `@media` rules.
    /// Defaults to [`MediaType::screen`].
    pub media_type: Option<MediaType>,
    /// Strategy for Stylo's style traversal.
    /// Defaults to [`StyleThreading::Parallel`].
    pub style_threading: StyleThreading,
    /// When true, `<img>` elements are painted as separate embedded layers by
    /// the embedding application rather than baked into the document's scene.
    /// Sub-documents (same-origin iframes) keep this false so their images
    /// stay in the parent's scene.
    pub embedded_images: bool,
}
