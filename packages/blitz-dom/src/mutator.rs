//! The DOM Standard's mutation algorithms
//! (<https://dom.spec.whatwg.org/#mutation-algorithms>) and the
//! [`MutationHooks`](crate::MutationHooks) they invoke.

use std::collections::HashSet;
use std::mem;
use std::ops::{Deref, DerefMut};

use crate::document::make_device;
use crate::layout::damage::ALL_DAMAGE;
use crate::node::{CanvasData, NodeFlags, SpecialElementData};
use crate::resource::{MutationHooks, ResourceRequest};
use crate::util::ImageType;
use crate::{
    Attribute, BaseDocument, Document, ElementData, Node, NodeData, QualName, local_name, qual_name,
};
use blitz_traits::shell::Viewport;
use slab::Slab;
use style::Atom;
use style::invalidation::element::restyle_hints::RestyleHint;
use style::stylesheets::OriginSet;

macro_rules! tag_and_attr {
    ($tag:tt, $attr:tt) => {
        (&local_name!($tag), &local_name!($attr))
    };
}

#[derive(Debug, Clone)]
pub enum AppendTextErr {
    /// The node is not a text node
    NotTextNode,
}

/// Operations that happen almost immediately, but are deferred within a
/// function for borrow-checker reasons.
enum SpecialOp {
    LoadImage(usize),
    LoadStylesheet(usize),
    UnloadStylesheet(usize),
    LoadCustomPaintSource(usize),
    ProcessButtonInput(usize),
    UnloadSubDocument(usize),
    #[cfg(feature = "custom-widget")]
    UnloadCustomWidget(usize),
}

pub struct DocumentMutator<'doc> {
    /// Document is public as an escape hatch, but users of this API should ideally avoid using it
    /// and prefer exposing additional functionality in DocumentMutator.
    pub doc: &'doc mut BaseDocument,

    eager_op_queue: Vec<SpecialOp>,

    // Tracked nodes for deferred processing when mutations have completed
    title_node: Option<usize>,
    style_nodes: HashSet<usize>,
    form_nodes: HashSet<usize>,

    /// Whether an element/attribute that affect animation status has been seen
    recompute_is_animating: bool,

    /// The (latest) node which has been mounted in and had autofocus=true, if any
    #[cfg(feature = "autofocus")]
    node_to_autofocus: Option<usize>,
}

impl Drop for DocumentMutator<'_> {
    fn drop(&mut self) {
        self.flush(); // Defined at bottom of file
    }
}

