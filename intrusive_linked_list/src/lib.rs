//! Intrusive linked list implementation for memory-efficient collections.
//!
//! This crate provides a simple intrusive linked list. APIs that insert/remove
//! nodes use node addresses as `usize`, while link fields are stored internally
//! as `Option<NonNull<IntrusiveLinkedList>>`.

#![cfg_attr(not(test), no_std)]

use core::fmt;
use core::ptr::NonNull;

/// An intrusive linked list node that can be embedded in other structures.
///
/// Each node contains an optional pointer to the next node in the list.
/// The list does not own the nodes; callers are responsible for ensuring
/// node memory remains valid while in the list.
#[repr(C)]
#[derive(Default)]
pub struct IntrusiveLinkedList {
    /// Pointer to the next node in the list, or `None` if this is the tail.
    next: Option<NonNull<IntrusiveLinkedList>>,
}

impl fmt::Debug for IntrusiveLinkedList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        let mut current = self.next;
        while let Some(node) = current {
            list.entry(&node);
            // SAFETY: We only traverse nodes that were previously added to the list,
            // so they must be valid.
            current = unsafe { node.as_ref().next };
        }
        list.finish()
    }
}

impl IntrusiveLinkedList {
    /// Creates a new empty intrusive linked list.
    #[must_use]
    pub const fn new() -> Self {
        Self { next: None }
    }

