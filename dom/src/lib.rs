//! Tools for declaratively constructing and incrementally updating HTML DOM trees on the web. Based
//! on the [`moxie`] UI runtime.

#![warn(missing_docs, intra_doc_link_resolution_failure)]

#[doc(hidden)]
pub use moxie::*;

use {
    crate::{
        embed::WebRuntime,
        event::{Event, EventHandle},
    },
    moxie,
    std::cell::Cell,
    tracing::*,
};

pub mod elements;
pub mod embed;
pub mod event;

pub use web_sys as sys;

/// The "boot sequence" for a moxie-dom instance creates a [crate::embed::WebRuntime] with the
/// provided arguments and begins scheduling its execution with `requestAnimationFrame` on state
/// changes.
///
/// If you need to schedule your root function more or less frequently than when state variables are
/// updated, see the [embed](crate::embed) module for granular control over scheduling.
///
/// In terms of the embed module's APIs, this function constructs a new
/// [`WebRuntime`](crate::embed::WebRuntime) and begins scheduling it with an
/// [`AnimationFrameScheduler`](crate::embed::AnimationFrameScheduler) which requests an animation
/// frame only when there are updates to state variables.
pub fn boot(new_parent: impl AsRef<sys::Element> + 'static, root: impl FnMut() + 'static) {
    WebRuntime::new(new_parent.as_ref().to_owned(), root)
        .animation_frame_scheduler()
        .run_on_state_changes();
}

/// Returns the current window. Panics if no window is available.
pub fn window() -> sys::Window {
    sys::window().expect("must run from within a `window`")
}

/// Returns the current document. Panics if called outside a web document context.
pub fn document() -> sys::Document {
    window()
        .document()
        .expect("must run from within a `window` with a valid `document`")
}

/// Create and mount a [DOM text node](https://developer.mozilla.org/en-US/docs/Web/API/Text).
/// This is normally called by the [`moxie::mox!`] macro.
#[topo::aware]
#[topo::from_env(parent: MemoElement)]
pub fn text(s: impl ToString) {
    // TODO consider a ToOwned-based memoization API that's lower level?
    // memo_ref<Ref, Arg, Output>(reference: Ref, init: impl FnOnce(Arg) -> Output)
    // where Ref: ToOwned<Owned=Arg> + PartialEq, etcetcetc
    let text_node = memo!(s.to_string(), |s| document().create_text_node(s));
    parent.ensure_child_attached(&text_node);
}

/// Create and mount an [HTML element](https://developer.mozilla.org/en-US/docs/Web/API/Element).
/// Called from the individual element macros, which in turn are normally called by the output of
/// the [`moxie::mox!`] macro.
///
/// The created DOM node is memoized at the bound callsite, allowing for subsequent re-executions to
/// be very cheap.
///
/// Mutation of the created element is performed during the `with_elem` closure via the provided
/// [`moxie_dom::MemoElement`] wrapper.
#[topo::aware]
#[topo::from_env(parent: MemoElement)]
pub fn element<ChildRet>(
    ty: &'static str,
    with_elem: impl FnOnce(&MemoElement) -> ChildRet,
) -> ChildRet {
    let elem = memo!(ty, |ty| document().create_element(ty).unwrap());
    parent.ensure_child_attached(&elem);
    let elem = MemoElement::new(elem);
    with_elem(&elem)
}

/// A topologically-aware "incremental smart pointer" for an HTML element.
///
/// Created during execution of the (element) macro and the element-specific wrappers. Offers a
/// "stringly-typed" API for mutating the contained DOM nodes, adhering fairly closely to the
/// upstream web specs.
pub struct MemoElement {
    curr: Cell<Option<sys::Node>>,
    elem: sys::Element,
}

impl MemoElement {
    fn new(elem: sys::Element) -> Self {
        Self {
            curr: Cell::new(None),
            elem,
        }
    }

