//! Subresource requests and DOM-tree mutation notifications.
//!
//! `blitz-dom` performs no network access.  The embedding application loads
//! every [`ResourceRequest`] it receives and installs the result with
//! [`BaseDocument::handle_message`].

use crate::node::ElementData;
use crate::util::ImageType;
use blitz_traits::net::Bytes;
use blitz_traits::node_id::NodeId;
use parley::fontique::FontStyle as FontiqueFontStyle;
use std::sync::{Arc, Mutex};
use style::{
    font_face::{
        FontFaceSourceFormat, FontFaceSourceFormatKeyword, FontStyleRange, Source, SourceList,
    },
    media_queries::MediaList,
    servo_arc::Arc as ServoArc,
    shared_lock::{Locked, SharedRwLock, SharedRwLockReadGuard},
    stylesheets::{
        AllowImportRules, CssRule, DocumentStyleSheet, ImportRule, Origin, Stylesheet,
        StylesheetInDocument, StylesheetLoader as ServoStylesheetLoader, UrlExtraData,
        import_rule::{ImportLayer, ImportSheet, ImportSupportsCondition},
    },
    values::{CssUrl, SourceLocation},
};
use url::Url;

/// A subresource the document needs.
#[derive(Clone, Debug)]
pub enum ResourceRequest {
    /// A `<link rel=stylesheet>` stylesheet.
    Stylesheet {
        node_id: NodeId,
        url: Url,
        /// Set for a stylesheet linked from the document's head: the id the
        /// document tracks as a pending critical resource until the
        /// stylesheet is installed, echoed back in
        /// [`ResourceLoadResponse::request_id`].
        request_id: Option<usize>,
    },
    /// An `@import` stylesheet.
    StylesheetImport { url: Url },
    /// A CSS `background-image`/`mask-image`, or a replaced element image that
    /// the document paints itself.
    Image {
        node_id: NodeId,
        url: Url,
        image_type: ImageType,
    },
    /// An `@font-face` font.
    Font {
        node_id: Option<NodeId>,
        url: Url,
        overrides: FontFaceOverrides,
    },
    /// HTML loaded for an `<iframe>` element's `src`. `request_id` identifies
    /// the load; [`BaseDocument::handle_message`] only attaches the document
    /// when it is still the iframe's current load.
    SubDocument {
        node_id: NodeId,
        request_id: usize,
        url: Url,
    },
}

/// Carries `@font-face` descriptors from CSS parsing through to font
/// registration so `parley::Collection::register_fonts` can alias the bytes
/// under the `font-family` declared in CSS rather than whatever family name
/// the TTF's own `name` table reports.
///
/// All fields are `Option` because each descriptor is independently optional
/// at the CSS level. Missing fields fall back to the values parley reads
/// from the font's own metadata.
#[derive(Clone, Debug, Default)]
pub struct FontFaceOverrides {
    /// `font-family` descriptor (the alias the rest of the stylesheet uses).
    pub family_name: Option<String>,
    /// `font-weight` descriptor as a single CSS weight (100–900). Stylo
    /// parses this as a range; we record the lower bound, which equals the
    /// upper bound in the common single-value case.
    pub weight: Option<f32>,
    /// `font-style` descriptor mapped to fontique's `FontStyle`.
    pub style: Option<FontiqueFontStyle>,
    /// `font-stretch` (`font-width`) descriptor as a single width. Like
    /// `weight`, we record the lower bound of Stylo's range.
    pub width: Option<parley::fontique::FontWidth>,
}

/// <https://dom.spec.whatwg.org/#mutation-algorithms>
pub trait MutationHooks: Send + Sync + 'static {
    /// <https://dom.spec.whatwg.org/#concept-element-attributes-change-ext>
    fn attribute_change_steps(
        &self,
        _node_id: NodeId,
        _element: &ElementData,
        _local_name: &str,
        _old_value: Option<&str>,
        _new_value: Option<&str>,
        _namespace: &str,
    ) {
    }

    /// <https://dom.spec.whatwg.org/#concept-node-insert-ext>
    fn insertion_steps(&self, _node_id: NodeId, _element: Option<&ElementData>) {}

    /// <https://dom.spec.whatwg.org/#concept-node-post-connection-ext>
    fn post_connection_steps(&self, _node_id: NodeId, _element: Option<&ElementData>) {}

    /// <https://dom.spec.whatwg.org/#concept-node-remove-ext>
    fn removing_steps(
        &self,
        _node_id: NodeId,
        _element: Option<&ElementData>,
        _is_subtree_root: bool,
        _old_ancestor: Option<&ElementData>,
    ) {
    }
}