    /// Returns `true` if the list contains no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.next.is_none()
    }

    /// Pushes a node at the front of the list.
    ///
    /// # Safety
    /// - `ptr` must point to a valid, properly aligned `IntrusiveLinkedList`.
    /// - The memory at `ptr` must remain valid for the lifetime of its membership in the list.
    /// - The caller must ensure no aliasing violations occur.
    pub unsafe fn push(&mut self, ptr: usize) {
        // SAFETY: Caller guarantees ptr is valid.
        let mut node = unsafe { NonNull::new_unchecked(ptr as *mut IntrusiveLinkedList) };
        // SAFETY: We have exclusive access via &mut self.
        unsafe { node.as_mut() }.next = self.next.take();
        self.next = Some(node);
    }

    /// Pushes a node at the back of the list.
    ///
    /// # Safety
    /// - `ptr` must point to a valid, properly aligned `IntrusiveLinkedList`.
    /// - The memory at `ptr` must remain valid for the lifetime of its membership in the list.
    /// - The caller must ensure no aliasing violations occur.
    pub unsafe fn push_back(&mut self, ptr: usize) {
        // SAFETY: Caller guarantees ptr is valid.
        let mut new_node = unsafe { NonNull::new_unchecked(ptr as *mut IntrusiveLinkedList) };
        // SAFETY: We have exclusive access to the new node.
        unsafe { new_node.as_mut().next = None };

        let mut last_node = &mut self.next;
        while let Some(node) = last_node {
            // SAFETY: All nodes in the list are valid by invariant.
            last_node = unsafe { &mut node.as_mut().next };
        }
        *last_node = Some(new_node);
    }

    /// Removes and returns the first node from the list.
    ///
    /// Returns `None` if the list is empty.
    #[must_use]
    pub fn pop(&mut self) -> Option<usize> {
        self.next.map(|node| {
            // SAFETY: node was in the list, so it must be valid.
            self.next = unsafe { node.as_ref().next };
            node.as_ptr() as usize
        })
    }

    /// Removes a node with the given address from the list.
    ///
    /// Returns `true` if the node was found and removed, `false` otherwise.
    pub fn remove_if(&mut self, ptr: usize) -> bool {
        let mut link = &mut self.next;
        while let Some(mut node) = *link {
            if node.as_ptr() as usize == ptr {
                // SAFETY: The current link points to a valid list node.
                *link = unsafe { node.as_ref().next };
                return true;
            }
            // SAFETY: The current link points to a valid list node.
            link = unsafe { &mut node.as_mut().next };
        }
        false
    }

    /// Inserts a node in sorted order by address (ascending).
    ///
    /// # Safety
    /// - `ptr` must point to a valid, properly aligned `IntrusiveLinkedList`.
    /// - The memory at `ptr` must remain valid for the lifetime of its membership in the list.
    /// - The caller must ensure no aliasing violations occur.
    ///
    /// # Panics
    /// Panics if the list is non-empty and the head node is unexpectedly `None`
    /// after the initial check (should not happen in correct usage).
    pub unsafe fn add_with_sort(&mut self, ptr: usize) {
        // SAFETY: Caller guarantees ptr is valid.
        let mut new_node = unsafe { NonNull::new_unchecked(ptr as *mut IntrusiveLinkedList) };

        if self.next.is_none() || self.next.expect("checked above").as_ptr() as usize > ptr {
            // SAFETY: new_node is valid.
            unsafe { new_node.as_mut() }.next = self.next.take();
            self.next = Some(new_node);
            return;
        }

        let mut prev = self.next.expect("checked above");
        // SAFETY: prev is valid as it is in the list.
        while let Some(current) = unsafe { prev.as_ref().next } {
            if current.as_ptr() as usize > ptr {
                break;
            }
            prev = current;
        }

        // SAFETY: Both prev and new_node are valid.
        unsafe { new_node.as_mut() }.next = unsafe { prev.as_mut().next.take() };
        unsafe { prev.as_mut() }.next = Some(new_node);
    }

    /// Returns the number of elements in the list.
    #[must_use]
    pub fn len(&self) -> usize {
        let mut count = 0;
        let mut current = self.next;
        while let Some(node) = current {
            // SAFETY: All nodes in the list are valid by invariant.
            current = unsafe { node.as_ref().next };
            count += 1;
        }
        count
    }

    /// Returns the next node pointer, if any.
    #[must_use]
    pub fn next(&self) -> Option<NonNull<IntrusiveLinkedList>> {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_push_and_pop() {
        let mut list = IntrusiveLinkedList::new();
        assert!(list.is_empty());
        assert_eq!(list.pop(), None);

        let mut node1 = IntrusiveLinkedList { next: None };
        let ptr1 = &mut node1 as *mut _ as usize;

        let mut node2 = IntrusiveLinkedList { next: None };
        let ptr2 = &mut node2 as *mut _ as usize;

        unsafe {
            list.push(ptr1);
            list.push(ptr2);
        }

        assert!(!list.is_empty());
        assert_eq!(list.len(), 2);

        assert_eq!(list.pop(), Some(ptr2));
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), None);
        assert!(list.is_empty());
    }

    #[test]
    fn test_push_back() {
        let mut list = IntrusiveLinkedList::new();

        let mut node1 = IntrusiveLinkedList { next: None };
        let ptr1 = &mut node1 as *mut _ as usize;
        let mut node2 = IntrusiveLinkedList { next: None };
        let ptr2 = &mut node2 as *mut _ as usize;
        let mut node3 = IntrusiveLinkedList { next: None };
        let ptr3 = &mut node3 as *mut _ as usize;

        // Push back to empty list
        unsafe {
            list.push_back(ptr1);
        }
        assert_eq!(list.len(), 1);
        assert_eq!(list.pop(), Some(ptr1));
        assert!(list.is_empty());

        // Push back to non-empty list
        unsafe {
            list.push_back(ptr1);
            list.push_back(ptr2);
            list.push_back(ptr3);
        }

        assert_eq!(list.len(), 3);
        // The list should be ptr1 -> ptr2 -> ptr3
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), Some(ptr2));
        assert_eq!(list.pop(), Some(ptr3));
        assert!(list.is_empty());
    }

    #[test]
    fn test_remove_if() {
        let mut list = IntrusiveLinkedList::new();

        let mut node1 = IntrusiveLinkedList { next: None };
        let ptr1 = &mut node1 as *mut _ as usize;
        let mut node2 = IntrusiveLinkedList { next: None };
        let ptr2 = &mut node2 as *mut _ as usize;
        let mut node3 = IntrusiveLinkedList { next: None };
        let ptr3 = &mut node3 as *mut _ as usize;

        unsafe {
            list.push(ptr3);
            list.push(ptr2);
            list.push(ptr1);
        }

        // Remove head
        assert!(list.remove_if(ptr1));

        assert_eq!(list.len(), 2);
        assert_eq!(list.pop(), Some(ptr2));
        assert_eq!(list.pop(), Some(ptr3));

        // Reset list
        unsafe {
            list.push(ptr3);
            list.push(ptr2);
            list.push(ptr1);
        }

        // Remove middle
        assert!(list.remove_if(ptr2));

        assert_eq!(list.len(), 2);
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), Some(ptr3));

        // Reset list
        unsafe {
            list.push(ptr3);
            list.push(ptr2);
            list.push(ptr1);
        }

        // Remove tail
        assert!(list.remove_if(ptr3));

        assert_eq!(list.len(), 2);
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), Some(ptr2));

        // Remove non-existent
        assert!(!list.remove_if(0xdeadbeef));
    }

    #[test]
    fn test_add_with_sort() {
        let mut list = IntrusiveLinkedList::new();

        let mut nodes = [
            IntrusiveLinkedList { next: None },
            IntrusiveLinkedList { next: None },
            IntrusiveLinkedList { next: None },
            IntrusiveLinkedList { next: None },
        ];
        let ptrs: Vec<usize> = nodes.iter_mut().map(|n| n as *mut _ as usize).collect();

        // ptrs are sorted by address
        let ptr1 = ptrs[0];
        let ptr2 = ptrs[1];
        let ptr3 = ptrs[2];
        let ptr4 = ptrs[3];

        // Add to empty list
        unsafe { list.add_with_sort(ptr2) };
        assert_eq!(list.len(), 1);
        assert_eq!(list.next().unwrap().as_ptr() as usize, ptr2);

        // Add smaller to head
        unsafe { list.add_with_sort(ptr1) };
        assert_eq!(list.len(), 2);
        assert_eq!(list.next().unwrap().as_ptr() as usize, ptr1);

        // Add to end
        unsafe { list.add_with_sort(ptr4) };
        assert_eq!(list.len(), 3);

        // Add to middle
        unsafe { list.add_with_sort(ptr3) };
        assert_eq!(list.len(), 4);

        // Check final order
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), Some(ptr2));
        assert_eq!(list.pop(), Some(ptr3));
        assert_eq!(list.pop(), Some(ptr4));
        assert!(list.is_empty());
    }
}
