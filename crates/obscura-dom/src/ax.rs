//! Accessibility-tree view of a DOM tree.
//!
//! This module provides:
//! - [`AXNode`] — one node in the accessibility tree with role, name, and hidden status.
//! - [`map_role`], [`compute_name`] — spec-compatible role and accessible-name computation.
//! - [`node_is_hidden`], [`node_is_hidden_in`] — author-intent hidden detection without layout.
//! - [`ax_tree`] — build the full accessibility tree for a document.
//! - [`visible_text`] — extract the visible, non-decorative text from a subtree.
//!
//! Hidden detection uses only author-intent signals (attributes, inline style, known
//! utility classes) and requires **no** CSS cascade or layout engine. This makes it
//! deterministic, testable, and fast.

use crate::tree::{Attribute, DomTree, Node, NodeData, NodeId};

// ============================================================================
// Public types
// ============================================================================

/// One entry in the accessibility tree.
///
/// Fields mirror the subset of the CDP `AXNode` shape that is useful for
/// text-extraction callers. CDP-specific properties (e.g. `focusable`,
/// `editable`) are left to the serialization layer in `obscura-cdp`.
#[derive(Debug, Clone, PartialEq)]
pub struct AXNode {
    /// Sequential, stable id for this `ax_tree()` call. Not persistent across calls.
    pub id: u32,
    /// Parent AXNode id, if any.
    pub parent_id: Option<u32>,
    /// WAI-ARIA or computed role string ("RootWebArea", "button", "StaticText", etc.).
    pub role: &'static str,
    /// Accessible name, computed per the spec (aria-label → aria-labelledby →
    /// alt → title → placeholder → text-content fallback).
    pub name: Option<String>,
    /// True when the node is hidden by author intent (attributes, inline style,
    /// or known utility classes).
    pub hidden: bool,
    /// True for block-level elements that should separate surrounding text with a
    /// blank line during [`visible_text`] extraction.
    pub is_block: bool,
    /// Back-reference to the underlying DOM node.
    pub dom_id: NodeId,
}

// ============================================================================
// Role mapping
// ============================================================================

/// Map a DOM node to its WAI-ARIA role string.
///
/// Returns an empty string for nodes that should be excluded from the
/// accessibility tree (Doctype, Comment, ProcessingInstruction).
///
/// This is a direct port of the CDP-layer implementation in
/// `obscura-cdp::domains::accessibility::map_role`. The role strings match
/// the Chrome DevTools Protocol conventions exactly.
pub fn map_role(data: &NodeData) -> &'static str {
    match data {
        NodeData::Document => "RootWebArea",
        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.as_ref();

            // Explicit role attribute takes priority over tag semantics.
            if let Some(role_attr) = attrs.iter().find(|a| a.name.local.as_ref() == "role") {
                return match role_attr.value.as_str() {
                    "button" => "button",
                    "link" => "link",
                    "heading" => "heading",
                    "textbox" | "searchbox" => "textbox",
                    "checkbox" => "checkbox",
                    "radio" => "radio",
                    "listbox" => "listbox",
                    "combobox" => "combobox",
                    "list" => "list",
                    "listitem" => "listitem",
                    "navigation" => "navigation",
                    "banner" => "banner",
                    "main" => "main",
                    "complementary" => "complementary",
                    "contentinfo" => "contentinfo",
                    "form" => "form",
                    "table" => "table",
                    "row" => "row",
                    "cell" | "gridcell" => "cell",
                    "img" => "image",
                    "dialog" => "dialog",
                    "alert" => "alert",
                    "tab" => "tab",
                    "tablist" => "tablist",
                    "tabpanel" => "tabpanel",
                    "menu" => "menu",
                    "menuitem" => "menuitem",
                    "toolbar" => "toolbar",
                    "separator" => "separator",
                    "presentation" | "none" => "presentation",
                    _ => "generic",
                };
            }

            match tag {
                "a" => {
                    if attrs.iter().any(|a| a.name.local.as_ref() == "href") {
                        "link"
                    } else {
                        "generic"
                    }
                }
                "button" | "summary" => "button",
                "input" => {
                    let type_attr = attrs
                        .iter()
                        .find(|a| a.name.local.as_ref() == "type")
                        .map(|a| a.value.as_str())
                        .unwrap_or("text");
                    match type_attr {
                        "submit" | "reset" | "button" | "image" => "button",
                        "checkbox" => "checkbox",
                        "radio" => "radio",
                        "range" => "slider",
                        "number" => "spinbutton",
                        "search" => "searchbox",
                        _ => "textbox",
                    }
                }
                "textarea" => "textbox",
                "select" => {
                    if attrs.iter().any(|a| {
                        a.name.local.as_ref() == "multiple"
                            || a.name.local.as_ref() == "size"
                    }) {
                        "listbox"
                    } else {
                        "combobox"
                    }
                }
                "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
                "img" | "svg" => "image",
                "ul" | "ol" | "menu" => "list",
                "li" => "listitem",
                "table" => "table",
                "tr" => "row",
                "td" | "th" => "cell",
                "nav" => "navigation",
                "header" => "banner",
                "main" => "main",
                "footer" => "contentinfo",
                "form" => "form",
                "dialog" => "dialog",
                "hr" => "separator",
                "label" => "LabelText",
                "article" => "article",
                "aside" => "complementary",
                "section" => "region",
                "figure" => "figure",
                "figcaption" => "StaticText",
                "p" | "div" | "span" | "pre" | "blockquote" | "code"
                | "em" | "strong" | "b" | "i" | "u" | "s" | "small"
                | "sub" | "sup" | "mark" | "del" | "ins" => "generic",
                "iframe" => "Iframe",
                _ => "generic",
            }
        }
        NodeData::Text { .. } => "StaticText",
        NodeData::Doctype { .. }
        | NodeData::Comment { .. }
        | NodeData::ProcessingInstruction { .. } => "",
    }
}

