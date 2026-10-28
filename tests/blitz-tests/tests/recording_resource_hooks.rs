//! Test support: a `ResourceHooks` implementation that records the
//! document's subresource requests, so a test can answer them at a time of its
//! choosing instead of letting a network layer decide.

use std::cell::RefCell;

use blitz_dom::{NodeId, ResourceHooks, ResourceRequest};

/// One subresource request the document reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecordedRequest {
    pub(crate) node_id: NodeId,
    pub(crate) url: String,
    /// The id the document tracks the request under while it blocks rendering.
    pub(crate) request_id: Option<usize>,
}

/// Records every `ResourceRequest` the document reports.
#[derive(Default)]
pub(crate) struct RecordingResourceHooks {
    requests: RefCell<Vec<RecordedRequest>>,
}

impl RecordingResourceHooks {
    /// The requests recorded so far, in the order the document reported them.
    pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.borrow().clone()
    }

    /// The url of every request recorded so far.
    pub(crate) fn urls(&self) -> Vec<String> {
        self.requests
            .borrow()
            .iter()
            .map(|request| request.url.clone())
            .collect()
    }
}

impl ResourceHooks for RecordingResourceHooks {
    fn resource_request(&self, request: ResourceRequest) {
        let (node_id, url, request_id) = match request {
            ResourceRequest::Stylesheet {
                node_id,
                url,
                request_id,
            } => (node_id, url, request_id),
            ResourceRequest::StylesheetImport { url } => (NodeId::default(), url, None),
            ResourceRequest::Image { node_id, url, .. } => (node_id, url, None),
            ResourceRequest::Font { url, .. } => (NodeId::default(), url, None),
            ResourceRequest::SubDocument { node_id, url, .. } => (node_id, url, None),
        };
        self.requests.borrow_mut().push(RecordedRequest {
            node_id,
            url: url.to_string(),
            request_id,
        });
    }
}