impl DocumentMutator<'_> {
    pub fn new<'doc>(doc: &'doc mut BaseDocument) -> DocumentMutator<'doc> {
        DocumentMutator {
            doc,
            eager_op_queue: Vec::new(),
            title_node: None,
            style_nodes: HashSet::new(),
            form_nodes: HashSet::new(),
            recompute_is_animating: false,
            #[cfg(feature = "autofocus")]
            node_to_autofocus: None,
        }
    }

    // Query methods

    pub fn node_has_parent(&self, node_id: usize) -> bool {
        self.doc.nodes[node_id].parent.is_some()
    }

    pub fn previous_sibling_id(&self, node_id: usize) -> Option<usize> {
        self.doc.nodes[node_id].backward(1).map(|node| node.id)
    }

    pub fn next_sibling_id(&self, node_id: usize) -> Option<usize> {
        self.doc.nodes[node_id].forward(1).map(|node| node.id)
    }

    pub fn parent_id(&self, node_id: usize) -> Option<usize> {
        self.doc.nodes[node_id].parent
    }

    pub fn last_child_id(&self, node_id: usize) -> Option<usize> {
        self.doc.nodes[node_id].children.last().copied()
    }

    pub fn child_ids(&self, node_id: usize) -> Vec<usize> {
        self.doc.nodes[node_id].children.clone()
    }

    pub fn element_name(&self, node_id: usize) -> Option<&QualName> {
        self.doc.nodes[node_id].element_data().map(|el| &el.name)
    }

    pub fn node_at_path(&self, start_node_id: usize, path: &[u8]) -> usize {
        let mut current = &self.doc.nodes[start_node_id];
        for i in path {
            let new_id = current.children[*i as usize];
            current = &self.doc.nodes[new_id];
        }
        current.id
    }

    // Node creation methods

    pub fn create_comment_node(&mut self) -> usize {
        self.doc.create_node(NodeData::Comment)
    }

    pub fn create_text_node(&mut self, text: &str) -> usize {
        self.doc.create_text_node(text)
    }

    pub fn create_element(&mut self, name: QualName, attrs: Vec<Attribute>) -> usize {
        let mut data = ElementData::new(name, attrs);
        data.flush_style_attribute(self.doc.guard(), &self.doc.url.url_extra_data());

        let id = self.doc.create_node(NodeData::Element(data));
        let node = self.doc.get_node_mut(id).unwrap();

        // Initialise style data
        *node.stylo_element_data.ensure_init_mut() = style::data::ElementData {
            damage: ALL_DAMAGE,
            ..Default::default()
        };

        id
    }

    /// <https://dom.spec.whatwg.org/#concept-node-clone>
    pub fn deep_clone_node(&mut self, node_id: usize) -> usize {
        self.doc.deep_clone_node(node_id)
    }

    // Node mutation methods

    pub fn set_node_text(&mut self, node_id: usize, value: &str) {
        let node = &mut self.doc.nodes[node_id];

        let text = match node.data {
            NodeData::Text(ref mut text) => text,
            // TODO: otherwise this is basically element.textContent which is a bit different - need to parse as html
            _ => return,
        };

        let changed = text.content != value;
        if changed {
            text.content.clear();
            text.content.push_str(value);
            node.insert_damage(ALL_DAMAGE);
            // Mark ancestors dirty so the style traversal visits this subtree.
            // Without this, the traversal may skip nodes with pending damage.
            node.mark_ancestors_dirty();
            let parent_id = node.parent;

            // Also insert damage on the parent element, since text content changes
            // affect the parent's layout (text may wrap differently, change size, etc.)
            if let Some(parent_id) = parent_id {
                let parent = &mut self.doc.nodes[parent_id];
                parent.insert_damage(ALL_DAMAGE);
            }

            self.maybe_record_node(parent_id);
        }
    }

    pub fn append_text_to_node(&mut self, node_id: usize, text: &str) -> Result<(), AppendTextErr> {
        let node = &mut self.doc.nodes[node_id];
        node.insert_damage(ALL_DAMAGE);
        node.mark_ancestors_dirty();
        match node.text_data_mut() {
            Some(data) => {
                data.content += text;
                Ok(())
            }
            None => Err(AppendTextErr::NotTextNode),
        }
    }

    pub fn add_attrs_if_missing(&mut self, node_id: usize, attrs: Vec<Attribute>) {
        let node = &mut self.doc.nodes[node_id];
        node.insert_damage(ALL_DAMAGE);
        let element_data = node.element_data_mut().expect("Not an element");

        let existing_names = element_data
            .attrs
            .iter()
            .map(|e| e.name.clone())
            .collect::<HashSet<_>>();

        for attr in attrs
            .into_iter()
            .filter(|attr| !existing_names.contains(&attr.name))
        {
            self.set_attribute(node_id, attr.name, &attr.value);
        }
    }

    /// <https://dom.spec.whatwg.org/#concept-element-attributes-set-value>
    pub fn set_attribute(&mut self, node_id: usize, name: QualName, value: &str) {
        self.doc.snapshot_node(node_id);

        let node = &mut self.doc.nodes[node_id];
        if let Some(mut data) = node.stylo_element_data.get_mut() {
            data.hint |= RestyleHint::restyle_subtree();
            data.damage.insert(ALL_DAMAGE);
        }

        // TODO: make this fine grained / conditional based on ElementSelectorFlags
        let parent = node.parent;
        if let Some(parent_id) = parent {
            let parent = &mut self.doc.nodes[parent_id];
            if let Some(mut data) = parent.stylo_element_data.get_mut() {
                data.hint |= RestyleHint::restyle_subtree();
            }
        }

        // Mark ancestors dirty so the style traversal visits this subtree.
        // Without this, the traversal may skip nodes with pending RestyleHint/damage
        // because it uses dirty_descendants flags to determine which subtrees to visit.
        self.doc.nodes[node_id].mark_ancestors_dirty();

        let node = &mut self.doc.nodes[node_id];

        let NodeData::Element(ref mut element) = node.data else {
            return;
        };

        // If element is a CustomWidget, then Ccall attribute_changed on it
        #[cfg(feature = "custom-widget")]
        if let SpecialElementData::CustomWidget(widget_data) = &mut element.special_data {
            let old_value = element.attrs.get(&name).as_ref().map(|attr| &*attr.value);
            widget_data
                .widget
                .attribute_changed(&name.local, old_value, Some(value));
        }

        let old_value = element
            .attrs
            .get(&name)
            .map(|attribute| attribute.value.to_string());
        element.attrs.set(name.clone(), value);
        let attribute_change_steps = self.doc.mutation_hooks.clone();
        // <https://dom.spec.whatwg.org/#concept-element-attributes-change-ext>
        // Step 3: "Run the attribute change steps with `element`, `attribute`'s local name, `oldValue`, `newValue`, and `attribute`'s namespace."
        attribute_change_steps.attribute_change_steps(
            node_id,
            element,
            name.local.as_ref(),
            old_value.as_deref(),
            Some(value),
            name.ns.as_ref(),
        );

        let tag = &element.name.local;
        let attr = &name.local;

        if *attr == local_name!("id") {
            element.id = Some(Atom::from(value))
        }

        if *attr == local_name!("value") {
            if let Some(input_data) = element.text_input_data_mut() {
                // Update text input value
                input_data.set_text(
                    &mut self.doc.font_ctx.lock().unwrap(),
                    &mut self.doc.layout_ctx,
                    value,
                );
            }
            return;
        }

        if *attr == local_name!("style") {
            element.flush_style_attribute(&self.doc.guard, &self.doc.url.url_extra_data());
            node.mark_style_attr_updated();
            return;
        }

        if *attr == local_name!("disabled") && element.can_be_disabled() {
            node.disable();
            return;
        }

        // If node if not in the document, then don't apply any special behaviours
        // and simply set the attribute value
        if !node.flags.is_in_document() {
            return;
        }

        if (tag, attr) == tag_and_attr!("input", "checked") {
            set_input_checked_state(element, value.to_string());
        } else if (tag, attr) == tag_and_attr!("img", "src") {
            self.load_image(node_id);
        } else if (tag, attr) == tag_and_attr!("canvas", "src") {
            self.load_custom_paint_src(node_id);
        } else if (tag, attr) == tag_and_attr!("link", "href") {
            self.load_linked_stylesheet(node_id);
        }
    }

    /// <https://dom.spec.whatwg.org/#concept-element-attributes-remove>
    pub fn clear_attribute(&mut self, node_id: usize, name: QualName) {
        self.doc.snapshot_node(node_id);

        let node = &mut self.doc.nodes[node_id];

        if let Some(mut data) = node.stylo_element_data.get_mut() {
            data.hint |= RestyleHint::restyle_subtree();
            data.damage.insert(ALL_DAMAGE);
        }

        // Mark ancestors dirty so the style traversal visits this subtree.
        // Without this, the traversal may skip nodes with pending RestyleHint/damage.
        node.mark_ancestors_dirty();

        let Some(element) = node.element_data_mut() else {
            return;
        };

        let removed_attr = element.attrs.remove(&name);
        let had_attr = removed_attr.is_some();
        if !had_attr {
            return;
        }
        let removed_value = removed_attr
            .as_ref()
            .map(|attribute| attribute.value.to_string());
        let attribute_change_steps = self.doc.mutation_hooks.clone();
        // <https://dom.spec.whatwg.org/#concept-element-attributes-change-ext>
        // Step 3: "Run the attribute change steps with `element`, `attribute`'s local name, `oldValue`, `newValue`, and `attribute`'s namespace."
        attribute_change_steps.attribute_change_steps(
            node_id,
            element,
            name.local.as_ref(),
            removed_value.as_deref(),
            None,
            name.ns.as_ref(),
        );

        // If element is a CustomWidget, then call attribute_changed on it
        #[cfg(feature = "custom-widget")]
        if let SpecialElementData::CustomWidget(widget_data) = &mut element.special_data {
            let old_value = removed_attr.as_ref().map(|attr| &*attr.value);
            widget_data
                .widget
                .attribute_changed(&name.local, old_value, None);
        }

        if name.local == local_name!("id") {
            element.id = None;
        }

        // Update text input value
        if name.local == local_name!("value") {
            if let Some(input_data) = element.text_input_data_mut() {
                input_data.set_text(
                    &mut self.doc.font_ctx.lock().unwrap(),
                    &mut self.doc.layout_ctx,
                    "",
                );
            }
        }

        let tag = &element.name.local;
        let attr = &name.local;

        if *attr == local_name!("disabled") && element.can_be_disabled() {
            node.enable();
            return;
        }

        if *attr == local_name!("style") {
            element.flush_style_attribute(&self.doc.guard, &self.doc.url.url_extra_data());
            node.mark_style_attr_updated();
        } else if (tag, attr) == tag_and_attr!("canvas", "src") {
            self.recompute_is_animating = true;
        } else if (tag, attr) == tag_and_attr!("link", "href") {
            self.unload_stylesheet(node_id);
        }
    }

    pub fn set_style_property(&mut self, node_id: usize, name: &str, value: &str) {
        self.doc.set_style_property(node_id, name, value)
    }

    pub fn remove_style_property(&mut self, node_id: usize, name: &str) {
        self.doc.remove_style_property(node_id, name)
    }

    pub fn set_sub_document(&mut self, node_id: usize, sub_document: Box<dyn Document>) {
        self.doc.set_sub_document(node_id, sub_document)
    }

    pub fn remove_sub_document(&mut self, node_id: usize) {
        self.doc.remove_sub_document(node_id)
    }

    #[cfg(feature = "custom-widget")]
    pub fn set_custom_widget(&mut self, node_id: usize, widget: Box<dyn crate::Widget>) {
        self.doc.set_custom_widget(node_id, widget)
    }

    #[cfg(feature = "custom-widget")]
    pub fn remove_custom_widget(&mut self, node_id: usize) {
        self.doc.remove_custom_widget(node_id)
    }

    /// <https://dom.spec.whatwg.org/#concept-node-remove>
    pub fn remove_node(&mut self, node_id: usize) {
        let old_ancestor = self.doc.nodes[node_id].parent;
        // Update child_idx values
        if let Some(parent_id) = old_ancestor {
            let parent = &mut self.doc.nodes[parent_id];
            parent.insert_damage(ALL_DAMAGE);
            // Mark ancestors dirty so the style traversal visits this subtree.
            parent.mark_ancestors_dirty();
            parent.children.retain(|id| *id != node_id);
            self.maybe_record_node(parent_id);
            self.doc.nodes[node_id].parent = None;
        }

        self.process_removed_subtree(node_id, old_ancestor);
    }

    pub fn remove_and_drop_node(&mut self, node_id: usize) -> Option<Node> {
        let old_ancestor = self.doc.nodes[node_id].parent;
        self.process_removed_subtree(node_id, old_ancestor);

        let node = self.doc.drop_node_ignoring_parent(node_id);

        // Update child_idx values
        if let Some(parent_id) = node.as_ref().and_then(|node| node.parent) {
            let parent = &mut self.doc.nodes[parent_id];
            parent.insert_damage(ALL_DAMAGE);
            let parent_is_in_doc = parent.flags.is_in_document();

            // TODO: make this fine grained / conditional based on ElementSelectorFlags
            if parent_is_in_doc {
                if let Some(mut data) = parent.stylo_element_data.get_mut() {
                    data.hint |= RestyleHint::restyle_subtree();
                }
                // Mark ancestors dirty so the style traversal visits this subtree.
                parent.mark_ancestors_dirty();
            }

            parent.children.retain(|id| *id != node_id);
            self.maybe_record_node(parent_id);
        }

        node
    }

    pub fn remove_and_drop_all_children(&mut self, node_id: usize) {
        let parent = &mut self.doc.nodes[node_id];
        let parent_is_in_doc = parent.flags.is_in_document();

        // TODO: make this fine grained / conditional based on ElementSelectorFlags
        if parent_is_in_doc {
            if let Some(mut data) = parent.stylo_element_data.get_mut() {
                data.hint |= RestyleHint::restyle_subtree();
            }
            // Mark ancestors dirty so the style traversal visits this subtree.
            parent.mark_ancestors_dirty();
        }

        let children = mem::take(&mut parent.children);
        for child_id in children {
            let old_ancestor = self.doc.nodes[child_id].parent;
            self.process_removed_subtree(child_id, old_ancestor);
            let _ = self.doc.drop_node_ignoring_parent(child_id);
        }
        self.maybe_record_node(node_id);
    }

    // Tree mutation methods
    pub fn remove_node_if_unparented(&mut self, node_id: usize) {
        if let Some(node) = self.doc.get_node(node_id) {
            if node.parent.is_none() {
                self.remove_and_drop_node(node_id);
            }
        }
    }

    /// <https://dom.spec.whatwg.org/#concept-node-insert>
    pub fn append_children(&mut self, parent_id: usize, child_ids: &[usize]) {
        self.add_children_to_parent(parent_id, child_ids, &|parent, child_ids| {
            parent.children.extend_from_slice(child_ids);
        });
    }

    /// <https://dom.spec.whatwg.org/#concept-node-insert>
    pub fn insert_nodes_before(&mut self, anchor_node_id: usize, new_node_ids: &[usize]) {
        let parent_id = self.doc.nodes[anchor_node_id].parent.unwrap();
        self.add_children_to_parent(parent_id, new_node_ids, &|parent, child_ids| {
            let node_child_idx = parent.index_of_child(anchor_node_id).unwrap();
            parent
                .children
                .splice(node_child_idx..node_child_idx, child_ids.iter().copied());
        });
    }

    /// <https://dom.spec.whatwg.org/#concept-node-insert>
    fn add_children_to_parent(
        &mut self,
        parent_id: usize,
        child_ids: &[usize],
        insert_children_fn: &dyn Fn(&mut Node, &[usize]),
    ) {
        let new_parent = &mut self.doc.nodes[parent_id];
        new_parent.insert_damage(ALL_DAMAGE);
        let new_parent_is_in_doc = new_parent.flags.is_in_document();

        // TODO: make this fine grained / conditional based on ElementSelectorFlags
        if new_parent_is_in_doc {
            if let Some(mut data) = new_parent.stylo_element_data.get_mut() {
                data.hint |= RestyleHint::restyle_subtree();
            }
            // Mark ancestors dirty so the style traversal visits this subtree.
            new_parent.mark_ancestors_dirty();
        }

        insert_children_fn(new_parent, child_ids);

        for child_id in child_ids.iter().copied() {
            let child = &mut self.doc.nodes[child_id];
            let old_parent_id = child.parent.replace(parent_id);

            let child_was_in_doc = child.flags.is_in_document();
            if new_parent_is_in_doc != child_was_in_doc {
                self.process_added_subtree(child_id);
            }

            if let Some(old_parent_id) = old_parent_id {
                let old_parent = &mut self.doc.nodes[old_parent_id];
                old_parent.insert_damage(ALL_DAMAGE);

                // TODO: make this fine grained / conditional based on ElementSelectorFlags
                if child_was_in_doc {
                    if let Some(mut data) = old_parent.stylo_element_data.get_mut() {
                        data.hint |= RestyleHint::restyle_subtree();
                    }
                    // Mark ancestors dirty so the style traversal visits this subtree.
                    old_parent.mark_ancestors_dirty();
                }

                old_parent.children.retain(|id| *id != child_id);
                self.maybe_record_node(old_parent_id);
            }
        }

        self.maybe_record_node(parent_id);

        // Step 7.7.1 of the DOM insert algorithm runs the insertion steps for
        // every shadow-including inclusive descendant of each inserted node.
        // <https://dom.spec.whatwg.org/#concept-node-insert-ext>
        for child_id in child_ids.iter().copied() {
            self.run_subtree_step(child_id, |hooks, node_id, element| {
                hooks.insertion_steps(node_id, element);
            });
        }

        // Step 12 of the DOM insert algorithm runs the post-connection steps
        // for each connected node in `staticNodeList`, after every insertion
        // step.
        // <https://dom.spec.whatwg.org/#concept-node-post-connection-ext>
        for child_id in child_ids.iter().copied() {
            if self.doc.nodes[child_id].flags.is_in_document() {
                self.run_subtree_step(child_id, |hooks, node_id, element| {
                    hooks.post_connection_steps(node_id, element);
                });
            }
        }
    }

    fn run_subtree_step(
        &self,
        node_id: usize,
        mut step: impl FnMut(&dyn MutationHooks, usize, Option<&ElementData>),
    ) {
        let mut node_ids = Vec::new();
        collect_subtree_node_ids(&self.doc.nodes, node_id, &mut node_ids);
        for node_id in node_ids {
            step(
                &*self.doc.mutation_hooks,
                node_id,
                self.doc.nodes[node_id].element_data(),
            );
        }
    }

    // Tree mutation methods (that defer to other methods)
    pub fn insert_nodes_after(&mut self, anchor_node_id: usize, new_node_ids: &[usize]) {
        match self.next_sibling_id(anchor_node_id) {
            Some(id) => self.insert_nodes_before(id, new_node_ids),
            None => {
                let parent_id = self.parent_id(anchor_node_id).unwrap();
                self.append_children(parent_id, new_node_ids)
            }
        }
    }

    /// <https://dom.spec.whatwg.org/#concept-node-insert>
    pub fn reparent_children(&mut self, old_parent_id: usize, new_parent_id: usize) {
        let child_ids = std::mem::take(&mut self.doc.nodes[old_parent_id].children);
        self.maybe_record_node(old_parent_id);
        self.append_children(new_parent_id, &child_ids);
    }

    /// <https://dom.spec.whatwg.org/#concept-node-replace>
    pub fn replace_node_with(&mut self, anchor_node_id: usize, new_node_ids: &[usize]) {
        self.insert_nodes_before(anchor_node_id, new_node_ids);
        self.remove_node(anchor_node_id);
    }
}

