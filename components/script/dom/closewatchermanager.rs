/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use dom_struct::dom_struct;
use script_bindings::script_runtime::CanGc;
use crate::dom::bindings::reflector::{reflect_dom_object};
use crate::dom::bindings::root::{DomRoot};
use crate::dom::closewatcher::CloseWatcher;
use crate::dom::types::Window;

#[dom_struct]
pub struct CloseWatcherManager {
    groups: Vec<Vec<DomRoot<CloseWatcher>>>,
    allowed_number_of_groups: usize,
    next_user_interaction_allows_a_new_group: bool
}

impl CloseWatcherManager {
    pub fn new_inherited() -> Self {
        Self {
            groups: vec![],
            allowed_number_of_groups: 1,
            next_user_interaction_allows_a_new_group: true
        }
    }

    pub fn new(window: &Window, can_gc: CanGc) -> DomRoot<Self> {
        reflect_dom_object(Box::new(Self::new_inherited()), window, can_gc)
    }

    pub fn add(&mut self, close_watcher: &DomRoot<CloseWatcher>) {
        // If manager's groups's size is less than manager's allowed number of groups
        if self.groups.borrow().len() < self.allowed_number_of_groups {
            // then append « closeWatcher » to manager's groups.
            let mut new_group = Vec::new();
            new_group.push(&close_watcher);
            self.groups.borrow_mut().push(new_group);
        } else {
            // Assert: manager's groups's size is at least 1 in this branch, since manager's allowed number of groups is always at least 1.
            assert!(!self.groups.borrow().is_empty());
            // Append closeWatcher to manager's groups's last item.
            self.groups.borrow_mut().last_mut().unwrap().push(close_watcher);
        }
        // Set manager's next user interaction allows a new group to true.
        self.next_user_interaction_allows_a_new_group = true;
    }

    pub fn remove(&mut self, close_watcher: &CloseWatcher) {
        // 2. For each group of manager's groups: remove closeWatcher from group
        for mut group in &self.groups {
            if let Some(index) = group.iter().position(|entry| **entry == *close_watcher) {
                group.remove(index);
            }
        }
        // 3. Remove any item from manager's group that is empty
        self.groups.borrow_mut().retain(|group| !group.is_empty());
    }

    /// https://html.spec.whatwg.org/multipage/interaction.html#process-close-watchers
    pub fn process_close_watchers(&mut self) -> bool {
        // 1. Let processedACloseWatcher be false.
        let mut processed_a_close_watcher = false;
        // 2. If window's close watcher manager's groups is not empty:
        if !self.groups.borrow().is_empty() {
            // 1. Let group be the last item in window's close watcher manager's groups.
            let group = self.groups.borrow_mut().last_mut().unwrap();
            // 2. For each closeWatcher of group, in reverse order:
            for close_watcher in group.iter().rev() {
                // 1. if the result of running closeWatcher's get enabled state is true, set processedACloseWatcher to true.
                if close_watcher.get_enabled_state() { processed_a_close_watcher = true; }
                // 2. Let shouldProceed be the result of requesting to close closeWatcher.
                let should_proceed = close_watcher.request_to_close(true);
                // 3. If shouldProceed is false, then break.
                if !should_proceed { break; }
            }
        }
        // 3. If window's close watcher manager's allowed number of groups is greater than 1, decrement it by 1.
        if self.allowed_number_of_groups > 1 {
            self.allowed_number_of_groups -= 1;
        }
        // 4. Return processedACloseWatcher.
        processed_a_close_watcher
    }

    // https://html.spec.whatwg.org/multipage/interaction.html#notify-the-close-watcher-manager-about-user-activation
    pub fn notify_about_user_activation(&mut self) {
        // 1. Let manager be window's close watcher manager.
        // 2. If manager's next user interaction allows a new group is true, then increment manager's allowed number of groups.
        if self.next_user_interaction_allows_a_new_group {
            self.allowed_number_of_groups += 1;
        }
        // 3. Set manager's next user interaction allows a new group to false.
        self.next_user_interaction_allows_a_new_group = false;
    }

    pub fn can_prevent_close(&self) -> bool {
        // 5. Let canPreventClose be true if window's close watcher manager's groups's size is less than window's close watcher manager's allowed number of groups...
        self.groups.borrow().len() < self.allowed_number_of_groups
    }
}