/// The document's subresource requests, reported by the embedding application.
pub trait ResourceHooks: Send + Sync + 'static {
    fn resource_request(&self, _request: ResourceRequest) {}

    /// Whether this implementation loads nothing at all, in which case a
    /// document must not wait for the resources it reports.
    fn is_noop(&self) -> bool {
        false
    }
}

/// Hooks for an embedding application that needs no mutation notifications.
pub struct NoopMutationHooks;

impl MutationHooks for NoopMutationHooks {}

/// Hooks for an embedding application that needs no subresource requests.
pub struct NoopResourceHooks;

impl ResourceHooks for NoopResourceHooks {
    fn is_noop(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug)]
pub enum Resource {
    Image(ImageType, u32, u32, Arc<Vec<u8>>),
    #[cfg(feature = "svg")]
    Svg(ImageType, crate::node::SvgImageData),
    Css(DocumentStyleSheet),
    Font(Bytes, FontFaceOverrides),
    /// HTML fetched for an `<iframe>` element's `src`
    DocumentSrc(String),
    None,
}

pub struct ResourceLoadResponse {
    pub request_id: usize,
    pub node_id: Option<NodeId>,
    pub resolved_url: Option<String>,
    pub result: Result<Resource, String>,
}

/// One `@import` rule waiting for its stylesheet.
pub(crate) struct PendingStylesheetImport {
    pub(crate) url: ServoArc<Url>,
    pub(crate) lock: SharedRwLock,
    pub(crate) media: ServoArc<Locked<MediaList>>,
    pub(crate) import_rule: ServoArc<Locked<ImportRule>>,
    pub(crate) loader: StylesheetLoader,
}

/// The loader stylo calls for `@import` rules while building a stylesheet.
#[derive(Clone)]
pub(crate) struct StylesheetLoader {
    pub(crate) pending_imports: Arc<Mutex<Vec<PendingStylesheetImport>>>,
    pub(crate) resource_hooks: Arc<dyn ResourceHooks>,
    /// Depth of `@import` nesting for the stylesheet this loader is parsing
    /// (0 for a top-level stylesheet).
    pub(crate) import_depth: usize,
}

/// Maximum depth of nested `@import` rules. Imports nested deeper than this
/// are refused, preventing unbounded recursion (e.g. a stylesheet whose
/// resolved `@import` URL yields another stylesheet importing in turn, with
/// the URL growing geometrically at each level).
const MAX_IMPORT_DEPTH: usize = 16;

impl ServoStylesheetLoader for StylesheetLoader {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: SourceLocation,
        lock: &SharedRwLock,
        media: ServoArc<Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> ServoArc<Locked<ImportRule>> {
        if self.import_depth >= MAX_IMPORT_DEPTH || !supports.as_ref().is_none_or(|s| s.enabled) {
            return ServoArc::new(lock.wrap(ImportRule {
                url,
                stylesheet: ImportSheet::new_refused(),
                supports,
                layer,
                source_location: location,
            }));
        }

        let import = ImportRule {
            url,
            stylesheet: ImportSheet::new_pending(),
            supports,
            layer,
            source_location: location,
        };

        let url = import.url.url().unwrap().clone();
        let import = ServoArc::new(lock.wrap(import));

        if let Ok(mut pending_imports) = self.pending_imports.lock() {
            pending_imports.push(PendingStylesheetImport {
                url: url.clone(),
                lock: lock.clone(),
                media,
                import_rule: import.clone(),
                loader: self.clone(),
            });
        }
        self.resource_hooks
            .resource_request(ResourceRequest::StylesheetImport {
                url: url.as_ref().clone(),
            });

        import
    }
}