impl<'doc> DocumentMutator<'doc> {
    pub fn flush(&mut self) {
        if self.recompute_is_animating {
            self.doc.has_canvas = self.doc.compute_has_canvas();
        }

        if let Some(id) = self.title_node {
            let title = self.doc.nodes[id].text_content();
            self.doc.shell_provider.set_window_title(title);
        }

        // Add/Update inline stylesheets (<style> elements)
        for id in self.style_nodes.drain() {
            self.doc.process_style_element(id);
        }

        for id in self.form_nodes.drain() {
            self.doc.reset_form_owner(id);
        }

        #[cfg(feature = "autofocus")]
        if let Some(node_id) = self.node_to_autofocus.take() {
            if self.doc.get_node(node_id).is_some() {
                self.doc.set_focus_to(node_id);
            }
        }
    }

    pub fn set_inner_html(&mut self, node_id: usize, html: &str) {
        self.remove_and_drop_all_children(node_id);
        self.doc
            .html_parser_provider
            .clone()
            .parse_inner_html(self, node_id, html);
    }

    fn flush_eager_ops(&mut self) {
        let mut ops = mem::take(&mut self.eager_op_queue);
        for op in ops.drain(0..) {
            match op {
                SpecialOp::LoadImage(node_id) => self.load_image(node_id),
                SpecialOp::LoadStylesheet(node_id) => self.load_linked_stylesheet(node_id),
                SpecialOp::UnloadStylesheet(node_id) => self.unload_stylesheet(node_id),
                SpecialOp::LoadCustomPaintSource(node_id) => self.load_custom_paint_src(node_id),
                SpecialOp::ProcessButtonInput(node_id) => self.process_button_input(node_id),
                SpecialOp::UnloadSubDocument(node_id) => self.remove_sub_document(node_id),
                #[cfg(feature = "custom-widget")]
                SpecialOp::UnloadCustomWidget(node_id) => self.remove_custom_widget(node_id),
            }
        }

        // Queue is empty, but put Vec back anyway so allocation can be reused.
        self.eager_op_queue = ops;
    }