// ============================================================================
// Accessible name computation
// ============================================================================

/// Compute the accessible name for a DOM node following the spec priority order.
///
/// Priority: aria-label → aria-labelledby → alt (image) → title →
/// placeholder → element's text content.
pub fn compute_name(dom: &DomTree, node: &Node) -> Option<String> {
    if let NodeData::Element { attrs, .. } = &node.data {
        // aria-label — highest priority
        if let Some(label) = attr_value(attrs, "aria-label") {
            if !label.is_empty() {
                return Some(label.to_string());
            }
        }

        // aria-labelledby — reference by ID
        if let Some(labelledby) = attr_value(attrs, "aria-labelledby") {
            let ids: Vec<&str> = labelledby.split_whitespace().collect();
            let mut name = String::new();
            for id_str in ids {
                if let Some(ref_id) = dom.get_element_by_id(id_str) {
                    name.push_str(&dom.text_content(ref_id));
                    name.push(' ');
                }
            }
            let trimmed = name.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }

        // alt attribute (primarily for images)
        if let Some(alt) = attr_value(attrs, "alt") {
            if !alt.is_empty() {
                return Some(alt.to_string());
            }
        }

        // title attribute
        if let Some(title) = attr_value(attrs, "title") {
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }

        // placeholder
        if let Some(placeholder) = attr_value(attrs, "placeholder") {
            if !placeholder.is_empty() {
                return Some(placeholder.to_string());
            }
        }
    }

    // For text nodes, the name is the text content
    if let NodeData::Text { contents } = &node.data {
        let trimmed = contents.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    None
}

// ============================================================================
// Hidden detection
// ============================================================================

/// CSS utility classes that are known to hide elements.
///
/// This is an opinionated curated list covering the most common frameworks
/// (Tailwind, Bootstrap) and generic conventions. It does not attempt to
/// parse the stylesheet — everything here is a deterministic class-name
/// match on the element's `class` attribute.
const HIDING_CLASSES: &[&str] = &[
    // Tailwind utility classes
    "hidden", "sr-only", "invisible", "collapse", "collapsed",
    "opacity-0",
    // Bootstrap utility classes
    "d-none", "visually-hidden",
    // Generic conventions
    "is-hidden", "is-collapsed",
];

/// Test whether a single DOM node is hidden by author intent.
///
/// Checks (in order):
///   1. `hidden` or `inert` attribute presence
///   2. `aria-hidden="true"` (exact match)
///   3. `role="presentation"` or `role="none"`
///   4. Inline `style` containing `display:none`, `visibility:hidden`,
///      `visibility:collapse`, or `opacity:0`
///   5. Class attribute containing any entry from [`HIDING_CLASSES`]
///
/// Does **not** walk the parent chain — use [`node_is_hidden_in`] for that.
/// Does **not** read the stylesheet — only inline style and the class list.
///
/// Returns `false` for non-Element nodes (Document, Text, Comment, etc.).
pub fn node_is_hidden(node: &Node) -> bool {
    let NodeData::Element { attrs, .. } = &node.data else {
        return false;
    };

    // [hidden] or [inert] attribute present
    if attrs.iter().any(|a| {
        let name = a.name.local.as_ref();
        name == "hidden" || name == "inert"
    }) {
        return true;
    }

    // [aria-hidden="true"]
    if let Some(v) = attr_value(attrs, "aria-hidden") {
        if v.eq_ignore_ascii_case("true") {
            return true;
        }
    }

    // role="presentation" | role="none"
    if let Some(v) = attr_value(attrs, "role") {
        if v == "presentation" || v == "none" {
            return true;
        }
    }

    // Inline style check
    if let Some(style) = attr_value(attrs, "style") {
        if style_hides(style) {
            return true;
        }
    }

    // Class list check
    if let Some(class) = attr_value(attrs, "class") {
        if class
            .split_whitespace()
            .any(|c| HIDING_CLASSES.contains(&c))
        {
            return true;
        }
    }

    false
}

/// Test whether a node is hidden by author intent, including any ancestor.
///
/// Walks up the parent chain from `node_id` and returns `true` if any
/// ancestor (or the node itself) is hidden per [`node_is_hidden`].
/// Returns `false` if the node does not exist.
pub fn node_is_hidden_in(dom: &DomTree, node_id: NodeId) -> bool {
    let mut current = Some(node_id);
    while let Some(id) = current {
        if let Some(node) = dom.get_node(id) {
            if node_is_hidden(&node) {
                return true;
            }
            current = node.parent;
        } else {
            return false;
        }
    }
    false
}

// ============================================================================
// AX tree builder
// ============================================================================

/// Build the full accessibility tree for a DOM document.
///
/// Returns a flat list of [`AXNode`] entries, one per DOM node that has a
/// non-empty role. Order matches the document order (depth-first).
///
/// The output is deterministic: the same DOM always produces the same list.
pub fn ax_tree(dom: &DomTree) -> Vec<AXNode> {
    let mut nodes: Vec<AXNode> = Vec::new();
    let mut id_counter: u32 = 0;
    // Map DOM NodeId → AX id for parent resolution
    let mut dom_to_ax: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();

    let document = dom.document();
    let all_dom_ids: Vec<NodeId> = std::iter::once(document)
        .chain(dom.descendants(document).into_iter())
        .collect();

    // First pass: assign AX ids to eligible nodes (non-empty role).
    let eligible: Vec<NodeId> = all_dom_ids
        .into_iter()
        .filter(|dom_id| {
            dom.get_node(*dom_id)
                .map(|n| !map_role(&n.data).is_empty())
                .unwrap_or(false)
        })
        .collect();

    for &dom_id in &eligible {
        id_counter += 1;
        dom_to_ax.insert(dom_id.raw(), id_counter);
    }

    // Second pass: build AXNode for eligible nodes.
    for &dom_id in &eligible {
        let Some(node) = dom.get_node(dom_id) else { continue };
        let ax_id = dom_to_ax[&dom_id.raw()];
        let role = map_role(&node.data);
        let name = compute_name(dom, &node);
        let hidden = node_is_hidden(&node);
        let tag = if let NodeData::Element { name, .. } = &node.data {
            Some(name.local.as_ref())
        } else {
            None
        };
        let is_block = tag.map(is_block_tag).unwrap_or(false);

        // Resolve parent_id: walk up DOM ancestors until we find one in the AX tree.
        let parent_id = {
            let mut current = dom_id;
            let mut result = None;
            loop {
                if let Some(next_parent) = dom.with_node(current, |n| n.parent).flatten() {
                    if let Some(&ax_pid) = dom_to_ax.get(&next_parent.raw()) {
                        result = Some(ax_pid);
                        break;
                    }
                    current = next_parent;
                } else {
                    break;
                }
            }
            result
        };

        nodes.push(AXNode {
            id: ax_id,
            parent_id,
            role,
            name,
            hidden,
            is_block,
            dom_id,
        });
    }

    nodes
}

// ============================================================================
// Visible text extraction
// ============================================================================

/// Roles whose rendered text should be excluded from visible-text output.
///
/// These are form controls, decorative elements, images, and structural
/// separators that never form part of the page's visible content.
const SKIPPABLE_ROLES: &[&str] = &[
    "button",
    "image",
    "separator",
    "presentation",
    "none",
    "checkbox",
    "radio",
    "spinbutton",
    "slider",
    "combobox",
    "listbox",
    "searchbox",
    "textbox",
    "Iframe",
];

/// Extract the visible text content from a DOM subtree.
///
/// The walker:
/// 1. Skips hidden subtrees (current node or any ancestor is hidden).
/// 2. Skips nodes with skippable roles (buttons, images, form controls, etc.).
/// 3. Inserts blank-line separators (`\n\n`) between block-level elements.
/// 4. Inserts inline line breaks (`\n`) at `<br>` elements.
/// 5. Preserves whitespace inside `<pre>` blocks.
/// 6. Collapses inline whitespace runs to a single space outside `<pre>`.
/// 7. Trims leading/trailing whitespace from the final result.
///
/// The resulting string is a best-effort approximation of what sighted users
/// perceive as the visible text content of the subtree, without requiring a
/// layout engine.
///
/// # Example
///
/// ```
/// use obscura_dom::{parse_html, ax, NodeData};
///
/// let dom = parse_html("<p>Hello <strong>world</strong></p>");
/// let root = dom.find_body_or_root();
/// let text = ax::visible_text(&dom, root);
/// assert_eq!(text, "Hello world");
/// ```
pub fn visible_text(dom: &DomTree, root: NodeId) -> String {
    let mut out = String::with_capacity(256);
    walk_visible(dom, root, false, false, &mut out);
    let trimmed = out.trim().to_string();
    trimmed
}

/// Recursive visibility-aware text walker.
fn walk_visible(
    dom: &DomTree,
    nid: NodeId,
    hidden: bool,
    in_pre: bool,
    out: &mut String,
) {
    let Some(node) = dom.get_node(nid) else { return };

    // Accumulate hidden state down the tree.
    let hidden = hidden || node_is_hidden(&node);

    if hidden {
        return;
    }

    match &node.data {
        NodeData::Text { contents } => {
            if in_pre {
                out.push_str(contents);
            } else {
                out.push_str(&collapse_whitespace(contents));
            }
        }
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();

            // Skip nodes with skippable roles.
            let role = map_role(&node.data);
            if SKIPPABLE_ROLES.contains(&role) {
                return;
            }

            // <br> produces a single line break.
            if tag == "br" {
                if !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                return;
            }

            // Block elements: add blank-line separator before content.
            if is_block_tag(tag) && !out.is_empty() && !out.ends_with('\n') {
                out.push_str("\n\n");
            }

            // Recurse into children.
            let child_pre = in_pre || tag == "pre";
            for child in dom.children(nid) {
                walk_visible(dom, child, hidden, child_pre, out);
            }

            // Close certain blocks with a blank-line separator.
            if matches!(tag, "p" | "div" | "pre" | "li" | "blockquote" | "tr") {
                if !out.is_empty() && !out.ends_with("\n\n") {
                    out.push_str("\n\n");
                }
            }
        }
        _ => {} // Document, Doctype, Comment, PI — no visible text
    }
}

