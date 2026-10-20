//! Hooks the embedding application implements to load subresources and to
//! observe DOM tree mutations.
//!
//! `blitz-dom` does not perform network access itself.  Every subresource the
//! document needs (stylesheets, fonts, images) is reported through
//! [`DocumentHooks::resource_request`], and the embedding application loads
//! it and hands the result back with [`BaseDocument::handle_message`].
//!
//! The DOM-tree hooks mirror the extension points the DOM Standard and HTML
//! Standard define for tree mutations, so an embedding application observes
//! mutations where the specifications say they happen:
//!
//! - <https://dom.spec.whatwg.org/#concept-node-insert-ext>
//! - <https://dom.spec.whatwg.org/#concept-node-remove-ext>
//! - <https://dom.spec.whatwg.org/#concept-node-post-connection-ext>
//! - <https://dom.spec.whatwg.org/#concept-element-attributes-change-ext>

use crate::node::ElementData;
use crate::util::ImageType;
use blitz_traits::net::Bytes;
use std::sync::{Arc, Mutex};
use style::{
    font_face::{
        FontFaceSourceFormat, FontFaceSourceFormatKeyword, FontStyle as StyloFontStyle, Source,
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

/// A subresource the document needs.  The embedding application loads it and
/// installs the result with [`BaseDocument::handle_message`].
#[derive(Clone, Debug)]
pub enum ResourceRequest {
    /// A stylesheet loaded for a `<link rel=stylesheet>` element.
    /// <https://html.spec.whatwg.org/multipage/links.html#link-type-stylesheet>
    Stylesheet { node_id: usize, url: Url },
    /// A stylesheet imported with `@import`.
    /// <https://drafts.csswg.org/css-cascade/#at-import>
    StylesheetImport { url: Url },
    /// An image referenced by `background-image`, `mask-image`, or a replaced
    /// element that is not rendered as an embedded layer.
    Image {
        node_id: usize,
        url: Url,
        image_type: ImageType,
    },
    /// A font referenced by an `@font-face` rule.
    /// <https://drafts.csswg.org/css-fonts/#font-face-rule>
    Font {
        node_id: Option<usize>,
        url: Url,
        overrides: FontFaceOverrides,
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
    pub style: Option<parley::fontique::FontStyle>,
}

/// The DOM-tree and subresource hooks of one document.
pub trait DocumentHooks: Send + Sync + 'static {
    /// Report a subresource the document needs.
    fn resource_request(&self, _request: ResourceRequest) {}

    /// Run the attribute change steps.
    /// <https://dom.spec.whatwg.org/#concept-element-attributes-change-ext>
    fn attribute_change_steps(
        &self,
        _node_id: usize,
        _element: &ElementData,
        _local_name: &str,
        _old_value: Option<&str>,
        _new_value: Option<&str>,
    ) {
    }

    /// Run the insertion steps.
    /// <https://dom.spec.whatwg.org/#concept-node-insert-ext>
    fn insertion_steps(&self, _node_id: usize, _element: Option<&ElementData>) {}

    /// Run the post-connection steps.
    /// <https://dom.spec.whatwg.org/#concept-node-post-connection-ext>
    fn post_connection_steps(&self, _node_id: usize, _element: Option<&ElementData>) {}

    /// Run the removing steps.
    /// <https://dom.spec.whatwg.org/#concept-node-remove-ext>
    fn removing_steps(
        &self,
        _node_id: usize,
        _element: Option<&ElementData>,
        _is_subtree_root: bool,
    ) {
    }
}

/// Hooks for an embedding application that needs no tree or subresource
/// notifications.
pub struct NoopDocumentHooks;

impl DocumentHooks for NoopDocumentHooks {}

/// The result of loading one subresource, installed by
/// [`crate::BaseDocument::handle_message`].
#[derive(Clone, Debug)]
pub enum Resource {
    Image(ImageType, u32, u32, Arc<Vec<u8>>),
    #[cfg(feature = "svg")]
    Svg(ImageType, Arc<usvg::Tree>),
    Css(DocumentStyleSheet),
    Font(Bytes, FontFaceOverrides),
    None,
}

#[allow(unused)]
pub struct ResourceLoadResponse {
    pub request_id: usize,
    pub node_id: Option<usize>,
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
    pub(crate) hooks: Arc<dyn DocumentHooks>,
}

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
        if !supports.as_ref().is_none_or(|s| s.enabled) {
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
        self.hooks
            .resource_request(ResourceRequest::StylesheetImport {
                url: url.as_ref().clone(),
            });

        import
    }
}

/// Collect the `@font-face` sources of a stylesheet, with the descriptors
/// needed to register the loaded font under its CSS family name.
pub(crate) fn collect_font_face_requests(
    sheet: &Stylesheet,
    read_guard: &SharedRwLockReadGuard,
) -> Vec<(Url, FontFaceOverrides)> {
    let mut requests = Vec::new();
    for rule in sheet.contents(read_guard).rules(read_guard).iter() {
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
                .map(|range| range.0.compute().value()),
            style: descriptor.font_style.as_ref().map(stylo_to_fontique_style),
        };
        let preferred_source = src
            .0
            .iter()
            .filter_map(|source| match source {
                Source::Url(url_source) => Some(url_source),
                // TODO: support local fonts in @font-face
                Source::Local(_) => None,
            })
            .find_map(|url_source| {
                let mut format = match &url_source.format_hint {
                    Some(FontFaceSourceFormat::Keyword(fmt)) => *fmt,
                    Some(FontFaceSourceFormat::String(str)) => match str.as_str() {
                        "woff2" => FontFaceSourceFormatKeyword::Woff2,
                        "ttf" => FontFaceSourceFormatKeyword::Truetype,
                        "otf" => FontFaceSourceFormatKeyword::Opentype,
                        _ => FontFaceSourceFormatKeyword::None,
                    },
                    _ => FontFaceSourceFormatKeyword::None,
                };
                if format == FontFaceSourceFormatKeyword::None {
                    let (_, end) = url_source.url.as_str().rsplit_once('.')?;
                    format = match end {
                        "woff2" => FontFaceSourceFormatKeyword::Woff2,
                        "woff" => FontFaceSourceFormatKeyword::Woff,
                        "ttf" => FontFaceSourceFormatKeyword::Truetype,
                        "otf" => FontFaceSourceFormatKeyword::Opentype,
                        "svg" => FontFaceSourceFormatKeyword::Svg,
                        "eot" => FontFaceSourceFormatKeyword::EmbeddedOpentype,
                        _ => FontFaceSourceFormatKeyword::None,
                    }
                }

                if matches!(
                    format,
                    FontFaceSourceFormatKeyword::Svg
                        | FontFaceSourceFormatKeyword::EmbeddedOpentype
                        | FontFaceSourceFormatKeyword::None
                ) {
                    return None;
                }

                let url = url_source.url.url().unwrap().as_ref().clone();
                Some((url, overrides.clone()))
            });
        if let Some(request) = preferred_source {
            requests.push(request);
        }
    }
    requests
}

/// Translate stylo's `@font-face` `font-style` descriptor into the fontique
/// `FontStyle` enum used by parley. Stylo encodes Italic and Oblique-with-
/// angle distinctly; CSS's bare `normal` is parsed as `Oblique(0deg, 0deg)`
/// by stylo (see the `FontStyle::parse` impl in stylo's `font_face.rs`), so
/// that pattern is treated as `Normal` here.
fn stylo_to_fontique_style(style: &StyloFontStyle) -> parley::fontique::FontStyle {
    use parley::fontique::FontStyle as Fq;
    match style {
        StyloFontStyle::Italic => Fq::Italic,
        StyloFontStyle::Oblique(min, max) => {
            let angle = min.degrees();
            // Stylo emits `Oblique(0deg, 0deg)` for the literal CSS `normal`
            // keyword. Map that back to `Normal` so parley's font matching
            // doesn't misclassify upright fonts.
            if angle == 0.0 && max.degrees() == 0.0 {
                Fq::Normal
            } else {
                Fq::Oblique(Some(angle))
            }
        }
    }
}

/// Build a stylesheet from CSS text, reporting its `@import` rules through
/// `loader`.
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