    fn process_added_subtree(&mut self, node_id: usize) {
        self.doc.iter_subtree_mut(node_id, |node_id, doc| {
            let node = &mut doc.nodes[node_id];
            node.flags.set(NodeFlags::IS_IN_DOCUMENT, true);
            node.insert_damage(ALL_DAMAGE);

            // If the node has an "id" attribute, store it in the ID map.
            if let Some(id_attr) = node.attr(local_name!("id")) {
                doc.nodes_to_id.insert(id_attr.to_string(), node_id);
            }

            let NodeData::Element(ref mut element) = node.data else {
                return;
            };

            // Custom post-processing by element tag name
            let tag = element.name.local.as_ref();
            match tag {
                "title" => self.title_node = Some(node_id),
                "link" => self.eager_op_queue.push(SpecialOp::LoadStylesheet(node_id)),
                "img" => self.eager_op_queue.push(SpecialOp::LoadImage(node_id)),
                "canvas" => self
                    .eager_op_queue
                    .push(SpecialOp::LoadCustomPaintSource(node_id)),
                "style" => {
                    self.style_nodes.insert(node_id);
                }
                "button" | "fieldset" | "input" | "select" | "textarea" | "object" | "output" => {
                    self.eager_op_queue
                        .push(SpecialOp::ProcessButtonInput(node_id));
                    self.form_nodes.insert(node_id);
                }
                _ => {}
            }

            #[cfg(feature = "autofocus")]
            if node.is_focussable() {
                if let NodeData::Element(ref element) = node.data {
                    if let Some(value) = element.attr(local_name!("autofocus")) {
                        if value == "true" {
                            self.node_to_autofocus = Some(node_id);
                        }
                    }
                }
            }
        });

        self.flush_eager_ops();
    }

