/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Creates flows and fragments from a DOM tree via a bottom-up, incremental traversal of the DOM.
//!
//! Each step of the traversal considers the node and existing flow, if there is one. If a node is
//! not dirty and an existing flow exists, then the traversal reuses that flow. Otherwise, it
//! proceeds to construct either a flow or a `ConstructionItem`. A construction item is a piece of
//! intermediate data that goes with a DOM node and hasn't found its "home" yet-maybe it's a box,
//! maybe it's an absolute or fixed position thing that hasn't found its containing block yet.
//! Construction items bubble up the tree from children to parents until they find their homes.

use std::collections::LinkedList;
use std::marker::PhantomData;
use std::mem;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use html5ever::{local_name, namespace_url, ns};
use log::debug;
use script_layout_interface::wrapper_traits::{
    PseudoElementType, ThreadSafeLayoutElement, ThreadSafeLayoutNode,
};
use script_layout_interface::{LayoutElementType, LayoutNodeType};
use servo_url::ServoUrl;
use style::LocalName;
use style::computed_values::caption_side::T as CaptionSide;
use style::computed_values::display::T as Display;
use style::computed_values::empty_cells::T as EmptyCells;
use style::computed_values::float::T as Float;
use style::computed_values::list_style_position::T as ListStylePosition;
use style::computed_values::position::T as Position;
use style::context::SharedStyleContext;
use style::dom::TElement;
use style::logical_geometry::Direction;
use style::properties::ComputedValues;
use style::selector_parser::{PseudoElement, RestyleDamage};
use style::servo::restyle_damage::ServoRestyleDamage;
use style::values::computed::Image;
use style::values::generics::counters::ContentItem;

use crate::block::BlockFlow;
use crate::context::LayoutContext;
use crate::data::{InnerLayoutData, LayoutDataFlags};
use crate::display_list::items::OpaqueNode;
use crate::flex::FlexFlow;
use crate::floats::FloatKind;
use crate::flow::{
    AbsoluteDescendants, Flow, FlowClass, FlowFlags, GetBaseFlow, ImmutableFlowUtils,
    MutableFlowUtils, MutableOwnedFlowUtils,
};
use crate::flow_ref::FlowRef;
use crate::fragment::{
    CanvasFragmentInfo, Fragment, FragmentFlags, GeneratedContentInfo, IframeFragmentInfo,
    ImageFragmentInfo, InlineAbsoluteFragmentInfo, InlineAbsoluteHypotheticalFragmentInfo,
    InlineBlockFragmentInfo, MediaFragmentInfo, SpecificFragmentInfo, SvgFragmentInfo,
    TableColumnFragmentInfo, UnscannedTextFragmentInfo, WhitespaceStrippingResult,
};
use crate::inline::{InlineFlow, InlineFragmentNodeFlags, InlineFragmentNodeInfo};
use crate::linked_list::prepend_from;
use crate::list_item::{ListItemFlow, ListStyleTypeContent};
use crate::multicol::{MulticolColumnFlow, MulticolFlow};
use crate::table::TableFlow;
use crate::table_caption::TableCaptionFlow;
use crate::table_cell::TableCellFlow;
use crate::table_colgroup::TableColGroupFlow;
use crate::table_row::TableRowFlow;
use crate::table_rowgroup::TableRowGroupFlow;
use crate::table_wrapper::TableWrapperFlow;
use crate::text::TextRunScanner;
use crate::traversal::PostorderNodeMutTraversal;
use crate::wrapper::{TextContent, ThreadSafeLayoutNodeHelpers};
use crate::{ServoArc, parallel};

/// The results of flow construction for a DOM node.
#[derive(Clone, Default)]
pub enum ConstructionResult {
    /// This node contributes nothing at all (`display: none`). Alternately, this is what newly
    /// created nodes have their `ConstructionResult` set to.
    #[default]
    None,

    /// This node contributed a flow at the proper position in the tree.
    /// Nothing more needs to be done for this node. It has bubbled up fixed
    /// and absolute descendant flows that have a containing block above it.
    Flow(FlowRef, AbsoluteDescendants),

    /// This node contributed some object or objects that will be needed to construct a proper flow
    /// later up the tree, but these objects have not yet found their home.
    ConstructionItem(ConstructionItem),
}

impl ConstructionResult {
    pub fn get(&mut self) -> ConstructionResult {
        // FIXME(pcwalton): Stop doing this with inline fragments. Cloning fragments is very
        // inefficient!
        (*self).clone()
    }
}

/// Represents the output of flow construction for a DOM node that has not yet resulted in a
/// complete flow. Construction items bubble up the tree until they find a `Flow` to be attached
/// to.
#[derive(Clone)]
pub enum ConstructionItem {
    /// Inline fragments and associated {ib} splits that have not yet found flows.
    InlineFragments(InlineFragmentsConstructionResult),
    /// Potentially ignorable whitespace.
    ///
    /// FIXME(emilio): How could whitespace have any PseudoElementType other
    /// than Normal?
    Whitespace(
        OpaqueNode,
        PseudoElementType,
        ServoArc<ComputedValues>,
        RestyleDamage,
    ),
    /// TableColumn Fragment
    TableColumnFragment(Fragment),
}

/// Represents inline fragments and {ib} splits that are bubbling up from an inline.
#[derive(Clone)]
pub struct InlineFragmentsConstructionResult {
    /// Any {ib} splits that we're bubbling up.
    pub splits: LinkedList<InlineBlockSplit>,

    /// Any fragments that succeed the {ib} splits.
    pub fragments: IntermediateInlineFragments,
}

/// Represents an {ib} split that has not yet found the containing block that it belongs to. This
/// is somewhat tricky. An example may be helpful. For this DOM fragment:
///
/// ```html
///     <span>
///     A
///     <div>B</div>
///     C
///     </span>
/// ```
///
/// The resulting `ConstructionItem` for the outer `span` will be:
///
/// ```rust,ignore
///     ConstructionItem::InlineFragments(
///         InlineFragmentsConstructionResult {
///             splits: linked_list![
///                 InlineBlockSplit {
///                     predecessors: IntermediateInlineFragments {
///                         fragments: linked_list![A],
///                         absolute_descendents: AbsoluteDescendents {
///                             descendant_links: vec![]
///                         },
///                     },
///                     flow: B,
///                 }
///             ],
///             fragments: linked_list![C],
///         },
///     )
/// ```
#[derive(Clone)]
pub struct InlineBlockSplit {
    /// The inline fragments that precede the flow.
    pub predecessors: IntermediateInlineFragments,

    /// The flow that caused this {ib} split.
    pub flow: FlowRef,
}

impl InlineBlockSplit {
    /// Flushes the given accumulator to the new split and makes a new accumulator to hold any
    /// subsequent fragments.
    fn new<'dom, ConcreteThreadSafeLayoutNode>(
        fragment_accumulator: &mut InlineFragmentsAccumulator,
        node: &ConcreteThreadSafeLayoutNode,
        style_context: &SharedStyleContext,
        flow: FlowRef,
    ) -> InlineBlockSplit
    where
        ConcreteThreadSafeLayoutNode: ThreadSafeLayoutNode<'dom>,
    {
        fragment_accumulator.enclosing_node.as_mut().expect(
            "enclosing_node is None; Are {ib} splits being generated outside of an inline node?"
        ).flags.remove(InlineFragmentNodeFlags::LAST_FRAGMENT_OF_ELEMENT);

        let split = InlineBlockSplit {
            predecessors: mem::replace(
                fragment_accumulator,
                InlineFragmentsAccumulator::from_inline_node(node, style_context),
            )
            .get_intermediate_inline_fragments::<ConcreteThreadSafeLayoutNode>(style_context),
            flow,
        };

        fragment_accumulator
            .enclosing_node
            .as_mut()
            .unwrap()
            .flags
            .remove(InlineFragmentNodeFlags::FIRST_FRAGMENT_OF_ELEMENT);

        split
    }
}

/// Holds inline fragments and absolute descendants.
#[derive(Clone)]
pub struct IntermediateInlineFragments {
    /// The list of fragments.
    pub fragments: LinkedList<Fragment>,

    /// The list of absolute descendants of those inline fragments.
    pub absolute_descendants: AbsoluteDescendants,
}

impl IntermediateInlineFragments {
    fn new() -> IntermediateInlineFragments {
        IntermediateInlineFragments {
            fragments: LinkedList::new(),
            absolute_descendants: AbsoluteDescendants::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.fragments.is_empty() && self.absolute_descendants.is_empty()
    }

    fn push_all(&mut self, mut other: IntermediateInlineFragments) {
        self.fragments.append(&mut other.fragments);
        self.absolute_descendants
            .push_descendants(other.absolute_descendants);
    }
}

/// Holds inline fragments that we're gathering for children of an inline node.
struct InlineFragmentsAccumulator {
    /// The list of fragments.
    fragments: IntermediateInlineFragments,

    /// Information about the inline box directly enclosing the fragments being gathered, if any.
    ///
    /// `inline::InlineFragmentNodeInfo` also stores flags indicating whether a fragment is the
    /// first and/or last of the corresponding inline box. This `InlineFragmentsAccumulator` may
    /// represent only one side of an {ib} split, so we store these flags as if it represented only
    /// one fragment. `to_intermediate_inline_fragments` later splits this hypothetical fragment
    /// into pieces, leaving the `FIRST_FRAGMENT_OF_ELEMENT` and `LAST_FRAGMENT_OF_ELEMENT` flags,
    /// if present, on the first and last fragments of the output.
    enclosing_node: Option<InlineFragmentNodeInfo>,

    /// Restyle damage to use for fragments created in this node.
    restyle_damage: RestyleDamage,

    /// Bidi control characters to insert before and after these fragments.
    bidi_control_chars: Option<(&'static str, &'static str)>,
}

impl InlineFragmentsAccumulator {
    fn new() -> InlineFragmentsAccumulator {
        InlineFragmentsAccumulator {
            fragments: IntermediateInlineFragments::new(),
            enclosing_node: None,
            bidi_control_chars: None,
            restyle_damage: RestyleDamage::empty(),
        }
    }

    fn from_inline_node<'dom>(
        node: &impl ThreadSafeLayoutNode<'dom>,
        style_context: &SharedStyleContext,
    ) -> InlineFragmentsAccumulator {
        InlineFragmentsAccumulator {
            fragments: IntermediateInlineFragments::new(),
            enclosing_node: Some(InlineFragmentNodeInfo {
                address: node.opaque(),
                pseudo: node.get_pseudo_element_type(),
                style: node.style(style_context),
                selected_style: node.selected_style(),
                flags: InlineFragmentNodeFlags::FIRST_FRAGMENT_OF_ELEMENT |
                    InlineFragmentNodeFlags::LAST_FRAGMENT_OF_ELEMENT,
            }),
            bidi_control_chars: None,
            restyle_damage: node.restyle_damage(),
        }
    }

    fn push(&mut self, fragment: Fragment) {
        self.fragments.fragments.push_back(fragment)
    }

