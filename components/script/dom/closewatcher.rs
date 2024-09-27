/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use js::rust::HandleObject;

use dom_struct::dom_struct;

use crate::dom::bindings::codegen::Bindings::CloseWatcherBinding::{
    CloseWatcherMethods, CloseWatcherOptions,
};
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::{Error, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::{gitreflect_dom_object_with_proto};
use crate::dom::bindings::root::DomRoot;
use crate::dom::event::{Event, EventBubbles, EventCancelable, EventStatus};
use crate::dom::eventtarget::EventTarget;
use crate::dom::types::Window;
use crate::script_runtime::CanGc;
use crate::test::Dom;

#[dom_struct]
pub struct CloseWatcher {
    eventtarget: EventTarget,
    is_active: Cell<bool>,
    is_running_cancel_action: Cell<bool>,
    window: Dom<Window>
}

impl CloseWatcher {
    #[allow(non_snake_case)]
    pub fn Constructor(
        window: &Window,
        proto: Option<HandleObject>,
        can_gc: CanGc,
        _: &CloseWatcherOptions,
    ) -> Fallible<DomRoot<CloseWatcher>> {
        // 1. If this's relevant global object's associated Document is not fully active, then throw an "InvalidStateError" DOMException.
        if !window.Document().is_fully_active() {
            return Err(Error::InvalidState);
        }
        // 2. Let closeWatcher be the result of establishing a close watcher given this's relevant global object
        let close_watcher = CloseWatcher::establish(window, proto, can_gc);
        // 3. If options["signal"] exists, then:
            // 1. If options["signal"] is aborted, then destroy closeWatcher.
            // 2. Add the following steps to options["signal"]:
                // 1. Destroy closeWatcher.
        // TODO: Implement AbortSignal handling
        // 4. Set this's internal close watcher to closeWatcher.
        return Ok(close_watcher);
    }

    pub fn new(
        window: &Window,
        proto: Option<HandleObject>,
        can_gc: CanGc,
    ) -> DomRoot<CloseWatcher> {
        let close_watcher = reflect_dom_object_with_proto(
            Box::new(CloseWatcher::new_inherited(window)),
            window,
            proto,
            can_gc,
        );

        close_watcher
    }
    pub fn new_inherited(
        window: &Window,
    ) -> CloseWatcher {
        let close_watcher = CloseWatcher {
            eventtarget: EventTarget::new_inherited(),
            is_active: Default::default(),
            is_running_cancel_action: Default::default(),
            window: Dom::from_ref(window),
        };
        close_watcher.is_active.set(true);
        return close_watcher
    }

    // https://html.spec.whatwg.org/multipage/interaction.html#establish-a-close-watcher
    pub fn establish(
        window: &Window,
        proto: Option<HandleObject>,
        can_gc: CanGc) -> DomRoot<CloseWatcher> {
        // 1. Assert: window's associated Document is fully active.
        assert!(window.Document().is_fully_active());
        // 2. Let closeWatcher be a new close watcher
        let close_watcher = CloseWatcher::new(&window, proto, can_gc);
        // 3. Let manager be window's close watcher manager.
        // 4-6. Moved to CloseWatcherManager::add
        window.close_watcher_manager().add(close_watcher.borrow());
        // 7. Return closeWatcher.
        return close_watcher
    }

    pub fn request_to_close(&self) -> bool {
        // 1. If closeWatcher is not active, then return true.
        if !self.is_active.get() { return false; }
        // 2. If closeWatcher's is running cancel action is true, then return true.
        if self.is_running_cancel_action.get() { return true; }
        // 3. Let window be closeWatcher's window.
        // 4. If window's associated Document is not fully active, then return true.
        if !self.window.Document().is_fully_active() { return true; }
        // 5. Let canPreventClose be true if window's close watcher manager's groups's size is less than window's close watcher manager's allowed number of groups, and window has history-action activation; otherwise false.
        let can_prevent_close = self.window.close_watcher_manager().can_prevent_close();
        // 6. Set closeWatcher's is running cancel action to true.
        self.is_running_cancel_action.set(true);
        // 7. Let shouldContinue be the result of running closeWatcher's cancel action given canPreventClose.
        let cancel_event = Event::new(self.window.upcast(), atom!("cancel"), EventBubbles::DoesNotBubble, if can_prevent_close { EventCancelable::Cancelable } else { EventCancelable::NotCancelable });
        let should_continue = cancel_event.fire(self.eventtarget.upcast()) == EventStatus::NotCanceled;
        // 8. Set closeWatcher's is running cancel action to false.
        self.is_running_cancel_action.set(false);
        // 9. If shouldContinue is false, then:
        if !should_continue {
            // 1. Assert: canPreventClose is true.
            assert!(can_prevent_close);
            // 2. Consume history-action user activation given window.
            // TODO
            // 3. Return false.
            return false;

        }
        // 10. Close closeWatcher.
        self.Close();
        // 11. Return true.
        return true
    }
}

#[allow(non_snake_case)]
impl CloseWatcherMethods for CloseWatcher {
    // https://html.spec.whatwg.org/multipage/interaction.html#dom-closewatcher-requestclose
    fn RequestClose(&self) {
        Self::request_to_close(self);
    }

    // https://html.spec.whatwg.org/multipage/interaction.html#dom-closewatcher-close
    fn Close(&self) {
        // 1. If closeWatcher is not active, then return.
        if !self.is_active.get() { return; }
        // 2. If closeWatcher's window's associated Document is not fully active, then return.
        if !self.window.Document().is_fully_active() { return; }
        // 3. Destroy closeWatcher.
        self.Destroy();
        // 4. Run closeWatcher's close action.
        let close_event = Event::new(self.window.upcast(), atom!("close"), EventBubbles::DoesNotBubble, EventCancelable::NotCancelable);
        close_event.fire(self.eventtarget.upcast());
    }

    // https://html.spec.whatwg.org/multipage/interaction.html#dom-closewatcher-destroy
    fn Destroy(&self) {
        // 1. Let manager be closeWatcher's window's close watcher manager.
        // 2. For each group of manager's groups: remove closeWatcher from group.
        // 3. Remove any item from manager's groups that is empty.
        self.window.close_watcher_manager().remove(self);
        self.is_active.set(false);
    }

    event_handler!(cancel, GetOncancel, SetOncancel);

    event_handler!(close, GetOnclose, SetOnclose);
}