    /// <https://dom.spec.whatwg.org/#concept-node-remove>
    fn process_removed_subtree(&mut self, node_id: usize, old_ancestor: Option<usize>) {
        let hooks = self.doc.mutation_hooks.clone();
        let subtree_root = node_id;
        let old_ancestor_element = old_ancestor
            .and_then(|ancestor_id| self.doc.nodes.get(ancestor_id))
            .and_then(|ancestor| ancestor.element_data());
        let mut node_ids = Vec::new();
        collect_subtree_node_ids(&self.doc.nodes, node_id, &mut node_ids);
        for removed_id in node_ids {
            // Step 11: "Run the removing steps with `node`, true, and `parent`."
            // Step 14.1: "Run the removing steps with `descendant`, false, and `parent`."
            // <https://dom.spec.whatwg.org/#concept-node-remove-ext>
            hooks.removing_steps(
                removed_id,
                self.doc.nodes[removed_id].element_data(),
                removed_id == subtree_root,
                old_ancestor_element,
            );
        }
        self.doc.iter_subtree_mut(node_id, |node_id, doc| {
            let node = &mut doc.nodes[node_id];
            node.flags.set(NodeFlags::IS_IN_DOCUMENT, false);

            // Clear hover state if this node was being hovered.
            // This prevents stale hover_node_id references.
            if doc.hover_node_id == Some(node_id) {
                doc.hover_node_id = None;
                doc.hover_node_is_text = false;
            }

            // Clear active state if this node was active
            // This prevents stale active_node_id references.
            if doc.active_node_id == Some(node_id) {
                doc.active_node_id = None;
            }

            // Remove any snapshot for this node to prevent stale snapshot references
            // during style invalidation.
            if node.has_snapshot {
                let opaque_id = style::dom::TNode::opaque(&&*node);
                doc.snapshots.remove(&opaque_id);
                node.has_snapshot = false;
            }

            // If the node has an "id" attribute remove it from the ID map.
            if let Some(id_attr) = node.attr(local_name!("id")) {
                doc.nodes_to_id.remove(id_attr);
            }

            let NodeData::Element(ref mut element) = node.data else {
                return;
            };

            match &element.special_data {
                SpecialElementData::SubDocument(_) => {
                    self.eager_op_queue
                        .push(SpecialOp::UnloadSubDocument(node_id));
                }
                #[cfg(feature = "custom-widget")]
                SpecialElementData::CustomWidget(_) => {
                    self.eager_op_queue
                        .push(SpecialOp::UnloadCustomWidget(node_id));
                }
                SpecialElementData::Stylesheet(_) => self
                    .eager_op_queue
                    .push(SpecialOp::UnloadStylesheet(node_id)),
                SpecialElementData::Image(_) => {}
                SpecialElementData::Canvas(_) => {
                    self.recompute_is_animating = true;
                }
                SpecialElementData::TableRoot(_) => {}
                SpecialElementData::TextInput(_) => {}
                SpecialElementData::CheckboxInput(_) => {}
                #[cfg(feature = "file_input")]
                SpecialElementData::FileInput(_) => {}
                SpecialElementData::None => {}
            }
        });

        self.flush_eager_ops();
    }

