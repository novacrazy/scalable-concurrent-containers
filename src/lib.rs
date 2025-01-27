#![deny(missing_docs, warnings, clippy::all, clippy::pedantic)]
#![doc = include_str!("../README.md")]

pub mod hash_map;
pub use hash_map::HashMap;

pub mod hash_set;
pub use hash_set::HashSet;

pub mod hash_index;
pub use hash_index::HashIndex;

pub mod hash_cache;
pub use hash_cache::HashCache;

mod linked_list;
pub use linked_list::Entry as LinkedEntry;
pub use linked_list::LinkedList;

mod bag;
pub use bag::Bag;

mod queue;
pub use queue::Queue;

mod stack;
pub use stack::Stack;

pub mod tree_index;
pub use tree_index::TreeIndex;

pub mod ebr;

mod exit_guard;
mod hash_table;
mod wait_queue;

#[cfg(feature = "serde")]
mod serde;

#[cfg(test)]
mod tests;