    fn push_all(&mut self, mut fragments: IntermediateInlineFragments) {
        self.fragments.fragments.append(&mut fragments.fragments);
        self.fragments
            .absolute_descendants
            .push_descendants(fragments.absolute_descendants);
    }

    fn get_intermediate_inline_fragments<'dom, N>(
        self,
        context: &SharedStyleContext,
    ) -> IntermediateInlineFragments
    where
        N: ThreadSafeLayoutNode<'dom>,
    {
        let InlineFragmentsAccumulator {
            mut fragments,
            enclosing_node,
            bidi_control_chars,
            restyle_damage,
        } = self;
        if let Some(mut enclosing_node) = enclosing_node {
            let fragment_count = fragments.fragments.len();
            for (index, fragment) in fragments.fragments.iter_mut().enumerate() {
                let mut enclosing_node = enclosing_node.clone();
                if index != 0 {
                    enclosing_node
                        .flags
                        .remove(InlineFragmentNodeFlags::FIRST_FRAGMENT_OF_ELEMENT)
                }
                if index != fragment_count - 1 {
                    enclosing_node
                        .flags
                        .remove(InlineFragmentNodeFlags::LAST_FRAGMENT_OF_ELEMENT)
                }
                fragment.add_inline_context_style(enclosing_node);
            }

            // Control characters are later discarded in transform_text, so they don't affect the
            // is_first/is_last styles above.
            enclosing_node.flags.remove(
                InlineFragmentNodeFlags::FIRST_FRAGMENT_OF_ELEMENT |
                    InlineFragmentNodeFlags::LAST_FRAGMENT_OF_ELEMENT,
            );

            if let Some((start, end)) = bidi_control_chars {
                fragments
                    .fragments
                    .push_front(control_chars_to_fragment::<N::ConcreteElement>(
                        &enclosing_node,
                        context,
                        start,
                        restyle_damage,
                    ));
                fragments
                    .fragments
                    .push_back(control_chars_to_fragment::<N::ConcreteElement>(
                        &enclosing_node,
                        context,
                        end,
                        restyle_damage,
                    ));
            }
        }
        fragments
    }
}

/// An object that knows how to create flows.
pub struct FlowConstructor<'a, N> {
    /// The layout context.
    pub layout_context: &'a LayoutContext<'a>,
    /// Satisfy the compiler about the unused parameters, which we use to improve the ergonomics of
    /// the ensuing impl {} by removing the need to parameterize all the methods individually.
    phantom2: PhantomData<N>,
}