    fn maybe_record_node(&mut self, node_id: impl Into<Option<usize>>) {
        let Some(node_id) = node_id.into() else {
            return;
        };

        let Some(tag_name) = self.doc.nodes[node_id]
            .data
            .downcast_element()
            .map(|elem| &elem.name.local)
        else {
            return;
        };

        match tag_name.as_ref() {
            "title" => self.title_node = Some(node_id),
            "style" => {
                self.style_nodes.insert(node_id);
            }
            _ => {}
        }
    }

    fn load_linked_stylesheet(&mut self, target_id: usize) {
        let node = &self.doc.nodes[target_id];

        let mut is_in_head = false;
        let mut parent_id = node.parent;
        while let Some(id) = parent_id
            && !is_in_head
        {
            let parent = &self.doc.nodes[id];
            is_in_head |= parent.data.is_element_with_tag_name(&local_name!("head"));
            parent_id = parent.parent;
        }

        let rel_attr = node.attr(local_name!("rel"));
        let href_attr = node.attr(local_name!("href"));

        let (Some(rels), Some(href)) = (rel_attr, href_attr) else {
            return;
        };
        if !rels.split_ascii_whitespace().any(|rel| rel == "stylesheet") {
            return;
        }

        let url = self.doc.resolve_url(href);
        let request_id = if is_in_head {
            // The document load event waits for a stylesheet linked from the
            // head; the node id identifies the pending resource while the
            // embedder loads it.
            self.doc.pending_critical_resources.insert(node.id);
            Some(node.id)
        } else {
            None
        };
        let hooks = self.doc.resource_hooks.clone();
        hooks.resource_request(ResourceRequest::Stylesheet {
            node_id: node.id,
            url,
            request_id,
        });
    }