    /// Retrieves access to the raw HTML element underlying the (MemoElement).
    ///
    /// Because this offers an escape hatch around the memoized mutations, it should be used with
    /// caution. Also because of this, it has a silly name intended to loudly announce that
    /// care must be taken.
    ///
    /// Code called by the root function of your application will be run quite frequently and
    /// so the tools for memoization are important for keeping your application responsive. If you
    /// have legitimate needs for this API, please consider filing an issue with your use case so
    /// the maintainers of this crate can consider "official" ways to support it.
    pub fn raw_element_that_has_sharp_edges_please_be_careful(&self) -> sys::Element {
        self.elem.clone()
    }

    // FIXME this should be topo-aware
    // TODO and it should be able to express its slot as an annotation
    /// Declare an attribute of the element, mutating the actual element's attribute when the passed
    /// value changes.
    ///
    /// A guard value is stored as a resulting "effect" of the mutation, and removes the attribute
    /// when `drop`ped, to ensure that the attribute is removed when this declaration is no longer
    /// referenced in the most recent (`moxie::Revision`).
    pub fn attr(&self, name: &'static str, value: impl ToString) -> &Self {
        topo::call!(slot: name, {
            memo_with!(
                value.to_string(),
                |v| {
                    self.elem.set_attribute(name, v).unwrap();
                    scopeguard::guard(self.elem.clone(), move |elem| {
                        elem.remove_attribute(name).unwrap()
                    })
                },
                |_| {}
            )
        });
        self
    }

    // FIXME this should be topo-aware
    /// Declare an event handler on the element.
    ///
    /// A guard value is stored as a resulting "effect" of the mutation, and removes the attribute
    /// when `drop`ped, to ensure that the attribute is removed when this declaration is no longer
    /// referenced in the most recent (`moxie::Revision`).
    ///
    /// Currently this is performed on every Revision, as changes to event handlers don't typically
    /// affect the debugging experience and have not yet shown up in performance profiles.
    pub fn on<Ev>(&self, callback: impl FnMut(Ev) + 'static) -> &Self
    where
        Ev: 'static + Event,
    {
        topo::call!(slot: Ev::NAME, {
            memo_with!(
                moxie::embed::Revision::current(),
                |_| {
                    let target: &sys::EventTarget = self.elem.as_ref();
                    EventHandle::new(target.clone(), callback)
                },
                |_| {}
            );
        });
        self
    }

    fn ensure_child_attached(&self, new_child: &sys::Node) {
        let prev_sibling = self.curr.replace(Some(new_child.clone()));

        let existing = if prev_sibling.is_none() {
            self.elem.first_child()
        } else {
            prev_sibling.and_then(|p| p.next_sibling())
        };

        if let Some(existing) = existing {
            if !existing.is_same_node(Some(new_child)) {
                self.elem.replace_child(new_child, &existing).unwrap();
            }
        } else {
            self.elem.append_child(new_child).unwrap();
        }
    }

    /// Declare the inner contents of the element, usually declaring children within the inner
    /// scope. After any children have been run and their nodes attached, this clears any trailing
    /// child nodes to ensure the element's children are correct per the latest declaration.
    // FIXME this should be topo-aware
    pub fn inner<Ret>(&self, children: impl FnOnce() -> Ret) -> Ret {
        let elem = self.elem.clone();
        let last_desired_child;
        let ret;
        topo::call!(
            {
                ret = children();

                // before this melement is dropped when the environment goes out of scope,
                // we need to get the last recorded child from this revision
                last_desired_child = topo::Env::expect::<MemoElement>().curr.replace(None);
            },
            env! {
                MemoElement => MemoElement::new(self.elem.clone()),
            }
        );

        // if there weren't any children declared this revision, we need to make sure we clean up
        // any from the last revision
        let mut next_to_remove = if let Some(c) = last_desired_child {
            c.next_sibling()
        } else {
            elem.first_child()
        };

        while let Some(to_remove) = next_to_remove {
            next_to_remove = to_remove.next_sibling();
            elem.remove_child(&to_remove).unwrap();
        }

        ret
    }
}