impl<'a, 'dom, ConcreteThreadSafeLayoutNode> FlowConstructor<'a, ConcreteThreadSafeLayoutNode>
where
    ConcreteThreadSafeLayoutNode: ThreadSafeLayoutNode<'dom>,
{
    /// Creates a new flow constructor.
    pub fn new(layout_context: &'a LayoutContext<'a>) -> Self {
        FlowConstructor {
            layout_context,
            phantom2: PhantomData,
        }
    }

    #[inline]
    fn style_context(&self) -> &SharedStyleContext {
        self.layout_context.shared_context()
    }

    #[inline]
    fn set_flow_construction_result(
        &self,
        node: &ConcreteThreadSafeLayoutNode,
        result: ConstructionResult,
    ) {
        node.set_flow_construction_result(result);
    }

    /// Builds the fragment for the given block or subclass thereof.
    fn build_fragment_for_block(&self, node: &ConcreteThreadSafeLayoutNode) -> Fragment {
        let specific_fragment_info = match node.type_id() {
            Some(LayoutNodeType::Element(LayoutElementType::HTMLIFrameElement)) => {
                SpecificFragmentInfo::Iframe(IframeFragmentInfo::new(node))
            },
            Some(LayoutNodeType::Element(LayoutElementType::HTMLImageElement)) => {
                let image_info = Box::new(ImageFragmentInfo::new(
                    node.image_url(),
                    node.image_density(),
                    node,
                    self.layout_context,
                ));
                SpecificFragmentInfo::Image(image_info)
            },
            Some(LayoutNodeType::Element(LayoutElementType::HTMLMediaElement)) => {
                let data = node.media_data().unwrap();
                SpecificFragmentInfo::Media(Box::new(MediaFragmentInfo::new(data)))
            },
            Some(LayoutNodeType::Element(LayoutElementType::HTMLObjectElement)) => {
                let elem = node.as_element().unwrap();
                let type_and_data = (
                    elem.get_attr(&ns!(), &local_name!("type")),
                    elem.get_attr(&ns!(), &local_name!("data")),
                );
                let object_data = match type_and_data {
                    (None, Some(uri)) if is_image_data(uri) => ServoUrl::parse(uri).ok(),
                    _ => None,
                };
                let image_info = Box::new(ImageFragmentInfo::new(
                    object_data,
                    None,
                    node,
                    self.layout_context,
                ));
                SpecificFragmentInfo::Image(image_info)
            },
            Some(LayoutNodeType::Element(LayoutElementType::HTMLTableElement)) => {
                SpecificFragmentInfo::TableWrapper
            },
            Some(LayoutNodeType::Element(LayoutElementType::HTMLTableColElement)) => {
                SpecificFragmentInfo::TableColumn(TableColumnFragmentInfo::new(node))
            },
            Some(LayoutNodeType::Element(LayoutElementType::HTMLTableCellElement)) => {
                SpecificFragmentInfo::TableCell
            },
            Some(LayoutNodeType::Element(LayoutElementType::HTMLTableRowElement)) |
            Some(LayoutNodeType::Element(LayoutElementType::HTMLTableSectionElement)) => {
                SpecificFragmentInfo::TableRow
            },
            Some(LayoutNodeType::Element(LayoutElementType::HTMLCanvasElement)) => {
                let data = node.canvas_data().unwrap();
                SpecificFragmentInfo::Canvas(Box::new(CanvasFragmentInfo::new(data)))
            },
            Some(LayoutNodeType::Element(LayoutElementType::SVGSVGElement)) => {
                let data = node.svg_data().unwrap();
                SpecificFragmentInfo::Svg(Box::new(SvgFragmentInfo::new(data)))
            },
            _ => {
                // This includes pseudo-elements.
                SpecificFragmentInfo::Generic
            },
        };

        Fragment::new(node, specific_fragment_info, self.layout_context)
    }

    /// Creates an inline flow from a set of inline fragments, then adds it as a child of the given
    /// flow or pushes it onto the given flow list.
    ///
    /// `#[inline(always)]` because this is performance critical and LLVM will not inline it
    /// otherwise.
    #[inline(always)]
    fn flush_inline_fragments_to_flow(
        &mut self,
        fragment_accumulator: InlineFragmentsAccumulator,
        flow: &mut FlowRef,
        absolute_descendants: &mut AbsoluteDescendants,
        legalizer: &mut Legalizer,
        node: &ConcreteThreadSafeLayoutNode,
    ) {
        let mut fragments = fragment_accumulator
            .get_intermediate_inline_fragments::<ConcreteThreadSafeLayoutNode>(
                self.style_context(),
            );
        if fragments.is_empty() {
            return;
        };

        strip_ignorable_whitespace_from_start(&mut fragments.fragments);
        strip_ignorable_whitespace_from_end(&mut fragments.fragments);
        if fragments.fragments.is_empty() {
            absolute_descendants.push_descendants(fragments.absolute_descendants);
            return;
        }

        // Build a list of all the inline-block fragments before fragments is moved.
        let mut inline_block_flows = vec![];
        for fragment in &fragments.fragments {
            match fragment.specific {
                SpecificFragmentInfo::InlineBlock(ref info) => {
                    inline_block_flows.push(info.flow_ref.clone())
                },
                SpecificFragmentInfo::InlineAbsoluteHypothetical(ref info) => {
                    inline_block_flows.push(info.flow_ref.clone())
                },
                SpecificFragmentInfo::InlineAbsolute(ref info) => {
                    inline_block_flows.push(info.flow_ref.clone())
                },
                _ => {},
            }
        }

        // We must scan for runs before computing minimum ascent and descent because scanning
        // for runs might collapse so much whitespace away that only hypothetical fragments
        // remain. In that case the inline flow will compute its ascent and descent to be zero.
        let scanned_fragments = TextRunScanner::new().scan_for_runs(
            &self.layout_context.font_context,
            mem::take(&mut fragments.fragments),
        );
        let mut inline_flow_ref = FlowRef::new(Arc::new(InlineFlow::from_fragments(
            scanned_fragments,
            node.style(self.style_context()).writing_mode,
        )));

        // Add all the inline-block fragments as children of the inline flow.
        for inline_block_flow in &inline_block_flows {
            inline_flow_ref.add_new_child(inline_block_flow.clone());
        }

        // Set up absolute descendants as necessary.
        //
        // The inline flow itself may need to become the containing block for absolute descendants
        // in order to handle cases like:
        //
        //      <div>
        //          <span style="position: relative">
        //              <span style="position: absolute; ..."></span>
        //          </span>
        //      </div>
        //
        // See the comment above `flow::AbsoluteDescendantInfo` for more information.
        inline_flow_ref.take_applicable_absolute_descendants(&mut fragments.absolute_descendants);
        absolute_descendants.push_descendants(fragments.absolute_descendants);

        {
            // FIXME(#6503): Use Arc::get_mut().unwrap() here.
            let inline_flow = FlowRef::deref_mut(&mut inline_flow_ref).as_mut_inline();
            inline_flow.minimum_line_metrics = inline_flow.minimum_line_metrics(
                &self.layout_context.font_context,
                &node.style(self.style_context()),
            );
        }

        inline_flow_ref.finish();
        legalizer.add_child::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
            self.style_context(),
            flow,
            inline_flow_ref,
        )
    }

    fn build_block_flow_using_construction_result_of_child(
        &mut self,
        flow: &mut FlowRef,
        node: &ConcreteThreadSafeLayoutNode,
        kid: ConcreteThreadSafeLayoutNode,
        inline_fragment_accumulator: &mut InlineFragmentsAccumulator,
        abs_descendants: &mut AbsoluteDescendants,
        legalizer: &mut Legalizer,
    ) {
        match kid.get_construction_result() {
            ConstructionResult::None => {},
            ConstructionResult::Flow(kid_flow, kid_abs_descendants) => {
                // If kid_flow is TableCaptionFlow, kid_flow should be added under
                // TableWrapperFlow.
                if flow.is_table() && kid_flow.is_table_caption() {
                    let construction_result =
                        ConstructionResult::Flow(kid_flow, AbsoluteDescendants::new());
                    self.set_flow_construction_result(&kid, construction_result)
                } else {
                    if !kid_flow
                        .base()
                        .flags
                        .contains(FlowFlags::IS_ABSOLUTELY_POSITIONED)
                    {
                        // Flush any inline fragments that we were gathering up. This allows us to
                        // handle {ib} splits.
                        let old_inline_fragment_accumulator = mem::replace(
                            inline_fragment_accumulator,
                            InlineFragmentsAccumulator::new(),
                        );
                        self.flush_inline_fragments_to_flow(
                            old_inline_fragment_accumulator,
                            flow,
                            abs_descendants,
                            legalizer,
                            node,
                        );
                    }
                    legalizer.add_child::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                        self.style_context(),
                        flow,
                        kid_flow,
                    )
                }
                abs_descendants.push_descendants(kid_abs_descendants);
            },
            ConstructionResult::ConstructionItem(ConstructionItem::InlineFragments(
                InlineFragmentsConstructionResult {
                    splits,
                    fragments: successor_fragments,
                },
            )) => {
                // Add any {ib} splits.
                for split in splits {
                    // Pull apart the {ib} split object and push its predecessor fragments
                    // onto the list.
                    let InlineBlockSplit {
                        predecessors,
                        flow: kid_flow,
                    } = split;
                    inline_fragment_accumulator.push_all(predecessors);

                    // Flush any inline fragments that we were gathering up.
                    debug!(
                        "flushing {} inline box(es) to flow A",
                        inline_fragment_accumulator.fragments.fragments.len()
                    );
                    let old_inline_fragment_accumulator = mem::replace(
                        inline_fragment_accumulator,
                        InlineFragmentsAccumulator::new(),
                    );
                    let absolute_descendants =
                        &mut inline_fragment_accumulator.fragments.absolute_descendants;
                    self.flush_inline_fragments_to_flow(
                        old_inline_fragment_accumulator,
                        flow,
                        absolute_descendants,
                        legalizer,
                        node,
                    );

                    // Push the flow generated by the {ib} split onto our list of flows.
                    legalizer.add_child::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                        self.style_context(),
                        flow,
                        kid_flow,
                    )
                }

                // Add the fragments to the list we're maintaining.
                inline_fragment_accumulator.push_all(successor_fragments);
            },
            ConstructionResult::ConstructionItem(ConstructionItem::Whitespace(
                whitespace_node,
                whitespace_pseudo,
                whitespace_style,
                whitespace_damage,
            )) => {
                // Add whitespace results. They will be stripped out later on when
                // between block elements, and retained when between inline elements.
                let fragment_info = SpecificFragmentInfo::UnscannedText(Box::new(
                    UnscannedTextFragmentInfo::new(Box::<str>::from(" "), None),
                ));
                let fragment = Fragment::from_opaque_node_and_style(
                    whitespace_node,
                    whitespace_pseudo,
                    whitespace_style,
                    node.selected_style(),
                    whitespace_damage,
                    fragment_info,
                );
                inline_fragment_accumulator
                    .fragments
                    .fragments
                    .push_back(fragment);
            },
            ConstructionResult::ConstructionItem(ConstructionItem::TableColumnFragment(_)) => {
                // TODO: Implement anonymous table objects for missing parents
                // CSS 2.1 § 17.2.1, step 3-2
            },
        }
    }

    /// Constructs a block flow, beginning with the given `initial_fragments` if present and then
    /// appending the construction results of children to the child list of the block flow. {ib}
    /// splits and absolutely-positioned descendants are handled correctly.
    fn build_flow_for_block_starting_with_fragments(
        &mut self,
        mut flow: FlowRef,
        node: &ConcreteThreadSafeLayoutNode,
        initial_fragments: IntermediateInlineFragments,
    ) -> ConstructionResult {
        // Gather up fragments for the inline flows we might need to create.
        let mut inline_fragment_accumulator = InlineFragmentsAccumulator::new();

        inline_fragment_accumulator
            .fragments
            .push_all(initial_fragments);

        // List of absolute descendants, in tree order.
        let mut abs_descendants = AbsoluteDescendants::new();
        let mut legalizer = Legalizer::new();
        let is_media_element_with_widget = node.type_id() ==
            Some(LayoutNodeType::Element(LayoutElementType::HTMLMediaElement)) &&
            node.as_element().unwrap().is_shadow_host();
        if !node.is_replaced_content() || is_media_element_with_widget {
            for kid in node.children() {
                if kid.get_pseudo_element_type() != PseudoElementType::Normal {
                    if node.is_replaced_content() {
                        // Replaced elements don't have pseudo-elements per spec.
                        continue;
                    }
                    self.process(&kid);
                }

                self.build_block_flow_using_construction_result_of_child(
                    &mut flow,
                    node,
                    kid,
                    &mut inline_fragment_accumulator,
                    &mut abs_descendants,
                    &mut legalizer,
                );
            }
        }

        // Perform a final flush of any inline fragments that we were gathering up to handle {ib}
        // splits, after stripping ignorable whitespace.
        self.flush_inline_fragments_to_flow(
            inline_fragment_accumulator,
            &mut flow,
            &mut abs_descendants,
            &mut legalizer,
            node,
        );

        // The flow is done.
        legalizer.finish(&mut flow);
        flow.finish();

        // Set up the absolute descendants.
        if flow.is_absolute_containing_block() {
            // This is the containing block for all the absolute descendants.
            flow.set_absolute_descendants(abs_descendants);

            abs_descendants = AbsoluteDescendants::new();
            if flow
                .base()
                .flags
                .contains(FlowFlags::IS_ABSOLUTELY_POSITIONED)
            {
                // This is now the only absolute flow in the subtree which hasn't yet
                // reached its CB.
                abs_descendants.push(flow.clone());
            }
        }
        ConstructionResult::Flow(flow, abs_descendants)
    }

    /// Constructs a flow for the given block node and its children. This method creates an
    /// initial fragment as appropriate and then dispatches to
    /// `build_flow_for_block_starting_with_fragments`. Currently the following kinds of flows get
    /// initial content:
    ///
    /// * Generated content gets the initial content specified by the `content` attribute of the
    ///   CSS.
    /// * `<input>` and `<textarea>` elements get their content.
    ///
    /// FIXME(pcwalton): It is not clear to me that there isn't a cleaner way to handle
    /// `<textarea>`.
    fn build_flow_for_block_like(
        &mut self,
        flow: FlowRef,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        let mut fragments = IntermediateInlineFragments::new();
        let node_is_input_or_text_area = node.type_id() ==
            Some(LayoutNodeType::Element(LayoutElementType::HTMLInputElement)) ||
            node.type_id() ==
                Some(LayoutNodeType::Element(
                    LayoutElementType::HTMLTextAreaElement,
                ));
        if node.get_pseudo_element_type().is_replaced_content() || node_is_input_or_text_area {
            // A TextArea's text contents are displayed through the input text
            // box, so don't construct them.
            if node.type_id() ==
                Some(LayoutNodeType::Element(
                    LayoutElementType::HTMLTextAreaElement,
                ))
            {
                for kid in node.children() {
                    self.set_flow_construction_result(&kid, ConstructionResult::None)
                }
            }

            let context = self.style_context();
            let mut style = node.style(context);
            style = context
                .stylist
                .style_for_anonymous::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                    &context.guards,
                    &PseudoElement::ServoLegacyText,
                    &style,
                );
            if node_is_input_or_text_area {
                style = context
                    .stylist
                    .style_for_anonymous::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                        &context.guards,
                        &PseudoElement::ServoLegacyInputText,
                        &style,
                    )
            }

            self.create_fragments_for_node_text_content(&mut fragments, node, &style)
        }
        self.build_flow_for_block_starting_with_fragments(flow, node, fragments)
    }

    /// Pushes fragments appropriate for the content of the given node onto the given list.
    fn create_fragments_for_node_text_content(
        &self,
        fragments: &mut IntermediateInlineFragments,
        node: &ConcreteThreadSafeLayoutNode,
        style: &ServoArc<ComputedValues>,
    ) {
        // Fast path: If there is no text content, return immediately.
        let text_content = node.text_content();
        if text_content.is_empty() {
            return;
        }

        let style = (*style).clone();
        let selected_style = node.selected_style();

        match text_content {
            TextContent::Text(string) => {
                let info = Box::new(UnscannedTextFragmentInfo::new(string, node.selection()));
                let specific_fragment_info = SpecificFragmentInfo::UnscannedText(info);
                fragments
                    .fragments
                    .push_back(Fragment::from_opaque_node_and_style(
                        node.opaque(),
                        node.get_pseudo_element_type(),
                        style,
                        selected_style,
                        node.restyle_damage(),
                        specific_fragment_info,
                    ))
            },
            TextContent::GeneratedContent(content_items) => {
                for content_item in content_items.into_iter() {
                    let specific_fragment_info = match content_item {
                        ContentItem::String(string) => {
                            let info =
                                Box::new(UnscannedTextFragmentInfo::new(string.into(), None));
                            SpecificFragmentInfo::UnscannedText(info)
                        },
                        ContentItem::Attr(attr) => {
                            let element = node.as_element().expect("Expected an element");
                            let attr_val = element
                                .get_attr(&attr.namespace_url, &LocalName::from(&*attr.attribute));
                            let string = attr_val.map_or("".to_string(), |s| s.to_string());
                            let info =
                                Box::new(UnscannedTextFragmentInfo::new(string.into(), None));
                            SpecificFragmentInfo::UnscannedText(info)
                        },
                        content_item => {
                            let content_item =
                                Box::new(GeneratedContentInfo::ContentItem(content_item));
                            SpecificFragmentInfo::GeneratedContent(content_item)
                        },
                    };
                    fragments
                        .fragments
                        .push_back(Fragment::from_opaque_node_and_style(
                            node.opaque(),
                            node.get_pseudo_element_type(),
                            style.clone(),
                            selected_style.clone(),
                            node.restyle_damage(),
                            specific_fragment_info,
                        ))
                }
            },
        }
    }

    /// Builds a flow for a node with `display: block`. This yields a `BlockFlow` with possibly
    /// other `BlockFlow`s or `InlineFlow`s underneath it, depending on whether {ib} splits needed
    /// to happen.
    fn build_flow_for_block(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
        float_kind: Option<FloatKind>,
    ) -> ConstructionResult {
        if node.style(self.style_context()).is_multicol() {
            return self.build_flow_for_multicol(node, float_kind);
        }

        let fragment = self.build_fragment_for_block(node);
        let flow = FlowRef::new(Arc::new(BlockFlow::from_fragment_and_float_kind(
            fragment, float_kind,
        )));
        self.build_flow_for_block_like(flow, node)
    }

    /// Bubbles up {ib} splits.
    fn accumulate_inline_block_splits(
        &mut self,
        splits: LinkedList<InlineBlockSplit>,
        node: &ConcreteThreadSafeLayoutNode,
        fragment_accumulator: &mut InlineFragmentsAccumulator,
        opt_inline_block_splits: &mut LinkedList<InlineBlockSplit>,
    ) {
        for split in splits {
            let InlineBlockSplit {
                predecessors,
                flow: kid_flow,
            } = split;
            fragment_accumulator.push_all(predecessors);

            opt_inline_block_splits.push_back(InlineBlockSplit::new(
                fragment_accumulator,
                node,
                self.style_context(),
                kid_flow,
            ));
        }
    }

    /// Concatenates the fragments of kids, adding in our own borders/padding/margins if necessary.
    /// Returns the `InlineFragmentsConstructionResult`, if any. There will be no
    /// `InlineFragmentsConstructionResult` if this node consisted entirely of ignorable
    /// whitespace.
    fn build_fragments_for_nonreplaced_inline_content(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        let mut opt_inline_block_splits: LinkedList<InlineBlockSplit> = LinkedList::new();
        let mut fragment_accumulator =
            InlineFragmentsAccumulator::from_inline_node(node, self.style_context());
        fragment_accumulator.bidi_control_chars =
            bidi_control_chars(&node.style(self.style_context()));

        let mut abs_descendants = AbsoluteDescendants::new();

        // Concatenate all the fragments of our kids, creating {ib} splits as necessary.
        let mut is_empty = true;
        for kid in node.children() {
            is_empty = false;
            if kid.get_pseudo_element_type() != PseudoElementType::Normal {
                self.process(&kid);
            }
            match kid.get_construction_result() {
                ConstructionResult::None => {},
                ConstructionResult::Flow(flow, kid_abs_descendants) => {
                    if !flow
                        .base()
                        .flags
                        .contains(FlowFlags::IS_ABSOLUTELY_POSITIONED)
                    {
                        opt_inline_block_splits.push_back(InlineBlockSplit::new(
                            &mut fragment_accumulator,
                            node,
                            self.style_context(),
                            flow,
                        ));
                        abs_descendants.push_descendants(kid_abs_descendants);
                    } else {
                        // Push the absolutely-positioned kid as an inline containing block.
                        let kid_node = flow.as_block().fragment.node;
                        let kid_pseudo = flow.as_block().fragment.pseudo;
                        let kid_style = flow.as_block().fragment.style.clone();
                        let kid_selected_style = flow.as_block().fragment.selected_style.clone();
                        let kid_restyle_damage = flow.as_block().fragment.restyle_damage;
                        let fragment_info = SpecificFragmentInfo::InlineAbsolute(
                            InlineAbsoluteFragmentInfo::new(flow),
                        );
                        fragment_accumulator.push(Fragment::from_opaque_node_and_style(
                            kid_node,
                            kid_pseudo,
                            kid_style,
                            kid_selected_style,
                            kid_restyle_damage,
                            fragment_info,
                        ));
                        fragment_accumulator
                            .fragments
                            .absolute_descendants
                            .push_descendants(kid_abs_descendants);
                    }
                },
                ConstructionResult::ConstructionItem(ConstructionItem::InlineFragments(
                    InlineFragmentsConstructionResult {
                        splits,
                        fragments: successors,
                    },
                )) => {
                    // Bubble up {ib} splits.
                    self.accumulate_inline_block_splits(
                        splits,
                        node,
                        &mut fragment_accumulator,
                        &mut opt_inline_block_splits,
                    );

                    // Push residual fragments.
                    fragment_accumulator.push_all(successors);
                },
                ConstructionResult::ConstructionItem(ConstructionItem::Whitespace(
                    whitespace_node,
                    whitespace_pseudo,
                    whitespace_style,
                    whitespace_damage,
                )) => {
                    // Instantiate the whitespace fragment.
                    let fragment_info = SpecificFragmentInfo::UnscannedText(Box::new(
                        UnscannedTextFragmentInfo::new(Box::<str>::from(" "), None),
                    ));
                    let fragment = Fragment::from_opaque_node_and_style(
                        whitespace_node,
                        whitespace_pseudo,
                        whitespace_style,
                        node.selected_style(),
                        whitespace_damage,
                        fragment_info,
                    );
                    fragment_accumulator.fragments.fragments.push_back(fragment)
                },
                ConstructionResult::ConstructionItem(ConstructionItem::TableColumnFragment(_)) => {
                    // TODO: Implement anonymous table objects for missing parents
                    // CSS 2.1 § 17.2.1, step 3-2
                },
            }
        }

        let node_style = node.style(self.style_context());
        if is_empty && has_padding_or_border(&node_style) {
            // An empty inline box needs at least one fragment to draw its background and borders.
            let info = SpecificFragmentInfo::UnscannedText(Box::new(
                UnscannedTextFragmentInfo::new(Box::<str>::from(""), None),
            ));
            let fragment = Fragment::from_opaque_node_and_style(
                node.opaque(),
                node.get_pseudo_element_type(),
                node_style.clone(),
                node.selected_style(),
                node.restyle_damage(),
                info,
            );
            fragment_accumulator.fragments.fragments.push_back(fragment)
        }

        // Finally, make a new construction result.
        if !opt_inline_block_splits.is_empty() ||
            !fragment_accumulator.fragments.is_empty() ||
            !abs_descendants.is_empty()
        {
            fragment_accumulator
                .fragments
                .absolute_descendants
                .push_descendants(abs_descendants);

            // If the node is positioned, then it's the containing block for all absolutely-
            // positioned descendants.
            if node_style.get_box().position != Position::Static {
                fragment_accumulator
                    .fragments
                    .absolute_descendants
                    .mark_as_having_reached_containing_block();
            }

            let construction_item =
                ConstructionItem::InlineFragments(InlineFragmentsConstructionResult {
                    splits: opt_inline_block_splits,
                    fragments: fragment_accumulator
                        .get_intermediate_inline_fragments::<ConcreteThreadSafeLayoutNode>(
                            self.style_context(),
                        ),
                });
            ConstructionResult::ConstructionItem(construction_item)
        } else {
            ConstructionResult::None
        }
    }

    /// Creates an `InlineFragmentsConstructionResult` for replaced content. Replaced content
    /// doesn't render its children, so this just nukes a child's fragments and creates a
    /// `Fragment`.
    fn build_fragments_for_replaced_inline_content(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        for kid in node.children() {
            self.set_flow_construction_result(&kid, ConstructionResult::None)
        }

        let context = self.style_context();
        let style = node.style(context);
        // If this node is ignorable whitespace, bail out now.
        if node.is_ignorable_whitespace(context) {
            return ConstructionResult::ConstructionItem(ConstructionItem::Whitespace(
                node.opaque(),
                node.get_pseudo_element_type(),
                context
                    .stylist
                    .style_for_anonymous::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                        &context.guards,
                        &PseudoElement::ServoLegacyText,
                        &style,
                    ),
                node.restyle_damage(),
            ));
        }

        // If this is generated content, then we need to initialize the accumulator with the
        // fragment corresponding to that content. Otherwise, just initialize with the ordinary
        // fragment that needs to be generated for this inline node.
        let mut fragments = IntermediateInlineFragments::new();
        match (node.get_pseudo_element_type(), node.type_id()) {
            (_, Some(LayoutNodeType::Text)) => {
                let text_style = context
                    .stylist
                    .style_for_anonymous::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                        &context.guards,
                        &PseudoElement::ServoLegacyText,
                        &style,
                    );
                self.create_fragments_for_node_text_content(&mut fragments, node, &text_style)
            },
            (PseudoElementType::Normal, _) => {
                fragments
                    .fragments
                    .push_back(self.build_fragment_for_block(node));
            },
            (_, _) => self.create_fragments_for_node_text_content(&mut fragments, node, &style),
        }

        let construction_item =
            ConstructionItem::InlineFragments(InlineFragmentsConstructionResult {
                splits: LinkedList::new(),
                fragments,
            });
        ConstructionResult::ConstructionItem(construction_item)
    }

    /// Build the fragment for an inline-block or inline-flex, based on the `display` flag
    fn build_fragment_for_inline_block_or_inline_flex(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
        display: Display,
    ) -> ConstructionResult {
        let block_flow_result = match display {
            Display::InlineBlock => self.build_flow_for_block(node, None),
            Display::InlineFlex => self.build_flow_for_flex(node, None),
            _ => panic!("The flag should be inline-block or inline-flex"),
        };
        let (block_flow, abs_descendants) = match block_flow_result {
            ConstructionResult::Flow(block_flow, abs_descendants) => (block_flow, abs_descendants),
            _ => unreachable!(),
        };

        let context = self.style_context();
        let style = node.style(context);
        let style = context
            .stylist
            .style_for_anonymous::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                &context.guards,
                &PseudoElement::ServoLegacyInlineBlockWrapper,
                &style,
            );
        let fragment_info =
            SpecificFragmentInfo::InlineBlock(InlineBlockFragmentInfo::new(block_flow));
        let fragment = Fragment::from_opaque_node_and_style(
            node.opaque(),
            node.get_pseudo_element_type(),
            style,
            node.selected_style(),
            node.restyle_damage(),
            fragment_info,
        );

        let mut fragment_accumulator = InlineFragmentsAccumulator::new();
        fragment_accumulator.fragments.fragments.push_back(fragment);
        fragment_accumulator
            .fragments
            .absolute_descendants
            .push_descendants(abs_descendants);

        let construction_item =
            ConstructionItem::InlineFragments(InlineFragmentsConstructionResult {
                splits: LinkedList::new(),
                fragments: fragment_accumulator
                    .get_intermediate_inline_fragments::<ConcreteThreadSafeLayoutNode>(context),
            });
        ConstructionResult::ConstructionItem(construction_item)
    }

    /// This is an annoying case, because the computed `display` value is `block`, but the
    /// hypothetical box is inline.
    fn build_fragment_for_absolutely_positioned_inline(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        let block_flow_result = self.build_flow_for_block(node, None);
        let (block_flow, abs_descendants) = match block_flow_result {
            ConstructionResult::Flow(block_flow, abs_descendants) => (block_flow, abs_descendants),
            _ => unreachable!(),
        };

        let fragment_info = SpecificFragmentInfo::InlineAbsoluteHypothetical(
            InlineAbsoluteHypotheticalFragmentInfo::new(block_flow),
        );
        let style_context = self.style_context();
        let style = node.style(style_context);
        let style = style_context
            .stylist
            .style_for_anonymous::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                &style_context.guards,
                &PseudoElement::ServoLegacyInlineAbsolute,
                &style,
            );
        let fragment = Fragment::from_opaque_node_and_style(
            node.opaque(),
            PseudoElementType::Normal,
            style,
            node.selected_style(),
            node.restyle_damage(),
            fragment_info,
        );

        let mut fragment_accumulator =
            InlineFragmentsAccumulator::from_inline_node(node, self.style_context());
        fragment_accumulator.fragments.fragments.push_back(fragment);
        fragment_accumulator
            .fragments
            .absolute_descendants
            .push_descendants(abs_descendants);

        let construction_item =
            ConstructionItem::InlineFragments(InlineFragmentsConstructionResult {
                splits: LinkedList::new(),
                fragments: fragment_accumulator
                    .get_intermediate_inline_fragments::<ConcreteThreadSafeLayoutNode>(
                        style_context,
                    ),
            });
        ConstructionResult::ConstructionItem(construction_item)
    }

    /// Builds one or more fragments for a node with `display: inline`. This yields an
    /// `InlineFragmentsConstructionResult`.
    fn build_fragments_for_inline(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        // Is this node replaced content?
        if !node.is_replaced_content() {
            // Go to a path that concatenates our kids' fragments.
            self.build_fragments_for_nonreplaced_inline_content(node)
        } else {
            // Otherwise, just nuke our kids' fragments, create our fragment if any, and be done
            // with it.
            self.build_fragments_for_replaced_inline_content(node)
        }
    }

    /// Places any table captions found under the given table wrapper, if the value of their
    /// `caption-side` property is equal to the given `side`.
    fn place_table_caption_under_table_wrapper_on_side(
        &mut self,
        table_wrapper_flow: &mut FlowRef,
        node: &ConcreteThreadSafeLayoutNode,
        side: CaptionSide,
    ) {
        // Only flows that are table captions are matched here.
        for kid in node.children() {
            match kid.get_construction_result() {
                ConstructionResult::Flow(kid_flow, _) => {
                    if kid_flow.is_table_caption() &&
                        kid_flow
                            .as_block()
                            .fragment
                            .style()
                            .get_inherited_table()
                            .caption_side ==
                            side
                    {
                        table_wrapper_flow.add_new_child(kid_flow);
                    }
                },
                ConstructionResult::None | ConstructionResult::ConstructionItem(_) => {},
            }
        }
    }

    /// Builds a flow for a node with `column-count` or `column-width` non-`auto`.
    /// This yields a `MulticolFlow` with a single `MulticolColumnFlow` underneath it.
    fn build_flow_for_multicol(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
        float_kind: Option<FloatKind>,
    ) -> ConstructionResult {
        let fragment = Fragment::new(node, SpecificFragmentInfo::Multicol, self.layout_context);
        let mut flow = FlowRef::new(Arc::new(MulticolFlow::from_fragment(fragment, float_kind)));

        let column_fragment = Fragment::new(
            node,
            SpecificFragmentInfo::MulticolColumn,
            self.layout_context,
        );
        let column_flow =
            FlowRef::new(Arc::new(MulticolColumnFlow::from_fragment(column_fragment)));

        // First populate the column flow with its children.
        let construction_result = self.build_flow_for_block_like(column_flow, node);

        let mut abs_descendants = AbsoluteDescendants::new();

        if let ConstructionResult::Flow(column_flow, column_abs_descendants) = construction_result {
            flow.add_new_child(column_flow);
            abs_descendants.push_descendants(column_abs_descendants);
        }

        // The flow is done.
        flow.finish();
        if flow.is_absolute_containing_block() {
            // This is the containing block for all the absolute descendants.
            flow.set_absolute_descendants(abs_descendants);

            abs_descendants = AbsoluteDescendants::new();

            if flow
                .base()
                .flags
                .contains(FlowFlags::IS_ABSOLUTELY_POSITIONED)
            {
                // This is now the only absolute flow in the subtree which hasn't yet
                // reached its containing block.
                abs_descendants.push(flow.clone());
            }
        }

        ConstructionResult::Flow(flow, abs_descendants)
    }

    /// Builds a flow for a node with `display: table`. This yields a `TableWrapperFlow` with
    /// possibly other `TableCaptionFlow`s or `TableFlow`s underneath it.
    fn build_flow_for_table(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
        float_value: Float,
    ) -> ConstructionResult {
        let mut legalizer = Legalizer::new();

        let table_style;
        let wrapper_style;
        {
            let context = self.style_context();
            table_style = node.style(context);
            wrapper_style = context
                .stylist
                .style_for_anonymous::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                    &context.guards,
                    &PseudoElement::ServoLegacyTableWrapper,
                    &table_style,
                );
        }
        // FIXME: this should use the writing mode of the containing block.
        let wrapper_float_kind =
            FloatKind::from_property_and_writing_mode(float_value, wrapper_style.writing_mode);
        let wrapper_fragment = Fragment::from_opaque_node_and_style(
            node.opaque(),
            PseudoElementType::Normal,
            wrapper_style,
            node.selected_style(),
            node.restyle_damage(),
            SpecificFragmentInfo::TableWrapper,
        );
        let mut wrapper_flow = FlowRef::new(Arc::new(
            TableWrapperFlow::from_fragment_and_float_kind(wrapper_fragment, wrapper_float_kind),
        ));

        let table_fragment = Fragment::new(node, SpecificFragmentInfo::Table, self.layout_context);
        let table_flow = FlowRef::new(Arc::new(TableFlow::from_fragment(table_fragment)));

        // First populate the table flow with its children.
        let construction_result = self.build_flow_for_block_like(table_flow, node);

        let mut abs_descendants = AbsoluteDescendants::new();

        // The order of the caption and the table are not necessarily the same order as in the DOM
        // tree. All caption blocks are placed before or after the table flow, depending on the
        // value of `caption-side`.
        self.place_table_caption_under_table_wrapper_on_side(
            &mut wrapper_flow,
            node,
            CaptionSide::Top,
        );

        if let ConstructionResult::Flow(table_flow, table_abs_descendants) = construction_result {
            legalizer.add_child::<ConcreteThreadSafeLayoutNode::ConcreteElement>(
                self.style_context(),
                &mut wrapper_flow,
                table_flow,
            );
            abs_descendants.push_descendants(table_abs_descendants);
        }

        // If the value of `caption-side` is `bottom`, place it now.
        self.place_table_caption_under_table_wrapper_on_side(
            &mut wrapper_flow,
            node,
            CaptionSide::Bottom,
        );

        // The flow is done.
        legalizer.finish(&mut wrapper_flow);
        wrapper_flow.finish();

        if wrapper_flow.is_absolute_containing_block() {
            // This is the containing block for all the absolute descendants.
            wrapper_flow.set_absolute_descendants(abs_descendants);

            abs_descendants = AbsoluteDescendants::new();

            if wrapper_flow
                .base()
                .flags
                .contains(FlowFlags::IS_ABSOLUTELY_POSITIONED)
            {
                // This is now the only absolute flow in the subtree which hasn't yet
                // reached its containing block.
                abs_descendants.push(wrapper_flow.clone());
            }
        }

        ConstructionResult::Flow(wrapper_flow, abs_descendants)
    }

    /// Builds a flow for a node with `display: table-caption`. This yields a `TableCaptionFlow`
    /// with possibly other `BlockFlow`s or `InlineFlow`s underneath it.
    fn build_flow_for_table_caption(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        let fragment = self.build_fragment_for_block(node);
        let flow = FlowRef::new(Arc::new(TableCaptionFlow::from_fragment(fragment)));
        self.build_flow_for_block_like(flow, node)
    }

    /// Builds a flow for a node with `display: table-row-group`. This yields a `TableRowGroupFlow`
    /// with possibly other `TableRowFlow`s underneath it.
    fn build_flow_for_table_rowgroup(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        let fragment = Fragment::new(node, SpecificFragmentInfo::TableRow, self.layout_context);
        let flow = FlowRef::new(Arc::new(TableRowGroupFlow::from_fragment(fragment)));
        self.build_flow_for_block_like(flow, node)
    }

    /// Builds a flow for a node with `display: table-row`. This yields a `TableRowFlow` with
    /// possibly other `TableCellFlow`s underneath it.
    fn build_flow_for_table_row(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        let fragment = Fragment::new(node, SpecificFragmentInfo::TableRow, self.layout_context);
        let flow = FlowRef::new(Arc::new(TableRowFlow::from_fragment(fragment)));
        self.build_flow_for_block_like(flow, node)
    }

    /// Builds a flow for a node with `display: table-cell`. This yields a `TableCellFlow` with
    /// possibly other `BlockFlow`s or `InlineFlow`s underneath it.
    fn build_flow_for_table_cell(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        let fragment = Fragment::new(node, SpecificFragmentInfo::TableCell, self.layout_context);

        // Determine if the table cell should be hidden. Per CSS 2.1 § 17.6.1.1, this will be true
        // if the cell has any in-flow elements (even empty ones!) and has `empty-cells` set to
        // `hide`.
        let hide = node
            .style(self.style_context())
            .get_inherited_table()
            .empty_cells ==
            EmptyCells::Hide &&
            node.children().all(|kid| {
                let position = kid.style(self.style_context()).get_box().position;
                !kid.is_content() || position == Position::Absolute || position == Position::Fixed
            });

        let flow = FlowRef::new(Arc::new(
            TableCellFlow::from_node_fragment_and_visibility_flag(node, fragment, !hide),
        ));
        self.build_flow_for_block_like(flow, node)
    }

    /// Builds a flow for a node with `display: list-item`. This yields a `ListItemFlow` with
    /// possibly other `BlockFlow`s or `InlineFlow`s underneath it.
    fn build_flow_for_list_item(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
        flotation: Float,
    ) -> ConstructionResult {
        // FIXME: this should use the writing mode of the containing block.
        let style = node.style(self.style_context());
        let flotation = FloatKind::from_property_and_writing_mode(flotation, style.writing_mode);
        let marker_fragments = match style.get_list().list_style_image {
            Image::Url(ref url_value) => {
                let image_info = Box::new(ImageFragmentInfo::new(
                    url_value.url().cloned().map(Into::into),
                    None,
                    node,
                    self.layout_context,
                ));
                vec![Fragment::new(
                    node,
                    SpecificFragmentInfo::Image(image_info),
                    self.layout_context,
                )]
            },
            // XXX: Non-None image types unimplemented.
            Image::ImageSet(..) |
            Image::Gradient(..) |
            Image::PaintWorklet(..) |
            Image::CrossFade(..) |
            Image::None => {
                match ListStyleTypeContent::from_list_style_type(style.get_list().list_style_type) {
                    ListStyleTypeContent::None => Vec::new(),
                    ListStyleTypeContent::StaticText(ch) => {
                        let text = format!("{}\u{a0}", ch);
                        let mut unscanned_marker_fragments = LinkedList::new();
                        unscanned_marker_fragments.push_back(Fragment::new(
                            node,
                            SpecificFragmentInfo::UnscannedText(Box::new(
                                UnscannedTextFragmentInfo::new(Box::<str>::from(text), None),
                            )),
                            self.layout_context,
                        ));
                        let marker_fragments = TextRunScanner::new().scan_for_runs(
                            &self.layout_context.font_context,
                            unscanned_marker_fragments,
                        );
                        marker_fragments.fragments
                    },
                    ListStyleTypeContent::GeneratedContent(info) => vec![Fragment::new(
                        node,
                        SpecificFragmentInfo::GeneratedContent(info),
                        self.layout_context,
                    )],
                }
            },
        };

        // If the list marker is outside, it becomes the special "outside fragment" that list item
        // flows have. If it's inside, it's just a plain old fragment. Note that this means that
        // we adopt Gecko's behavior rather than WebKit's when the marker causes an {ib} split,
        // which has caused some malaise (Bugzilla #36854) but CSS 2.1 § 12.5.1 lets me do it, so
        // there.
        let mut initial_fragments = IntermediateInlineFragments::new();
        let main_fragment = self.build_fragment_for_block(node);
        let flow = match style.get_list().list_style_position {
            ListStylePosition::Outside => Arc::new(ListItemFlow::from_fragments_and_flotation(
                main_fragment,
                marker_fragments,
                flotation,
            )),
            ListStylePosition::Inside => {
                for marker_fragment in marker_fragments {
                    initial_fragments.fragments.push_back(marker_fragment)
                }
                Arc::new(ListItemFlow::from_fragments_and_flotation(
                    main_fragment,
                    vec![],
                    flotation,
                ))
            },
        };

        self.build_flow_for_block_starting_with_fragments(
            FlowRef::new(flow),
            node,
            initial_fragments,
        )
    }

    /// Creates a fragment for a node with `display: table-column`.
    fn build_fragments_for_table_column(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        // CSS 2.1 § 17.2.1. Treat all child fragments of a `table-column` as `display: none`.
        for kid in node.children() {
            self.set_flow_construction_result(&kid, ConstructionResult::None)
        }

        let specific = SpecificFragmentInfo::TableColumn(TableColumnFragmentInfo::new(node));
        let construction_item = ConstructionItem::TableColumnFragment(Fragment::new(
            node,
            specific,
            self.layout_context,
        ));
        ConstructionResult::ConstructionItem(construction_item)
    }

    /// Builds a flow for a node with `display: table-column-group`.
    /// This yields a `TableColGroupFlow`.
    fn build_flow_for_table_colgroup(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
    ) -> ConstructionResult {
        let fragment = Fragment::new(
            node,
            SpecificFragmentInfo::TableColumn(TableColumnFragmentInfo::new(node)),
            self.layout_context,
        );
        let mut col_fragments = vec![];
        for kid in node.children() {
            // CSS 2.1 § 17.2.1. Treat all non-column child fragments of `table-column-group`
            // as `display: none`.
            if let ConstructionResult::ConstructionItem(ConstructionItem::TableColumnFragment(
                fragment,
            )) = kid.get_construction_result()
            {
                col_fragments.push(fragment)
            }
        }
        if col_fragments.is_empty() {
            debug!("add SpecificFragmentInfo::TableColumn for empty colgroup");
            let specific = SpecificFragmentInfo::TableColumn(TableColumnFragmentInfo::new(node));
            col_fragments.push(Fragment::new(node, specific, self.layout_context));
        }
        let mut flow = FlowRef::new(Arc::new(TableColGroupFlow::from_fragments(
            fragment,
            col_fragments,
        )));
        flow.finish();

        ConstructionResult::Flow(flow, AbsoluteDescendants::new())
    }

    /// Builds a flow for a node with 'display: flex'.
    fn build_flow_for_flex(
        &mut self,
        node: &ConcreteThreadSafeLayoutNode,
        float_kind: Option<FloatKind>,
    ) -> ConstructionResult {
        let fragment = self.build_fragment_for_block(node);
        let flow = FlowRef::new(Arc::new(FlexFlow::from_fragment(fragment, float_kind)));
        self.build_flow_for_block_like(flow, node)
    }

    /// Attempts to perform incremental repair to account for recent changes to this node. This
    /// can fail and return false, indicating that flows will need to be reconstructed.
    ///
    /// TODO(pcwalton): Add some more fast paths, like toggling `display: none`, adding block kids
    /// to block parents with no {ib} splits, adding out-of-flow kids, etc.
    pub fn repair_if_possible(&mut self, node: &ConcreteThreadSafeLayoutNode) -> bool {
        // We can skip reconstructing the flow if we don't have to reconstruct and none of our kids
        // did either.
        //
        // We visit the kids first and reset their HAS_NEWLY_CONSTRUCTED_FLOW flags after checking
        // them.  NOTE: Make sure not to bail out early before resetting all the flags!
        let mut need_to_reconstruct = false;

        // If the node has display: none, it's possible that we haven't even
        // styled the children once, so we need to bailout early here.
        if node.style(self.style_context()).get_box().clone_display() == Display::None {
            return false;
        }

        for kid in node.children() {
            if kid
                .flags()
                .contains(LayoutDataFlags::HAS_NEWLY_CONSTRUCTED_FLOW)
            {
                kid.remove_flags(LayoutDataFlags::HAS_NEWLY_CONSTRUCTED_FLOW);
                need_to_reconstruct = true
            }
        }

        if need_to_reconstruct {
            return false;
        }

        if node
            .restyle_damage()
            .contains(ServoRestyleDamage::RECONSTRUCT_FLOW)
        {
            return false;
        }

        let mut set_has_newly_constructed_flow_flag = false;
        let result = {
            let style = node.style(self.style_context());

            if style.can_be_fragmented() || style.is_multicol() {
                return false;
            }

            let damage = node.restyle_damage();
            let mut data = node.mutate_layout_data().unwrap();

            match *node.construction_result_mut(&mut data) {
                ConstructionResult::None => true,
                ConstructionResult::Flow(ref mut flow, _) => {
                    // The node's flow is of the same type and has the same set of children and can
                    // therefore be repaired by simply propagating damage and style to the flow.
                    if !flow.is_block_flow() {
                        return false;
                    }

                    let flow = FlowRef::deref_mut(flow);
                    flow.mut_base().restyle_damage.insert(damage);
                    flow.repair_style_and_bubble_inline_sizes(&style);
                    true
                },
                ConstructionResult::ConstructionItem(ConstructionItem::InlineFragments(
                    ref mut inline_fragments_construction_result,
                )) => {
                    if !inline_fragments_construction_result.splits.is_empty() {
                        return false;
                    }

                    for fragment in inline_fragments_construction_result
                        .fragments
                        .fragments
                        .iter_mut()
                    {
                        // Only mutate the styles of fragments that represent the dirty node (including
                        // pseudo-element).
                        if fragment.node != node.opaque() {
                            continue;
                        }
                        if fragment.pseudo != node.get_pseudo_element_type() {
                            continue;
                        }

                        match fragment.specific {
                            SpecificFragmentInfo::InlineBlock(ref mut inline_block_fragment) => {
                                let flow_ref =
                                    FlowRef::deref_mut(&mut inline_block_fragment.flow_ref);
                                flow_ref.mut_base().restyle_damage.insert(damage);
                                // FIXME(pcwalton): Fragment restyle damage too?
                                flow_ref.repair_style_and_bubble_inline_sizes(&style);
                            },
                            SpecificFragmentInfo::InlineAbsoluteHypothetical(
                                ref mut inline_absolute_hypothetical_fragment,
                            ) => {
                                let flow_ref = FlowRef::deref_mut(
                                    &mut inline_absolute_hypothetical_fragment.flow_ref,
                                );
                                flow_ref.mut_base().restyle_damage.insert(damage);
                                // FIXME(pcwalton): Fragment restyle damage too?
                                flow_ref.repair_style_and_bubble_inline_sizes(&style);
                            },
                            SpecificFragmentInfo::InlineAbsolute(
                                ref mut inline_absolute_fragment,
                            ) => {
                                let flow_ref =
                                    FlowRef::deref_mut(&mut inline_absolute_fragment.flow_ref);
                                flow_ref.mut_base().restyle_damage.insert(damage);
                                // FIXME(pcwalton): Fragment restyle damage too?
                                flow_ref.repair_style_and_bubble_inline_sizes(&style);
                            },
                            SpecificFragmentInfo::ScannedText(_) => {
                                // Text fragments in ConstructionResult haven't been scanned yet
                                unreachable!()
                            },
                            SpecificFragmentInfo::GeneratedContent(_) |
                            SpecificFragmentInfo::UnscannedText(_) => {
                                // We can't repair this unscanned text; we need to update the
                                // scanned text fragments.
                                //
                                // TODO: Add code to find and repair the ScannedText fragments?
                                return false;
                            },
                            _ => {
                                fragment.repair_style(&style);
                                set_has_newly_constructed_flow_flag = true;
                            },
                        }
                    }
                    true
                },
                ConstructionResult::ConstructionItem(_) => false,
            }
        };
        if set_has_newly_constructed_flow_flag {
            node.insert_flags(LayoutDataFlags::HAS_NEWLY_CONSTRUCTED_FLOW);
        }
        result
    }
}