    fn unload_stylesheet(&mut self, node_id: usize) {
        let node = &mut self.doc.nodes[node_id];
        let Some(element) = node.element_data_mut() else {
            unreachable!();
        };
        let SpecialElementData::Stylesheet(stylesheet) = element.special_data.take() else {
            unreachable!();
        };

        let guard = self.doc.guard.read();
        self.doc.stylist.remove_stylesheet(stylesheet, &guard);
        self.doc
            .stylist
            .force_stylesheet_origins_dirty(OriginSet::all());

        self.doc.nodes_to_stylesheet.remove(&node_id);
    }

    fn load_image(&mut self, target_id: usize) {
        // `<img>` elements whose pixels the embedding application decodes and
        // composites itself are driven by the HTML "update the image data"
        // algorithm, not by the document's layout-time image loader.
        if self.doc.embedded_images() {
            return;
        }
        let node = &self.doc.nodes[target_id];
        let Some(raw_src) = node.attr(local_name!("src")) else {
            return;
        };
        if raw_src.is_empty() {
            return;
        }
        let src = self.doc.resolve_url(raw_src);
        let hooks = self.doc.resource_hooks.clone();
        hooks.resource_request(ResourceRequest::Image {
            node_id: target_id,
            url: src,
            image_type: ImageType::Image,
        });
    }

    fn load_custom_paint_src(&mut self, target_id: usize) {
        let node = &mut self.doc.nodes[target_id];
        if let Some(raw_src) = node.attr(local_name!("src")) {
            if let Ok(custom_paint_source_id) = raw_src.parse::<u64>() {
                self.recompute_is_animating = true;
                let canvas_data = SpecialElementData::Canvas(CanvasData {
                    custom_paint_source_id,
                });
                node.element_data_mut().unwrap().special_data = canvas_data;
            }
        }
    }

    fn process_button_input(&mut self, target_id: usize) {
        let node = &self.doc.nodes[target_id];
        let Some(data) = node.element_data() else {
            return;
        };

        let tagname = data.name.local.as_ref();
        let type_attr = data.attr(local_name!("type"));
        let value = data.attr(local_name!("value"));

        // Add content of "value" attribute as a text node child if:
        //   - Tag name is
        if let ("input", Some("button" | "submit" | "reset"), Some(value)) =
            (tagname, type_attr, value)
        {
            let value = value.to_string();
            let id = self.create_text_node(&value);
            self.append_children(target_id, &[id]);
            return;
        }
        #[cfg(feature = "file_input")]
        if let ("input", Some("file")) = (tagname, type_attr) {
            let button_id = self.create_element(
                qual_name!("button", html),
                vec![
                    Attribute {
                        name: qual_name!("type", html),
                        value: "button".to_string(),
                    },
                    Attribute {
                        name: qual_name!("tabindex", html),
                        value: "-1".to_string(),
                    },
                ],
            );
            let label_id = self.create_element(qual_name!("label", html), vec![]);
            let text_id = self.create_text_node("No File Selected");
            let button_text_id = self.create_text_node("Browse");
            self.append_children(target_id, &[button_id, label_id]);
            self.append_children(label_id, &[text_id]);
            self.append_children(button_id, &[button_text_id]);
        }
    }
}

/// Set 'checked' state on an input based on given attributevalue
fn set_input_checked_state(element: &mut ElementData, value: String) {
    let Ok(checked) = value.parse() else {
        return;
    };
    match element.special_data {
        SpecialElementData::CheckboxInput(ref mut checked_mut) => *checked_mut = checked,
        // If we have just constructed the element, set the node attribute,
        // and NodeSpecificData will be created from that later
        // this simulates the checked attribute being set in html,
        // and the element's checked property being set from that
        SpecialElementData::None => element.attrs.push(Attribute {
            name: qual_name!("checked", html),
            value: checked.to_string(),
        }),
        _ => {}
    }
}