pub(crate) fn collect_font_face_requests(
    sheet: &Stylesheet,
    read_guard: &SharedRwLockReadGuard,
) -> Vec<(Url, FontFaceOverrides)> {
    collect_font_face_requests_in_rules(
        sheet.contents(read_guard).rules(read_guard).iter(),
        read_guard,
    )
}

/// Collect the `@font-face` sources among `rules`, with the descriptors
/// needed to register the loaded font under its CSS family name.
pub(crate) fn collect_font_face_requests_in_rules<'a>(
    rules: impl Iterator<Item = &'a CssRule>,
    read_guard: &SharedRwLockReadGuard,
) -> Vec<(Url, FontFaceOverrides)> {
    let mut requests = Vec::new();
    for rule in rules {
        let CssRule::FontFace(font_face) = rule else {
            continue;
        };
        let descriptor = &font_face.read_with(read_guard).descriptors;
        let Some(family) = descriptor.font_family.as_ref() else {
            continue;
        };
        let Some(src) = descriptor.src.as_ref() else {
            continue;
        };
        let overrides = FontFaceOverrides {
            family_name: Some(family.name.to_string()),
            weight: descriptor
                .font_weight
                .as_ref()
                .and_then(|range| range.0.compute().map(|weight| weight.value())),
            style: descriptor.font_style.as_ref().map(stylo_to_fontique_style),
            width: descriptor.font_width.as_ref().and_then(|range| {
                range
                    .compute()
                    .map(|width| parley::fontique::FontWidth::from_percentage(width.0.0.to_float()))
            }),
        };
        if let Some((url, _format)) = preferred_font_source(src) {
            requests.push((url, overrides));
        }
    }
    requests
}

/// The first source in an `@font-face` `src` list that is in a format we
/// support, with its resolved url and format.
fn preferred_font_source(source_list: &SourceList) -> Option<(Url, FontFaceSourceFormatKeyword)> {
    source_list.0.iter().find_map(|source| {
        let url_source = match source {
            Source::Url(url_source) => url_source,
            // TODO: support local fonts in @font-face
            Source::Local(_) => return None,
        };

        // A relative url with no base url to resolve against
        // yields None; skip the source instead of panicking
        let Some(url) = url_source.url.url() else {
            #[cfg(feature = "tracing")]
            tracing::warn!("Skipping @font-face source with unresolvable url");
            return None;
        };

        let format = match &url_source.format_hint {
            Some(FontFaceSourceFormat::Keyword(keyword)) => *keyword,
            Some(FontFaceSourceFormat::String(string)) => font_format_from_string(string),
            None => font_format_from_extension(url),
        };

        let supported = match format {
            FontFaceSourceFormatKeyword::None
            | FontFaceSourceFormatKeyword::Collection
            | FontFaceSourceFormatKeyword::Opentype
            | FontFaceSourceFormatKeyword::Truetype => true,
            FontFaceSourceFormatKeyword::Woff | FontFaceSourceFormatKeyword::Woff2 => {
                cfg!(feature = "woff")
            }
            FontFaceSourceFormatKeyword::EmbeddedOpentype
            | FontFaceSourceFormatKeyword::Svg
            | FontFaceSourceFormatKeyword::Unknown => false,
        };
        if !supported {
            #[cfg(feature = "tracing")]
            tracing::warn!("Skipping unsupported font of type {:?}", format);
            return None;
        }

        Some((url.as_ref().clone(), format))
    })
}