impl<'dom, ConcreteThreadSafeLayoutNode>
    PostorderNodeMutTraversal<'dom, ConcreteThreadSafeLayoutNode>
    for FlowConstructor<'_, ConcreteThreadSafeLayoutNode>
where
    ConcreteThreadSafeLayoutNode: ThreadSafeLayoutNode<'dom>,
{
    // Construct Flow based on 'display', 'position', and 'float' values.
    //
    // CSS 2.1 Section 9.7
    //
    // TODO: This should actually consult the table in that section to get the
    // final computed value for 'display'.
    fn process(&mut self, node: &ConcreteThreadSafeLayoutNode) {
        node.insert_flags(LayoutDataFlags::HAS_NEWLY_CONSTRUCTED_FLOW);

        let style = node.style(self.style_context());

        // Bail out if this node is display: none. The style system guarantees
        // that we don't arrive here for children of those.
        if style.get_box().display.is_none() {
            self.set_flow_construction_result(node, ConstructionResult::None);
            return;
        }

        // Get the `display` property for this node, and determine whether this node is floated.
        let (display, float, positioning) = match node.type_id() {
            None => {
                // Pseudo-element.
                (
                    style.get_box().display,
                    style.get_box().float,
                    style.get_box().position,
                )
            },
            Some(LayoutNodeType::Element(_)) => {
                let original_display = style.get_box().original_display;
                // FIXME(emilio, #19771): This munged_display business is pretty
                // wrong. After we fix this we should be able to unify the
                // pseudo-element path too.
                let munged_display = match original_display {
                    Display::Inline | Display::InlineBlock => original_display,
                    _ => style.get_box().display,
                };
                (
                    munged_display,
                    style.get_box().float,
                    style.get_box().position,
                )
            },
            Some(LayoutNodeType::Text) => (Display::Inline, Float::None, Position::Static),
        };

        debug!(
            "building flow for node: {:?} {:?} {:?} {:?}",
            display,
            float,
            positioning,
            node.type_id()
        );

        // FIXME(emilio): This should look at display-outside and
        // display-inside, but there's so much stuff that goes through the
        // generic "block" codepath (wrongly).
        //
        // Switch on display and floatedness.
        match (display, float, positioning) {
            // `display: none` contributes no flow construction result.
            (Display::None, _, _) => {
                self.set_flow_construction_result(node, ConstructionResult::None);
            },

            // Table items contribute table flow construction results.
            (Display::Table, float_value, _) => {
                let construction_result = self.build_flow_for_table(node, float_value);
                self.set_flow_construction_result(node, construction_result)
            },

            // Absolutely positioned elements will have computed value of
            // `float` as 'none' and `display` as per the table.
            // Only match here for block items. If an item is absolutely
            // positioned, but inline we shouldn't try to construct a block
            // flow here - instead, let it match the inline case
            // below.
            (Display::Block, _, Position::Absolute) | (Display::Block, _, Position::Fixed) => {
                let construction_result = self.build_flow_for_block(node, None);
                self.set_flow_construction_result(node, construction_result)
            },

            // Inline items that are absolutely-positioned contribute inline fragment construction
            // results with a hypothetical fragment.
            (Display::Inline, _, Position::Absolute) |
            (Display::Inline, _, Position::Fixed) |
            (Display::InlineBlock, _, Position::Absolute) |
            (Display::InlineBlock, _, Position::Fixed) => {
                let construction_result =
                    self.build_fragment_for_absolutely_positioned_inline(node);
                self.set_flow_construction_result(node, construction_result)
            },

            // Inline items contribute inline fragment construction results.
            //
            // FIXME(pcwalton, #3307): This is not sufficient to handle floated generated content.
            (Display::Inline, Float::None, _) => {
                let construction_result = self.build_fragments_for_inline(node);
                self.set_flow_construction_result(node, construction_result)
            },

            // Inline-block items contribute inline fragment construction results.
            (Display::InlineBlock, Float::None, _) => {
                let construction_result =
                    self.build_fragment_for_inline_block_or_inline_flex(node, Display::InlineBlock);
                self.set_flow_construction_result(node, construction_result)
            },

            // Table items contribute table flow construction results.
            (Display::TableCaption, _, _) => {
                let construction_result = self.build_flow_for_table_caption(node);
                self.set_flow_construction_result(node, construction_result)
            },

            // Table items contribute table flow construction results.
            (Display::TableColumnGroup, _, _) => {
                let construction_result = self.build_flow_for_table_colgroup(node);
                self.set_flow_construction_result(node, construction_result)
            },

            // Table items contribute table flow construction results.
            (Display::TableColumn, _, _) => {
                let construction_result = self.build_fragments_for_table_column(node);
                self.set_flow_construction_result(node, construction_result)
            },

            // Table items contribute table flow construction results.
            (Display::TableRowGroup, _, _) |
            (Display::TableHeaderGroup, _, _) |
            (Display::TableFooterGroup, _, _) => {
                let construction_result = self.build_flow_for_table_rowgroup(node);
                self.set_flow_construction_result(node, construction_result)
            },

            // Table items contribute table flow construction results.
            (Display::TableRow, _, _) => {
                let construction_result = self.build_flow_for_table_row(node);
                self.set_flow_construction_result(node, construction_result)
            },

            // Table items contribute table flow construction results.
            (Display::TableCell, _, _) => {
                let construction_result = self.build_flow_for_table_cell(node);
                self.set_flow_construction_result(node, construction_result)
            },

            // Flex items contribute flex flow construction results.
            (Display::Flex, float_value, _) => {
                // FIXME: this should use the writing mode of the containing block.
                let float_kind =
                    FloatKind::from_property_and_writing_mode(float_value, style.writing_mode);
                let construction_result = self.build_flow_for_flex(node, float_kind);
                self.set_flow_construction_result(node, construction_result)
            },

            (Display::InlineFlex, _, _) => {
                let construction_result =
                    self.build_fragment_for_inline_block_or_inline_flex(node, Display::InlineFlex);
                self.set_flow_construction_result(node, construction_result)
            },

            // Block flows that are not floated contribute block flow construction results.
            //
            // TODO(pcwalton): Make this only trigger for blocks and handle the other `display`
            // properties separately.
            (_, float_value, _) => {
                // FIXME: this should use the writing mode of the containing block.
                let float_kind =
                    FloatKind::from_property_and_writing_mode(float_value, style.writing_mode);
                // List items contribute their own special flows.
                let construction_result = if display.is_list_item() {
                    self.build_flow_for_list_item(node, float_value)
                } else {
                    self.build_flow_for_block(node, float_kind)
                };
                self.set_flow_construction_result(node, construction_result)
            },
        }
    }
}