/// Type that allows mutable access to the viewport
/// And syncs it back to stylist on drop.
pub struct ViewportMut<'doc> {
    doc: &'doc mut BaseDocument,
    initial_viewport: Viewport,
}
impl ViewportMut<'_> {
    pub fn new(doc: &mut BaseDocument) -> ViewportMut<'_> {
        let initial_viewport = doc.viewport.clone();
        ViewportMut {
            doc,
            initial_viewport,
        }
    }
}
impl Deref for ViewportMut<'_> {
    type Target = Viewport;

    fn deref(&self) -> &Self::Target {
        &self.doc.viewport
    }
}
impl DerefMut for ViewportMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.doc.viewport
    }
}
impl Drop for ViewportMut<'_> {
    fn drop(&mut self) {
        if self.doc.viewport == self.initial_viewport {
            return;
        }

        self.doc.set_stylist_device(make_device(
            &self.doc.viewport,
            self.doc.media_type.clone(),
            self.doc.font_ctx.clone(),
        ));
        self.doc.scroll_viewport_by(0.0, 0.0); // Clamp scroll offset

        let scale_has_changed =
            self.doc.viewport().scale_f64() != self.initial_viewport.scale_f64();
        if scale_has_changed {
            self.doc.invalidate_inline_contexts();
            self.doc.shell_provider.request_redraw();
        }
    }
}

#[cfg(test)]
mod test {
    use style::media_queries::MediaType;
    use style_dom::ElementState;

    use crate::{Attribute, BaseDocument, DocumentConfig, ElementData, NodeData, qual_name};

    #[test]
    fn media_type_defaults_to_screen() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        assert_eq!(*document.media_type(), MediaType::screen());
        assert_eq!(document.stylist_device().media_type(), MediaType::screen());
    }

    #[test]
    fn media_type_honors_config() {
        let mut document = BaseDocument::new(DocumentConfig {
            media_type: Some(MediaType::print()),
            ..Default::default()
        });
        assert_eq!(*document.media_type(), MediaType::print());
        assert_eq!(document.stylist_device().media_type(), MediaType::print());
    }

    #[test]
    fn set_media_type_updates_stylist_device() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        assert_eq!(document.stylist_device().media_type(), MediaType::screen());

        document.set_media_type(MediaType::print());
        assert_eq!(*document.media_type(), MediaType::print());
        assert_eq!(document.stylist_device().media_type(), MediaType::print());
    }

    #[test]
    fn mutator_remove_disabled() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let id = document.create_node(NodeData::Element(ElementData::new(
            qual_name!("button"),
            vec![Attribute {
                name: qual_name!("disabled"),
                value: "".into(),
            }],
        )));

        let node = document.get_node(id).unwrap();
        assert!(
            node.element_state.contains(ElementState::DISABLED),
            "form node is disabled"
        );
        assert!(
            !node.element_state.contains(ElementState::ENABLED),
            "form node is not enabled yet"
        );

        let mut mutator = document.mutate();
        mutator.clear_attribute(id, qual_name!("disabled"));
        drop(mutator);

        let node = document.get_node(id).unwrap();
        assert!(
            !node.element_state.contains(ElementState::DISABLED),
            "form node is no longer disabled"
        );
        assert!(
            node.element_state.contains(ElementState::ENABLED),
            "form node is enabled"
        );
    }

    #[test]
    fn mutator_set_disabled() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let id = document.create_node(NodeData::Element(ElementData::new(
            qual_name!("button"),
            vec![],
        )));

        let node = document.get_node(id).unwrap();
        assert!(
            !node.element_state.contains(ElementState::DISABLED),
            "form node is not disabled"
        );
        assert!(
            node.element_state.contains(ElementState::ENABLED),
            "form node is enabled"
        );

        let mut mutator = document.mutate();
        mutator.set_attribute(id, qual_name!("disabled"), "");
        drop(mutator);

        let node = document.get_node(id).unwrap();

        assert!(
            node.element_state.contains(ElementState::DISABLED),
            "form node is disabled"
        );
        assert!(
            !node.element_state.contains(ElementState::ENABLED),
            "form node is no longer enabled enabled"
        );
    }

    #[test]
    fn mutator_set_disabled_invalid_node() {
        let mut document = BaseDocument::new(DocumentConfig::default());
        let id = document.create_node(NodeData::Element(ElementData::new(qual_name!("a"), vec![])));

        let node = document.get_node(id).unwrap();
        assert!(
            !node.element_state.contains(ElementState::DISABLED),
            "form node is not disabled"
        );
        assert!(
            !node.element_state.contains(ElementState::ENABLED),
            "form node is enabled"
        );

        let mut mutator = document.mutate();
        mutator.set_attribute(id, qual_name!("disabled"), "");
        drop(mutator);

        let node = document.get_node(id).unwrap();
        assert!(
            !node.element_state.contains(ElementState::DISABLED),
            "form node is not disabled"
        );
        assert!(
            !node.element_state.contains(ElementState::ENABLED),
            "form node is enabled"
        );
    }
}

fn collect_subtree_node_ids(nodes: &Slab<Node>, node_id: usize, node_ids: &mut Vec<usize>) {
    node_ids.push(node_id);
    for child_id in nodes[node_id].children.iter().copied() {
        collect_subtree_node_ids(nodes, child_id, node_ids);
    }
}