// ============================================================================
// Internal helpers
// ============================================================================

/// Retrieve an attribute value from the attribute list.
fn attr_value<'a>(attrs: &'a [Attribute], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|a| a.name.local.as_ref() == name)
        .map(|a| a.value.as_str())
}

/// Test whether an HTML tag name is a block-level element.
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "pre"
            | "blockquote"
            | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "tr" | "hr"
            | "ul" | "ol"
            | "table"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "main"
            | "aside"
            | "nav"
            | "li"
    )
}

/// Parse an inline `style` attribute and return `true` if it contains a
/// hiding declaration.
fn style_hides(style: &str) -> bool {
    for decl in style.split(';') {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        let Some((prop_raw, val_raw)) = decl.split_once(':') else {
            continue;
        };
        let prop = prop_raw.trim().to_ascii_lowercase();
        // Strip !important and trim.
        let val = val_raw
            .trim()
            .trim_end_matches("!important")
            .trim()
            .to_ascii_lowercase();
        match prop.as_str() {
            "display" if val == "none" => return true,
            "visibility" if val == "hidden" || val == "collapse" => return true,
            "opacity" => {
                if val == "0" {
                    return true;
                }
                if let Ok(n) = val.parse::<f32>() {
                    if n == 0.0 {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Collapse runs of whitespace to a single space.
///
/// Leading/trailing whitespace is preserved for later trimming. Newlines,
/// tabs, and multiple spaces are all replaced with a single space.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use html5ever::{ns, LocalName, Namespace, QualName};
    use crate::tree::{Attribute, DomTree, Node, NodeData, NodeId};

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn text_node(contents: &str) -> NodeData {
        NodeData::Text {
            contents: contents.to_string(),
        }
    }

    fn elem_node(tag: &str, attrs: Vec<(&str, &str)>) -> NodeData {
        let attrs: Vec<Attribute> = attrs
            .into_iter()
            .map(|(k, v)| Attribute {
                name: QualName::new(None, Namespace::default(), LocalName::from(k)),
                value: v.to_string(),
            })
            .collect();
        NodeData::Element {
            name: QualName::new(None, ns!(html), LocalName::from(tag)),
            attrs,
            template_contents: None,
            mathml_annotation_xml_integration_point: false,
        }
    }

    fn mk_tree(body_html: &str) -> DomTree {
        let html = format!("<html><head></head><body>{}</body></html>", body_html);
        crate::tree_sink::parse_html(&html)
    }

    // Returns the body <body> node from a parsed tree.
    fn body_id(dom: &DomTree) -> NodeId {
        dom.find_body_or_root()
    }

    // ------------------------------------------------------------------
    // map_role tests
    // ------------------------------------------------------------------

    #[test]
    fn map_role_document_is_root_web_area() {
        assert_eq!(map_role(&NodeData::Document), "RootWebArea");
    }

    #[test]
    fn map_role_text_is_static_text() {
        assert_eq!(map_role(&text_node("hello")), "StaticText");
    }

    #[test]
    fn map_role_button() {
        let data = elem_node("button", vec![]);
        assert_eq!(map_role(&data), "button");
    }

    #[test]
    fn map_role_explicit_aria_role_wins() {
        let data = elem_node("div", vec![("role", "button")]);
        assert_eq!(map_role(&data), "button");
    }

    #[test]
    fn map_role_anchor_with_href_is_link() {
        let data = elem_node("a", vec![("href", "https://example.com")]);
        assert_eq!(map_role(&data), "link");
    }

    #[test]
    fn map_role_anchor_without_href_is_generic() {
        let data = elem_node("a", vec![]);
        assert_eq!(map_role(&data), "generic");
    }

    #[test]
    fn map_role_heading() {
        let data = elem_node("h1", vec![]);
        assert_eq!(map_role(&data), "heading");
    }

    #[test]
    fn map_role_image() {
        let data = elem_node("img", vec![("alt", "photo")]);
        assert_eq!(map_role(&data), "image");
    }

    #[test]
    fn map_role_unknown_tag_is_generic() {
        let data = elem_node("blink", vec![]);
        assert_eq!(map_role(&data), "generic");
    }

    #[test]
    fn map_role_comment_is_empty() {
        assert_eq!(map_role(&NodeData::Comment { contents: String::new() }), "");
    }

    #[test]
    fn map_role_doctype_is_empty() {
        assert_eq!(
            map_role(&NodeData::Doctype {
                name: String::new(),
                public_id: String::new(),
                system_id: String::new()
            }),
            ""
        );
    }

    // ------------------------------------------------------------------
    // node_is_hidden tests
    // ------------------------------------------------------------------

    fn hidden_node(tag: &str, attrs: Vec<(&str, &str)>) -> Node {
        Node {
            id: NodeId::new(1),
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            data: elem_node(tag, attrs),
        }
    }

    fn text_node_hidden_check() -> Node {
        Node {
            id: NodeId::new(1),
            parent: None,
            first_child: None,
            last_child: None,
            prev_sibling: None,
            next_sibling: None,
            data: NodeData::Text {
                contents: "hello".to_string(),
            },
        }
    }

    #[test]
    fn hidden_detects_hidden_attr() {
        assert!(node_is_hidden(&hidden_node("div", vec![("hidden", "")])));
    }

    #[test]
    fn hidden_detects_inert_attr() {
        assert!(node_is_hidden(&hidden_node("div", vec![("inert", "")])));
    }

    #[test]
    fn hidden_detects_aria_hidden_true() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("aria-hidden", "true")]
        )));
    }

    #[test]
    fn hidden_aria_hidden_false_not_hidden() {
        assert!(!node_is_hidden(&hidden_node(
            "div",
            vec![("aria-hidden", "false")]
        )));
    }

    #[test]
    fn hidden_detects_role_presentation() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("role", "presentation")]
        )));
    }

    #[test]
    fn hidden_detects_role_none() {
        assert!(node_is_hidden(&hidden_node("div", vec![("role", "none")])));
    }

    #[test]
    fn hidden_role_button_not_hidden() {
        assert!(!node_is_hidden(&hidden_node(
            "div",
            vec![("role", "button")]
        )));
    }

    #[test]
    fn hidden_inline_display_none() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("style", "display: none")]
        )));
    }

    #[test]
    fn hidden_inline_display_none_no_space() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("style", "display:none")]
        )));
    }

    #[test]
    fn hidden_inline_display_block_not_hidden() {
        assert!(!node_is_hidden(&hidden_node(
            "div",
            vec![("style", "display: block")]
        )));
    }

    #[test]
    fn hidden_inline_visibility_hidden() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("style", "visibility: hidden")]
        )));
    }

    #[test]
    fn hidden_inline_visibility_visible_not_hidden() {
        assert!(!node_is_hidden(&hidden_node(
            "div",
            vec![("style", "visibility: visible")]
        )));
    }

    #[test]
    fn hidden_inline_opacity_zero() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("style", "opacity: 0")]
        )));
    }

    #[test]
    fn hidden_inline_opacity_partial_not_hidden() {
        assert!(!node_is_hidden(&hidden_node(
            "div",
            vec![("style", "opacity: 0.5")]
        )));
    }

    #[test]
    fn hidden_multiple_declarations_one_hides() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("style", "color: red; display: none; font-size: 12px")]
        )));
    }

    #[test]
    fn hidden_class_hidden() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("class", "hidden")]
        )));
    }

    #[test]
    fn hidden_class_hidden_with_others() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("class", "hidden my-component")]
        )));
    }

    #[test]
    fn hidden_class_not_hidden() {
        assert!(!node_is_hidden(&hidden_node(
            "div",
            vec![("class", "my-component")]
        )));
    }

    #[test]
    fn hidden_class_sr_only() {
        assert!(node_is_hidden(&hidden_node(
            "div",
            vec![("class", "sr-only")]
        )));
    }

    #[test]
    fn hidden_text_node_not_hidden() {
        assert!(!node_is_hidden(&text_node_hidden_check()));
    }

    // ------------------------------------------------------------------
    // compute_name tests
    // ------------------------------------------------------------------

    #[test]
    fn compute_name_aria_label_wins() {
        let dom = DomTree::new();
        let node = hidden_node("div", vec![("aria-label", "Hello")]);
        assert_eq!(compute_name(&dom, &node), Some("Hello".to_string()));
    }

    #[test]
    fn compute_name_text_node_is_text() {
        let dom = DomTree::new();
        let doc = dom.document();
        let txt_id = dom.new_node(text_node("Hello world"));
        dom.append_child(doc, txt_id);
        let node = dom.get_node(txt_id).unwrap();
        assert_eq!(compute_name(&dom, &node), Some("Hello world".to_string()));
    }

    #[test]
    fn compute_name_empty_text_gives_none() {
        let dom = DomTree::new();
        let doc = dom.document();
        let txt_id = dom.new_node(text_node("   "));
        dom.append_child(doc, txt_id);
        let node = dom.get_node(txt_id).unwrap();
        assert_eq!(compute_name(&dom, &node), None);
    }

    // ------------------------------------------------------------------
    // visible_text tests
    // ------------------------------------------------------------------

    #[test]
    fn visible_text_simple_paragraph() {
        let dom = mk_tree("<p>Hello world</p>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "Hello world");
    }

    #[test]
    fn visible_text_nested_text() {
        let dom = mk_tree("<div><p>Hello <strong>world</strong></p></div>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "Hello world");
    }

    #[test]
    fn visible_text_paragraphs_separated() {
        let dom = mk_tree("<p>First</p><p>Second</p>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "First\n\nSecond");
    }

    #[test]
    fn visible_text_line_break() {
        let dom = mk_tree("<p>Line1<br>Line2</p>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "Line1\nLine2");
    }

    #[test]
    fn visible_text_preserves_pre_whitespace() {
        let dom = mk_tree("<pre>  code\n    block</pre>");
        let root = body_id(&dom);
        // html5ever may normalize leading/trailing whitespace in text nodes
        // even inside <pre>. Verify that internal whitespace (newline + indent)
        // is preserved.
        let text = visible_text(&dom, root);
        assert!(
            text.contains("code") && text.contains("block"),
            "pre text should contain both words, got: {:?}",
            text,
        );
        assert!(
            text.contains('\n'),
            "pre text should preserve newlines, got: {:?}",
            text,
        );
    }

    #[test]
    fn visible_text_hidden_element() {
        let dom = mk_tree("<div><span hidden>secret</span>visible</div>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "visible");
    }

    #[test]
    fn visible_text_hidden_ancestor() {
        let dom = mk_tree("<div hidden><span>secret</span></div>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "");
    }

    #[test]
    fn visible_text_skips_button_role() {
        let dom = mk_tree("<div>text <button>Click</button> more</div>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "text  more");
    }

    #[test]
    fn visible_text_skips_image_role() {
        let dom = mk_tree("<p>see <img alt='photo'> picture</p>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "see  picture");
    }

    #[test]
    fn visible_text_skips_role_presentation() {
        let dom = mk_tree("<div role='presentation'>x</div><p>actual</p>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "actual");
    }

    #[test]
    fn visible_text_list_items() {
        let dom = mk_tree("<ul><li>Alpha</li><li>Beta</li></ul>");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "Alpha\n\nBeta");
    }

    #[test]
    fn visible_text_inline_display_none_skips_subtree() {
        let dom = mk_tree(
            "<p>before<span style='display:none'>hidden</span>after</p>",
        );
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "beforeafter");
    }

    #[test]
    fn visible_text_empty_document() {
        let dom = mk_tree("");
        let root = body_id(&dom);
        assert_eq!(visible_text(&dom, root), "");
    }

    // ------------------------------------------------------------------
    // ax_tree tests
    // ------------------------------------------------------------------

    #[test]
    fn ax_tree_empty_document_has_root_web_area() {
        let dom = mk_tree("");
        let tree = ax_tree(&dom);
        assert!(!tree.is_empty());
        assert_eq!(tree[0].role, "RootWebArea");
    }

    #[test]
    fn ax_tree_text_node_is_static_text() {
        let dom = mk_tree("<p>hi</p>");
        let tree = ax_tree(&dom);
        let static_texts: Vec<&AXNode> = tree.iter().filter(|n| n.role == "StaticText").collect();
        assert_eq!(static_texts.len(), 1);
        assert_eq!(static_texts[0].name.as_deref(), Some("hi"));
    }

    #[test]
    fn ax_tree_skips_comment_nodes() {
        let dom = mk_tree("<!-- comment --><p>text</p>");
        let tree = ax_tree(&dom);
        // Should have RootWebArea, generic (html), generic (head), generic (body),
        // generic (p), StaticText
        let comments: Vec<&AXNode> = tree.iter().filter(|n| n.name.as_deref() == Some("comment")).collect();
        let static_texts: Vec<&AXNode> = tree.iter().filter(|n| n.role == "StaticText").collect();
        assert!(comments.is_empty());
        assert_eq!(static_texts.len(), 1);
    }

    #[test]
    fn ax_tree_hidden_flag() {
        let dom = mk_tree("<div hidden>secret</div>");
        let tree = ax_tree(&dom);
        let hidden_nodes: Vec<&AXNode> = tree.iter().filter(|n| n.hidden).collect();
        assert!(!hidden_nodes.is_empty(), "expected at least one hidden node");
        // The hidden div is the <div hidden> element — find the generic node
        // whose child is the StaticText "secret".
        let hiding_generics: Vec<&AXNode> = tree.iter().filter(|n| n.role == "generic" && n.hidden).collect();
        assert!(!hiding_generics.is_empty(), "expected a hidden generic node");
    }

    #[test]
    fn ax_tree_parent_id_resolved() {
        let dom = mk_tree("<p>hello</p>");
        let tree = ax_tree(&dom);
        // body is generic -> its children are p (generic) and maybe others
        let p_node = tree.iter().find(|n| n.dom_id != NodeId::new(0) && n.role == "generic").unwrap();
        // p_node should have a parent_id (the body/html parent)
        assert!(p_node.parent_id.is_some(), "p node should have a parent_id");
    }

    // ------------------------------------------------------------------
    // style_hides tests
    // ------------------------------------------------------------------

    #[test]
    fn style_hides_display_none() {
        assert!(style_hides("display:none"));
    }

    #[test]
    fn style_hides_display_none_with_space() {
        assert!(style_hides("display: none"));
    }

    #[test]
    fn style_hides_display_block_not_hidden() {
        assert!(!style_hides("display: block"));
    }

    #[test]
    fn style_hides_visibility_hidden() {
        assert!(style_hides("visibility:hidden"));
    }

    #[test]
    fn style_hides_visibility_collapse() {
        assert!(style_hides("visibility:collapse"));
    }

    #[test]
    fn style_hides_visibility_visible_not_hidden() {
        assert!(!style_hides("visibility: visible"));
    }

    #[test]
    fn style_hides_opacity_zero() {
        assert!(style_hides("opacity:0"));
    }

    #[test]
    fn style_hides_opacity_fraction_zero() {
        assert!(style_hides("opacity: 0.0"));
    }

    #[test]
    fn style_hides_opacity_half_not_hidden() {
        assert!(!style_hides("opacity: 0.5"));
    }

    #[test]
    fn style_hides_important_flag() {
        assert!(style_hides("display: none !important"));
    }

    #[test]
    fn style_hides_mixed_declarations() {
        assert!(style_hides("color: red; display: none; margin: 0"));
    }

    #[test]
    fn style_hides_empty_string_not_hidden() {
        assert!(!style_hides(""));
    }

    #[test]
    fn style_hides_malformed_no_colon() {
        assert!(!style_hides("justsomegarbage"));
    }
}