/// A utility trait with some useful methods for node queries.
trait NodeUtils {
    /// Returns true if this node doesn't render its kids and false otherwise.
    fn is_replaced_content(&self) -> bool;

    fn construction_result_mut(self, layout_data: &mut InnerLayoutData) -> &mut ConstructionResult;

    /// Sets the construction result of a flow.
    fn set_flow_construction_result(self, result: ConstructionResult);

    /// Returns the construction result for this node.
    fn get_construction_result(self) -> ConstructionResult;
}

impl<'dom, ConcreteThreadSafeLayoutNode> NodeUtils for ConcreteThreadSafeLayoutNode
where
    ConcreteThreadSafeLayoutNode: ThreadSafeLayoutNode<'dom>,
{
    fn is_replaced_content(&self) -> bool {
        match self.type_id() {
            Some(LayoutNodeType::Text) |
            Some(LayoutNodeType::Element(LayoutElementType::HTMLImageElement)) |
            Some(LayoutNodeType::Element(LayoutElementType::HTMLMediaElement)) |
            Some(LayoutNodeType::Element(LayoutElementType::HTMLIFrameElement)) |
            Some(LayoutNodeType::Element(LayoutElementType::HTMLCanvasElement)) |
            Some(LayoutNodeType::Element(LayoutElementType::SVGSVGElement)) => true,
            Some(LayoutNodeType::Element(LayoutElementType::HTMLObjectElement)) => {
                let elem = self.as_element().unwrap();
                let type_and_data = (
                    elem.get_attr(&ns!(), &local_name!("type")),
                    elem.get_attr(&ns!(), &local_name!("data")),
                );
                match type_and_data {
                    (None, Some(uri)) => is_image_data(uri),
                    _ => false,
                }
            },
            Some(LayoutNodeType::Element(_)) => false,
            None => self.get_pseudo_element_type().is_replaced_content(),
        }
    }

    fn construction_result_mut(self, data: &mut InnerLayoutData) -> &mut ConstructionResult {
        match self.get_pseudo_element_type() {
            PseudoElementType::Before => &mut data.before_flow_construction_result,
            PseudoElementType::After => &mut data.after_flow_construction_result,
            PseudoElementType::DetailsSummary => &mut data.details_summary_flow_construction_result,
            PseudoElementType::DetailsContent => &mut data.details_content_flow_construction_result,
            PseudoElementType::Normal => &mut data.flow_construction_result,
        }
    }

    #[inline(always)]
    fn set_flow_construction_result(self, result: ConstructionResult) {
        let mut layout_data = self.mutate_layout_data().unwrap();
        let dst = self.construction_result_mut(&mut layout_data);
        *dst = result;
    }

    #[inline(always)]
    fn get_construction_result(self) -> ConstructionResult {
        let mut layout_data = self.mutate_layout_data().unwrap();
        self.construction_result_mut(&mut layout_data).get()
    }
}

