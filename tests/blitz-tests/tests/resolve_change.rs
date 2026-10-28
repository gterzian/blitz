//! `BaseDocument::resolve` reports whether the pass changed the rendering: a
//! style change (a `:hover` rule matching) is reported, while a state change
//! that leaves every computed style identical is not.

use blitz_dom::DocumentConfig;
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

fn doc(html: &str) -> HtmlDocument {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(200, 200, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    assert!(
        doc.resolve(0.0),
        "the first resolve builds the initial rendering"
    );
    doc
}

#[test]
fn hovering_without_a_hover_rule_changes_nothing() {
    let mut doc = doc(r#"<html><body style="margin:0">
        <div id="target" style="width:100px; height:100px;"></div>
    </body></html>"#);
    assert!(doc.set_hover_to(50.0, 50.0), "the pointer enters the div");
    assert!(
        !doc.resolve(0.0),
        "no :hover rule matches, so no computed style changed"
    );
    assert!(
        !doc.resolve(0.0),
        "a second resolve finds nothing left to change"
    );
}

#[test]
fn hovering_over_a_hover_rule_changes_the_rendering() {
    let mut doc = doc(r#"<html><head><style>
            #target { width: 100px; height: 100px; background: rgb(0, 0, 255); }
            #target:hover { background: rgb(255, 0, 0); }
        </style></head><body style="margin:0">
        <div id="target"></div>
    </body></html>"#);
    assert!(doc.set_hover_to(50.0, 50.0), "the pointer enters the div");
    assert!(
        doc.resolve(0.0),
        "the :hover rule changed the background colour"
    );
    assert!(!doc.resolve(0.0), "the change is not reported twice");
}

#[test]
fn leaving_a_hovered_element_reports_the_reverted_style() {
    let mut doc = doc(r#"<html><head><style>
            #target:hover { background: rgb(255, 0, 0); }
        </style></head><body style="margin:0">
        <div id="target" style="width:100px; height:100px;"></div>
    </body></html>"#);
    assert!(doc.set_hover_to(50.0, 50.0));
    assert!(doc.resolve(0.0), "entering the div changed the background");
    assert!(doc.clear_hover());
    assert!(doc.resolve(0.0), "leaving the div reverted the background");
}
