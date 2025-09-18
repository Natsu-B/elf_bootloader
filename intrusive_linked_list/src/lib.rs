#![cfg_attr(not(test), no_std)]

use core::fmt;
use core::ptr::NonNull;

#[repr(C)]
pub struct IntrusiveLinkedList {
    next: Option<NonNull<IntrusiveLinkedList>>,
}

impl fmt::Debug for IntrusiveLinkedList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        let mut current = self.next;
        while let Some(intrusive_linked_list) = current {
            list.entry(&intrusive_linked_list);
            current = unsafe { intrusive_linked_list.as_ref().next };
        }
        list.finish()
    }
}

impl IntrusiveLinkedList {
    pub const fn new() -> Self {
        Self { next: None }
    }

    pub fn is_none(&self) -> bool {
        self.next.is_none()
    }

    pub unsafe fn push(&mut self, ptr: usize) {
        let mut intrusive_linked_list =
            unsafe { NonNull::new_unchecked(ptr as *mut IntrusiveLinkedList) };
        unsafe { intrusive_linked_list.as_mut() }.next = self.next.take();
        self.next = Some(intrusive_linked_list);
    }

    pub unsafe fn push_back(&mut self, ptr: usize) {
        let mut new_node = unsafe { NonNull::new_unchecked(ptr as *mut IntrusiveLinkedList) };
        unsafe { new_node.as_mut().next = None };

        let mut last_node = &mut self.next;
        while let Some(node) = last_node {
            last_node = unsafe { &mut node.as_mut().next };
        }
        *last_node = Some(new_node);
    }

    pub fn pop(&mut self) -> Option<usize> {
        self.next.map(|intrusive_linked_list| {
            self.next = unsafe { intrusive_linked_list.as_ref().next };
            intrusive_linked_list.as_ptr() as usize
        })
    }

    pub fn remove_if(&mut self, ptr: usize) -> bool {
        let mut intrusive_linked_list_ptr = self.next;
        if let Some(intrusive_linked_list) = intrusive_linked_list_ptr
            && intrusive_linked_list.as_ptr() as usize == ptr
        {
            self.next = unsafe { intrusive_linked_list.as_ref().next };
            return true;
        }
        while let Some(mut intrusive_linked_list) = intrusive_linked_list_ptr {
            if let Some(intrusive_linked_list_next) = unsafe { intrusive_linked_list.as_mut().next }
                && intrusive_linked_list_next.as_ptr() as usize == ptr
            {
                unsafe { intrusive_linked_list.as_mut() }.next =
                    unsafe { intrusive_linked_list_next.as_ref().next };
                return true;
            }
            intrusive_linked_list_ptr = unsafe { intrusive_linked_list.as_mut().next };
        }
        false
    }

    pub unsafe fn add_with_sort(&mut self, ptr: usize) {
        let mut new_intrusive_linked_list =
            unsafe { NonNull::new_unchecked(ptr as *mut IntrusiveLinkedList) };

        if self.next.is_none() || self.next.unwrap().as_ptr() as usize > ptr {
            unsafe { new_intrusive_linked_list.as_mut() }.next = self.next.take();
            self.next = Some(new_intrusive_linked_list);
            return;
        }

        let mut prev = self.next.unwrap();
        while let Some(current) = unsafe { prev.as_ref().next } {
            if current.as_ptr() as usize > ptr {
                break;
            }
            prev = current;
        }

        unsafe { new_intrusive_linked_list.as_mut() }.next = unsafe { prev.as_mut().next.take() };
        unsafe { prev.as_mut() }.next = Some(new_intrusive_linked_list);
    }

    pub fn size(&self) -> usize {
        let mut counter = 0;
        let mut intrusive_linked_list = self.next;
        while let Some(tmp) = intrusive_linked_list {
            intrusive_linked_list = unsafe { tmp.as_ref().next };
            counter += 1;
        }
        counter
    }