impl FlowRef {
    /// Adds a new flow as a child of this flow. Fails if this flow is marked as a leaf.
    fn add_new_child(&mut self, mut new_child: FlowRef) {
        {
            let kid_base = FlowRef::deref_mut(&mut new_child).mut_base();
            kid_base.parallel.parent = parallel::mut_owned_flow_to_unsafe_flow(self);
        }

        let base = FlowRef::deref_mut(self).mut_base();
        base.children.push_back(new_child);
        let _ = base.parallel.children_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Finishes a flow. Once a flow is finished, no more child flows or fragments may be added to
    /// it. This will normally run the bubble-inline-sizes (minimum and preferred -- i.e. intrinsic
    /// -- inline-size) calculation, unless the global `bubble_inline-sizes_separately` flag is on.
    ///
    /// All flows must be finished at some point, or they will not have their intrinsic inline-sizes
    /// properly computed. (This is not, however, a memory safety problem.)
    fn finish(&mut self) {
        FlowRef::deref_mut(self).bubble_inline_sizes();
        FlowRef::deref_mut(self)
            .mut_base()
            .restyle_damage
            .remove(ServoRestyleDamage::BUBBLE_ISIZES);
    }
}

/// Strips ignorable whitespace from the start of a list of fragments.
pub fn strip_ignorable_whitespace_from_start(this: &mut LinkedList<Fragment>) {
    if this.is_empty() {
        return; // Fast path.
    }

    let mut leading_fragments_consisting_of_solely_bidi_control_characters = LinkedList::new();
    while !this.is_empty() {
        match this
            .front_mut()
            .as_mut()
            .unwrap()
            .strip_leading_whitespace_if_necessary()
        {
            WhitespaceStrippingResult::RetainFragment => break,
            WhitespaceStrippingResult::FragmentContainedOnlyBidiControlCharacters => {
                leading_fragments_consisting_of_solely_bidi_control_characters
                    .push_back(this.pop_front().unwrap())
            },
            WhitespaceStrippingResult::FragmentContainedOnlyWhitespace => {
                let removed_fragment = this.pop_front().unwrap();
                if let Some(ref mut remaining_fragment) = this.front_mut() {
                    remaining_fragment.meld_with_prev_inline_fragment(&removed_fragment);
                }
            },
        }
    }
    prepend_from(
        this,
        &mut leading_fragments_consisting_of_solely_bidi_control_characters,
    );
}

/// Strips ignorable whitespace from the end of a list of fragments.
pub fn strip_ignorable_whitespace_from_end(this: &mut LinkedList<Fragment>) {
    if this.is_empty() {
        return;
    }

    let mut trailing_fragments_consisting_of_solely_bidi_control_characters = LinkedList::new();
    while !this.is_empty() {
        match this
            .back_mut()
            .as_mut()
            .unwrap()
            .strip_trailing_whitespace_if_necessary()
        {
            WhitespaceStrippingResult::RetainFragment => break,
            WhitespaceStrippingResult::FragmentContainedOnlyBidiControlCharacters => {
                trailing_fragments_consisting_of_solely_bidi_control_characters
                    .push_front(this.pop_back().unwrap())
            },
            WhitespaceStrippingResult::FragmentContainedOnlyWhitespace => {
                let removed_fragment = this.pop_back().unwrap();
                if let Some(ref mut remaining_fragment) = this.back_mut() {
                    remaining_fragment.meld_with_next_inline_fragment(&removed_fragment);
                }
            },
        }
    }
    this.append(&mut trailing_fragments_consisting_of_solely_bidi_control_characters);
}

/// If the 'unicode-bidi' property has a value other than 'normal', return the bidi control codes
/// to inject before and after the text content of the element.
fn bidi_control_chars(style: &ServoArc<ComputedValues>) -> Option<(&'static str, &'static str)> {
    use style::computed_values::direction::T::*;
    use style::computed_values::unicode_bidi::T::*;

    let unicode_bidi = style.get_text().unicode_bidi;
    let direction = style.get_inherited_box().direction;

    // See the table in http://dev.w3.org/csswg/css-writing-modes/#unicode-bidi
    match (unicode_bidi, direction) {
        (Normal, _) => None,
        (Embed, Ltr) => Some(("\u{202A}", "\u{202C}")),
        (Embed, Rtl) => Some(("\u{202B}", "\u{202C}")),
        (Isolate, Ltr) => Some(("\u{2066}", "\u{2069}")),
        (Isolate, Rtl) => Some(("\u{2067}", "\u{2069}")),
        (BidiOverride, Ltr) => Some(("\u{202D}", "\u{202C}")),
        (BidiOverride, Rtl) => Some(("\u{202E}", "\u{202C}")),
        (IsolateOverride, Ltr) => Some(("\u{2068}\u{202D}", "\u{202C}\u{2069}")),
        (IsolateOverride, Rtl) => Some(("\u{2068}\u{202E}", "\u{202C}\u{2069}")),
        (Plaintext, _) => Some(("\u{2068}", "\u{2069}")),
    }
}

fn control_chars_to_fragment<E>(
    node: &InlineFragmentNodeInfo,
    context: &SharedStyleContext,
    text: &str,
    restyle_damage: RestyleDamage,
) -> Fragment
where
    E: TElement,
{
    let info = SpecificFragmentInfo::UnscannedText(Box::new(UnscannedTextFragmentInfo::new(
        Box::<str>::from(text),
        None,
    )));
    let text_style = context.stylist.style_for_anonymous::<E>(
        &context.guards,
        &PseudoElement::ServoLegacyText,
        &node.style,
    );

    Fragment::from_opaque_node_and_style(
        node.address,
        node.pseudo,
        text_style,
        node.selected_style.clone(),
        restyle_damage,
        info,
    )
}

/// Returns true if this node has non-zero padding or border.
fn has_padding_or_border(values: &ComputedValues) -> bool {
    let padding = values.get_padding();
    let border = values.get_border();

    use style::Zero;
    !padding.padding_top.is_definitely_zero() ||
        !padding.padding_right.is_definitely_zero() ||
        !padding.padding_bottom.is_definitely_zero() ||
        !padding.padding_left.is_definitely_zero() ||
        !border.border_top_width.is_zero() ||
        !border.border_right_width.is_zero() ||
        !border.border_bottom_width.is_zero() ||
        !border.border_left_width.is_zero()
}

/// Maintains a stack of anonymous boxes needed to ensure that the flow tree is *legal*. The tree
/// is legal if it follows the rules in CSS 2.1 § 17.2.1.
///
/// As an example, the legalizer makes sure that table row flows contain only table cells. If the
/// flow constructor attempts to place, say, a block flow directly underneath the table row, the
/// legalizer generates an anonymous table cell in between to hold the block.
///
/// Generally, the flow constructor should use `Legalizer::add_child()` instead of calling
/// `Flow::add_new_child()` directly. This ensures that the flow tree remains legal at all times
/// and centralizes the anonymous flow generation logic in one place.
struct Legalizer {
    /// A stack of anonymous flows that have yet to be finalized (i.e. that still could acquire new
    /// children).
    stack: Vec<FlowRef>,
}

impl Legalizer {
    /// Creates a new legalizer.
    fn new() -> Legalizer {
        Legalizer { stack: vec![] }
    }

