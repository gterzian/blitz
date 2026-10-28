//! Regression test for https://github.com/DioxusLabs/blitz/issues/689
//!
//! Style resolution must not run while render-blocking stylesheets are still
//! loading. If it does, elements get computed styles from an incomplete
//! cascade, and the restyle triggered by the stylesheet's arrival spuriously
//! starts CSS transitions from those unstyled values (e.g. `visibility:
//! visible` -> `hidden`), leaving transitioned properties stuck at their
//! pre-stylesheet values in one-shot renders.

use std::rc::Rc;

use blitz_dom::resource::{Resource, ResourceLoadResponse};
use blitz_dom::{DocumentConfig, DocumentEvent, ResourceHooks};
use blitz_html::HtmlDocument;
use style::properties::generated::longhands::visibility::computed_value::T as Visibility;
use style::stylesheets::Origin;

use crate::recording_resource_hooks::{RecordedRequest, RecordingResourceHooks};

/// Install a stylesheet for the request recorded by `hooks`.
fn install_stylesheet(
    doc: &mut HtmlDocument,
    hooks: &RecordingResourceHooks,
    request: &RecordedRequest,
    css: &str,
    response_url: &str,
) {
    let sheet = doc.make_stylesheet(css, Origin::Author);
    doc.handle_message(DocumentEvent::ResourceLoad(ResourceLoadResponse {
        request_id: request.request_id.unwrap_or(0),
        node_id: Some(request.node_id),
        resolved_url: Some(response_url.to_string()),
        result: Ok(Resource::Css(sheet)),
    }));
    let _ = hooks;
}

/// Fail the request recorded by `hooks`.
fn fail_request(doc: &mut HtmlDocument, request: &RecordedRequest, error: &str) {
    doc.handle_message(DocumentEvent::ResourceLoad(ResourceLoadResponse {
        request_id: request.request_id.unwrap_or(0),
        node_id: Some(request.node_id),
        resolved_url: Some(request.url.clone()),
        result: Err(error.to_string()),
    }));
}

fn transitioned_visibility(doc: &mut HtmlDocument) -> Visibility {
    doc.resolve(0.0);
    let node = doc
        .get_node(doc.get_element_by_id("transitioned").unwrap())
        .unwrap();
    node.primary_styles()
        .unwrap()
        .get_inherited_box()
        .slow_clone_visibility()
}

const HTML: &str = r#"<!doctype html>
<html>
  <head><link rel="stylesheet" href="case.css"></head>
  <body><p id="transitioned">text</p></body>
</html>"#;

const CSS: &str = "#transitioned { transition: all 0.2s; visibility: hidden }";

fn document(hooks: &Rc<RecordingResourceHooks>) -> HtmlDocument {
    HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            base_url: Some("http://example.com/".to_string()),
            resource_hooks: Some(Rc::clone(hooks) as Rc<dyn ResourceHooks>),
            ..Default::default()
        },
    )
}

#[test]
fn failed_stylesheet_unblocks_rendering() {
    let hooks = Rc::new(RecordingResourceHooks::default());
    let mut doc = document(&hooks);
    assert!(doc.has_pending_critical_resources());

    let request = hooks.requests().pop().expect("stylesheet requested");
    assert!(request.url.ends_with("case.css"));
    fail_request(&mut doc, &request, "connection refused");

    assert_eq!(transitioned_visibility(&mut doc), Visibility::Visible);
    assert!(!doc.has_pending_critical_resources());
    let node = doc
        .get_node(doc.get_element_by_id("transitioned").unwrap())
        .unwrap();
    assert!(node.final_layout().size.height > 0.0);
}

#[test]
fn empty_or_non_css_stylesheet_unblocks_rendering() {
    for bytes in [b"".as_slice(), b"<!doctype html><p>not CSS</p>", b"\xff"] {
        let hooks = Rc::new(RecordingResourceHooks::default());
        let mut doc = document(&hooks);
        assert!(doc.has_pending_critical_resources());

        let request = hooks.requests().pop().expect("stylesheet requested");
        let css = String::from_utf8_lossy(bytes).into_owned();
        install_stylesheet(
            &mut doc,
            &hooks,
            &request,
            &css,
            "http://example.com/case.css",
        );

        assert!(!doc.has_pending_critical_resources());
        doc.resolve(0.0);
        let node = doc
            .get_node(doc.get_element_by_id("transitioned").unwrap())
            .unwrap();
        assert!(node.final_layout().size.height > 0.0);
    }
}

#[test]
fn invalid_stylesheet_url_does_not_block_rendering() {
    let hooks = Rc::new(RecordingResourceHooks::default());
    let mut doc = HtmlDocument::from_html(
        &HTML.replace("case.css", "http://[invalid"),
        DocumentConfig {
            base_url: Some("http://example.com/".to_string()),
            resource_hooks: Some(Rc::clone(&hooks) as Rc<dyn blitz_dom::ResourceHooks>),
            ..Default::default()
        },
    );

    doc.resolve(0.0);
    assert!(!doc.has_pending_critical_resources());
    assert!(hooks.requests().is_empty());
}

#[test]
fn transition_does_not_start_from_pre_stylesheet_styles() {
    let hooks = Rc::new(RecordingResourceHooks::default());
    let mut doc = document(&hooks);

    // The stylesheet request should have been issued and be tracked as a
    // pending critical resource.
    assert!(doc.has_pending_critical_resources());

    // Resolve while the stylesheet is still loading. This must not give
    // elements computed styles (which would later be treated as
    // before-change styles and start spurious transitions).
    assert!(!doc.resolve(0.0), "a blocked resolve changes nothing");

    // Deliver the stylesheet and resolve again.
    let request = hooks.requests().pop().expect("css requested");
    install_stylesheet(
        &mut doc,
        &hooks,
        &request,
        CSS,
        "http://example.com/case.css",
    );
    doc.resolve(1.0);
    assert!(!doc.has_pending_critical_resources());

    // The element must be hidden immediately: no transition from the
    // pre-stylesheet `visibility: visible` should have started.
    let node_id = doc.get_element_by_id("transitioned").unwrap();
    let node = doc.get_node(node_id).unwrap();
    let styles = node.primary_styles().unwrap();
    assert_eq!(
        styles.get_inherited_box().slow_clone_visibility(),
        Visibility::Hidden,
        "transitioned property should have its stylesheet value, not the pre-stylesheet value"
    );

    // And nothing should be animating.
    assert!(!doc.is_animating());
}
