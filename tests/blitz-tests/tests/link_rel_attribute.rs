//! A `<link>` starts loading its stylesheet once it has both `rel=stylesheet`
//! and an `href`, whichever of the two a script sets last.
//!
//! The insertion hook runs before a script has set any attribute, so the load
//! has to be re-triggered by the attribute that completes the pair; only
//! `href` did, and a link given its `href` first and its `rel` second was never
//! fetched.

use std::rc::Rc;

use blitz_dom::{DocumentConfig, NodeId, QualName, ResourceHooks, local_name, ns};
use blitz_html::HtmlDocument;

use crate::recording_resource_hooks::RecordingResourceHooks;

fn attr(name: &str) -> QualName {
    QualName::new(None, ns!(), name.into())
}

/// An empty document whose resources are recorded, with a `<link>` freshly
/// appended to its head, carrying no attributes yet.
fn doc_with_link() -> (HtmlDocument, NodeId, Rc<RecordingResourceHooks>) {
    let hooks = Rc::new(RecordingResourceHooks::default());
    let mut doc = HtmlDocument::from_html(
        "<html><head></head><body></body></html>",
        DocumentConfig {
            base_url: Some("http://example.com/".to_string()),
            resource_hooks: Some(Rc::clone(&hooks) as Rc<dyn ResourceHooks>),
            ..Default::default()
        },
    );
    let head = doc
        .find_element_by_tag_name(&local_name!("head"))
        .expect("head")
        .id;
    let link = doc
        .mutate()
        .create_element(QualName::new(None, ns!(html), local_name!("link")), vec![]);
    doc.mutate().append_children(head, &[link]);
    assert!(hooks.urls().is_empty());
    (doc, link, hooks)
}

#[test]
fn rel_set_after_href_loads_the_sheet() {
    let (mut doc, link, hooks) = doc_with_link();

    doc.mutate().set_attribute(link, attr("href"), "late.css");
    assert!(
        hooks.urls().is_empty(),
        "an href alone does not name a stylesheet"
    );

    doc.mutate().set_attribute(link, attr("rel"), "stylesheet");
    assert_eq!(
        hooks.urls(),
        vec!["http://example.com/late.css".to_string()]
    );
}

#[test]
fn href_set_after_rel_loads_the_sheet() {
    let (mut doc, link, hooks) = doc_with_link();

    doc.mutate().set_attribute(link, attr("rel"), "stylesheet");
    assert!(hooks.urls().is_empty());

    doc.mutate().set_attribute(link, attr("href"), "late.css");
    assert_eq!(
        hooks.urls(),
        vec!["http://example.com/late.css".to_string()]
    );
}