    /// Makes the `child` flow a new child of `parent`. Anonymous flows are automatically inserted
    /// to keep the tree legal.
    fn add_child<E>(
        &mut self,
        context: &SharedStyleContext,
        parent: &mut FlowRef,
        mut child: FlowRef,
    ) where
        E: TElement,
    {
        while !self.stack.is_empty() {
            if self.try_to_add_child::<E>(context, parent, &mut child) {
                return;
            }
            self.flush_top_of_stack(parent)
        }

        while !self.try_to_add_child::<E>(context, parent, &mut child) {
            self.push_next_anonymous_flow::<E>(context, parent)
        }
    }

    /// Flushes all flows we've been gathering up.
    fn finish(mut self, parent: &mut FlowRef) {
        while !self.stack.is_empty() {
            self.flush_top_of_stack(parent)
        }
    }

    /// Attempts to make `child` a child of `parent`. On success, this returns true. If this would
    /// make the tree illegal, this method does nothing and returns false.
    ///
    /// This method attempts to create anonymous blocks in between `parent` and `child` if and only
    /// if those blocks will only ever have `child` as their sole child. At present, this is only
    /// true for anonymous block children of flex flows.
    fn try_to_add_child<E>(
        &mut self,
        context: &SharedStyleContext,
        parent: &mut FlowRef,
        child: &mut FlowRef,
    ) -> bool
    where
        E: TElement,
    {
        let parent = self.stack.last_mut().unwrap_or(parent);
        let (parent_class, child_class) = (parent.class(), child.class());
        match (parent_class, child_class) {
            (FlowClass::TableWrapper, FlowClass::Table) |
            (FlowClass::Table, FlowClass::TableColGroup) |
            (FlowClass::Table, FlowClass::TableRowGroup) |
            (FlowClass::Table, FlowClass::TableRow) |
            (FlowClass::Table, FlowClass::TableCaption) |
            (FlowClass::TableRowGroup, FlowClass::TableRow) |
            (FlowClass::TableRow, FlowClass::TableCell) => {
                parent.add_new_child((*child).clone());
                true
            },

            (FlowClass::TableWrapper, _) |
            (FlowClass::Table, _) |
            (FlowClass::TableRowGroup, _) |
            (FlowClass::TableRow, _) |
            (_, FlowClass::Table) |
            (_, FlowClass::TableColGroup) |
            (_, FlowClass::TableRowGroup) |
            (_, FlowClass::TableRow) |
            (_, FlowClass::TableCaption) |
            (_, FlowClass::TableCell) => false,

            (FlowClass::Flex, FlowClass::Inline) => {
                FlowRef::deref_mut(child)
                    .mut_base()
                    .flags
                    .insert(FlowFlags::MARGINS_CANNOT_COLLAPSE);
                let mut block_wrapper = Legalizer::create_anonymous_flow::<E, _>(
                    context,
                    parent,
                    &[PseudoElement::ServoLegacyAnonymousBlock],
                    SpecificFragmentInfo::Generic,
                    BlockFlow::from_fragment,
                );

                {
                    let flag = if parent.as_flex().main_mode() == Direction::Inline {
                        FragmentFlags::IS_INLINE_FLEX_ITEM
                    } else {
                        FragmentFlags::IS_BLOCK_FLEX_ITEM
                    };
                    let block = FlowRef::deref_mut(&mut block_wrapper).as_mut_block();
                    block.base.flags.insert(FlowFlags::MARGINS_CANNOT_COLLAPSE);
                    block.fragment.flags.insert(flag);
                }
                block_wrapper.add_new_child((*child).clone());
                block_wrapper.finish();
                parent.add_new_child(block_wrapper);
                true
            },

            (FlowClass::Flex, _) => {
                {
                    let flag = if parent.as_flex().main_mode() == Direction::Inline {
                        FragmentFlags::IS_INLINE_FLEX_ITEM
                    } else {
                        FragmentFlags::IS_BLOCK_FLEX_ITEM
                    };
                    let block = FlowRef::deref_mut(child).as_mut_block();
                    block.base.flags.insert(FlowFlags::MARGINS_CANNOT_COLLAPSE);
                    block.fragment.flags.insert(flag);
                }
                parent.add_new_child((*child).clone());
                true
            },

            _ => {
                parent.add_new_child((*child).clone());
                true
            },
        }
    }