    pub fn get_next(&self) -> Option<NonNull<IntrusiveLinkedList>> {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use core::panic;
    use super::*;

    #[test]
    fn test_push_and_pop() {
        let mut list = IntrusiveLinkedList::new();
        assert!(list.is_none());
        assert_eq!(list.pop(), None);

        let mut intrusive_linked_list1 = IntrusiveLinkedList { next: None };
        let ptr1 = &mut intrusive_linked_list1 as *mut _ as usize;

        let mut intrusive_linked_list2 = IntrusiveLinkedList { next: None };
        let ptr2 = &mut intrusive_linked_list2 as *mut _ as usize;

        unsafe {
            list.push(ptr1);
            list.push(ptr2);
        }

        assert!(!list.is_none());
        assert_eq!(list.size(), 2);

        assert_eq!(list.pop(), Some(ptr2));
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), None);
        assert!(list.is_none());
    }

    #[test]
    fn test_push_back() {
        let mut list = IntrusiveLinkedList::new();

        let mut intrusive_linked_list1 = IntrusiveLinkedList { next: None };
        let ptr1 = &mut intrusive_linked_list1 as *mut _ as usize;
        let mut intrusive_linked_list2 = IntrusiveLinkedList { next: None };
        let ptr2 = &mut intrusive_linked_list2 as *mut _ as usize;
        let mut intrusive_linked_list3 = IntrusiveLinkedList { next: None };
        let ptr3 = &mut intrusive_linked_list3 as *mut _ as usize;

        // Push back to empty list
        unsafe {
            list.push_back(ptr1);
        }
        assert_eq!(list.size(), 1);
        assert_eq!(list.pop(), Some(ptr1));
        assert!(list.is_none());

        // Push back to non-empty list
        unsafe {
            list.push_back(ptr1);
            list.push_back(ptr2);
            list.push_back(ptr3);
        }

        assert_eq!(list.size(), 3);
        // The list should be ptr1 -> ptr2 -> ptr3
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), Some(ptr2));
        assert_eq!(list.pop(), Some(ptr3));
        assert!(list.is_none());
    }

    #[test]
    fn test_remove_if() {
        let mut list = IntrusiveLinkedList::new();

        let mut intrusive_linked_list1 = IntrusiveLinkedList { next: None };
        let ptr1 = &mut intrusive_linked_list1 as *mut _ as usize;
        let mut intrusive_linked_list2 = IntrusiveLinkedList { next: None };
        let ptr2 = &mut intrusive_linked_list2 as *mut _ as usize;
        let mut intrusive_linked_list3 = IntrusiveLinkedList { next: None };
        let ptr3 = &mut intrusive_linked_list3 as *mut _ as usize;

        unsafe {
            list.push(ptr3);
            list.push(ptr2);
            list.push(ptr1);
        }

        // Remove next
        assert!(list.remove_if(ptr1));

        assert_eq!(list.size(), 2);
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

        assert_eq!(list.size(), 2);
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

        assert_eq!(list.size(), 2);
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), Some(ptr2));

        // Remove non-existent
        assert!(!list.remove_if(0xdeadbeef));
    }

    #[test]
    fn test_add_with_sort() {
        let mut list = IntrusiveLinkedList::new();

        let mut intrusive_linked_lists = [
            IntrusiveLinkedList { next: None },
            IntrusiveLinkedList { next: None },
            IntrusiveLinkedList { next: None },
            IntrusiveLinkedList { next: None },
        ];
        let ptrs: Vec<usize> = intrusive_linked_lists
            .iter_mut()
            .map(|n| n as *mut _ as usize)
            .collect();

        // ptrs are sorted by address
        let ptr1 = ptrs[0];
        let ptr2 = ptrs[1];
        let ptr3 = ptrs[2];
        let ptr4 = ptrs[3];

        // Add to empty list
        unsafe { list.add_with_sort(ptr2) };
        assert_eq!(list.size(), 1);
        assert_eq!(list.next.unwrap().as_ptr() as usize, ptr2);

        // Add smaller to next
        unsafe { list.add_with_sort(ptr1) };
        assert_eq!(list.size(), 2);
        assert_eq!(list.next.unwrap().as_ptr() as usize, ptr1);

        // Add to end
        unsafe { list.add_with_sort(ptr4) };
        assert_eq!(list.size(), 3);

        // Add to middle
        unsafe { list.add_with_sort(ptr3) };
        assert_eq!(list.size(), 4);

        // Check final order
        assert_eq!(list.pop(), Some(ptr1));
        assert_eq!(list.pop(), Some(ptr2));
        assert_eq!(list.pop(), Some(ptr3));
        assert_eq!(list.pop(), Some(ptr4));
        assert!(list.is_none());
    }
}