/// Map a `format("...")` string hint to a format. Besides the keywords, this
/// accepts the legacy `-variations` strings and the `ttf`/`otf` extensions.
fn font_format_from_string(format: &str) -> FontFaceSourceFormatKeyword {
    const FORMATS: &[(&str, FontFaceSourceFormatKeyword)] = &[
        ("collection", FontFaceSourceFormatKeyword::Collection),
        (
            "embedded-opentype",
            FontFaceSourceFormatKeyword::EmbeddedOpentype,
        ),
        ("opentype", FontFaceSourceFormatKeyword::Opentype),
        ("opentype-variations", FontFaceSourceFormatKeyword::Opentype),
        ("otf", FontFaceSourceFormatKeyword::Opentype),
        ("svg", FontFaceSourceFormatKeyword::Svg),
        ("truetype", FontFaceSourceFormatKeyword::Truetype),
        ("truetype-variations", FontFaceSourceFormatKeyword::Truetype),
        ("ttf", FontFaceSourceFormatKeyword::Truetype),
        ("woff", FontFaceSourceFormatKeyword::Woff),
        ("woff-variations", FontFaceSourceFormatKeyword::Woff),
        ("woff2", FontFaceSourceFormatKeyword::Woff2),
        ("woff2-variations", FontFaceSourceFormatKeyword::Woff2),
    ];
    FORMATS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(format))
        .map_or(FontFaceSourceFormatKeyword::Unknown, |(_, format)| *format)
}

/// Guess a font's format from the file extension of its url's path. Urls
/// with no known extension, such as `data:` urls, get
/// `FontFaceSourceFormatKeyword::None` and are sniffed from their bytes once
/// fetched.
fn font_format_from_extension(url: &Url) -> FontFaceSourceFormatKeyword {
    const EXTENSIONS: &[(&str, FontFaceSourceFormatKeyword)] = &[
        ("eot", FontFaceSourceFormatKeyword::EmbeddedOpentype),
        ("otf", FontFaceSourceFormatKeyword::Opentype),
        ("svg", FontFaceSourceFormatKeyword::Svg),
        ("ttc", FontFaceSourceFormatKeyword::Collection),
        ("ttf", FontFaceSourceFormatKeyword::Truetype),
        ("woff", FontFaceSourceFormatKeyword::Woff),
        ("woff2", FontFaceSourceFormatKeyword::Woff2),
    ];
    if url.cannot_be_a_base() {
        return FontFaceSourceFormatKeyword::None;
    }
    let file_name = url.path().rsplit('/').next().unwrap_or_default();
    let Some((_, extension)) = file_name.rsplit_once('.') else {
        return FontFaceSourceFormatKeyword::None;
    };
    EXTENSIONS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(extension))
        .map_or(FontFaceSourceFormatKeyword::None, |(_, format)| *format)
}

/// Translate stylo's `@font-face` `font-style` descriptor into the fontique
/// `FontStyle` enum used by parley. Stylo encodes Italic and Oblique-with-
/// angle distinctly; CSS's bare `normal` is parsed as `Oblique(0deg, 0deg)`
/// by stylo (see the `FontStyle::parse` impl in stylo's `font_face.rs`), so
/// that pattern is treated as `Normal` here.
fn stylo_to_fontique_style(style: &FontStyleRange) -> parley::fontique::FontStyle {
    use parley::fontique::FontStyle as Fq;
    match style {
        FontStyleRange::Italic => Fq::Italic,
        FontStyleRange::Oblique(min, max) => {
            let angle = min.degrees();
            // Stylo emits `Oblique(0deg, 0deg)` for the literal CSS `normal`
            // keyword. Map that back to `Normal` so parley's font matching
            // doesn't misclassify upright fonts.
            if angle.is_none_or(|angle| angle == 0.0)
                && max.degrees().is_none_or(|angle| angle == 0.0)
            {
                Fq::Normal
            } else {
                Fq::Oblique(angle)
            }
        }
    }
}

pub(crate) fn make_stylesheet(
    css: &str,
    url: &UrlExtraData,
    origin: Origin,
    media: ServoArc<Locked<MediaList>>,
    guard: &SharedRwLock,
    loader: &StylesheetLoader,
) -> DocumentStyleSheet {
    let sheet = Stylesheet::from_str(
        css,
        url.clone(),
        origin,
        media,
        guard.clone(),
        Some(loader),
        None,
        selectors::context::QuirksMode::NoQuirks,
        AllowImportRules::Yes,
    );
    DocumentStyleSheet(ServoArc::new(sheet))
}