    /// Finalizes the flow on the top of the stack.
    fn flush_top_of_stack(&mut self, parent: &mut FlowRef) {
        let mut child = self.stack.pop().expect("flush_top_of_stack(): stack empty");
        child.finish();
        self.stack.last_mut().unwrap_or(parent).add_new_child(child)
    }

    /// Adds the anonymous flow that would be necessary to make an illegal child of `parent` legal
    /// to the stack.
    fn push_next_anonymous_flow<E>(&mut self, context: &SharedStyleContext, parent: &FlowRef)
    where
        E: TElement,
    {
        let parent_class = self.stack.last().unwrap_or(parent).class();
        match parent_class {
            FlowClass::TableRow => self.push_new_anonymous_flow::<E, _>(
                context,
                parent,
                &[PseudoElement::ServoAnonymousTableCell],
                SpecificFragmentInfo::TableCell,
                TableCellFlow::from_fragment,
            ),
            FlowClass::Table | FlowClass::TableRowGroup => self.push_new_anonymous_flow::<E, _>(
                context,
                parent,
                &[PseudoElement::ServoAnonymousTableRow],
                SpecificFragmentInfo::TableRow,
                TableRowFlow::from_fragment,
            ),
            FlowClass::TableWrapper => self.push_new_anonymous_flow::<E, _>(
                context,
                parent,
                &[PseudoElement::ServoLegacyAnonymousTable],
                SpecificFragmentInfo::Table,
                TableFlow::from_fragment,
            ),
            _ => self.push_new_anonymous_flow::<E, _>(
                context,
                parent,
                &[
                    PseudoElement::ServoLegacyTableWrapper,
                    PseudoElement::ServoLegacyAnonymousTableWrapper,
                ],
                SpecificFragmentInfo::TableWrapper,
                TableWrapperFlow::from_fragment,
            ),
        }
    }

    /// Creates an anonymous flow and pushes it onto the stack.
    fn push_new_anonymous_flow<E, F>(
        &mut self,
        context: &SharedStyleContext,
        reference: &FlowRef,
        pseudos: &[PseudoElement],
        specific_fragment_info: SpecificFragmentInfo,
        constructor: fn(Fragment) -> F,
    ) where
        E: TElement,
        F: Flow,
    {
        let new_flow = Self::create_anonymous_flow::<E, _>(
            context,
            reference,
            pseudos,
            specific_fragment_info,
            constructor,
        );
        self.stack.push(new_flow)
    }

    /// Creates a new anonymous flow. The new flow is identical to `reference` except with all
    /// styles applying to every pseudo-element in `pseudos` applied.
    ///
    /// This method invokes the supplied constructor function on the given specific fragment info
    /// in order to actually generate the flow.
    fn create_anonymous_flow<E, F>(
        context: &SharedStyleContext,
        reference: &FlowRef,
        pseudos: &[PseudoElement],
        specific_fragment_info: SpecificFragmentInfo,
        constructor: fn(Fragment) -> F,
    ) -> FlowRef
    where
        E: TElement,
        F: Flow,
    {
        let reference_block = reference.as_block();
        let mut new_style = reference_block.fragment.style.clone();
        for pseudo in pseudos {
            new_style =
                context
                    .stylist
                    .style_for_anonymous::<E>(&context.guards, pseudo, &new_style);
        }
        let fragment = reference_block
            .fragment
            .create_similar_anonymous_fragment(new_style, specific_fragment_info);
        FlowRef::new(Arc::new(constructor(fragment)))
    }
}

pub fn is_image_data(uri: &str) -> bool {
    static TYPES: &[&str] = &["data:image/png", "data:image/gif", "data:image/jpeg"];
    TYPES.iter().any(|&type_| uri.starts_with(type_))
}
