//! `page_size` / `page_token` for the List* RPCs.
//!
//! Follows the official emulator: items are listed in name order, a page holds at most
//! `page_size` items (0 or out of range means [`MAX_PAGE_SIZE`]), and the next page token is the
//! name of the first item on the next page. A page starts at the first item whose name is at or
//! after the token, so a token stays valid when that item is deleted in between.

/// Largest page, and the page size when the request leaves it at 0.
pub const MAX_PAGE_SIZE: usize = 1000;

/// Returns one page of `items` and the token for the next page (empty on the last page).
/// `items` may come in any order ([`Pager`]).
pub fn page<T>(
    items: impl IntoIterator<Item = T>,
    name: impl Fn(&T) -> &str,
    page_size: i32,
    page_token: &str,
) -> (Vec<T>, String) {
    let mut p = Pager::new(name, page_size, page_token);
    for item in items {
        p.push(item);
    }
    p.finish()
}

/// Collects one page from items in any order, keeping only the page and the item after it: a
/// sorted buffer of at most `page_size + 1` items. A page of a large unsorted collection (sessions
/// in a hash map) then costs one pass over it and no copy of it, and [`Pager::wants`] lets the
/// caller skip building an item that would not be kept. An item in ascending or random order is
/// rejected or inserted in O(log page_size); only descending order makes every insert shift the
/// buffer.
pub struct Pager<'t, T, F> {
    name: F,
    token: &'t str,
    size: usize,
    keep: Vec<T>,
}

impl<'t, T, F: Fn(&T) -> &str> Pager<'t, T, F> {
    pub fn new(name: F, page_size: i32, page_token: &'t str) -> Self {
        let size = match usize::try_from(page_size) {
            Ok(n) if (1..=MAX_PAGE_SIZE).contains(&n) => n,
            _ => MAX_PAGE_SIZE,
        };
        Pager {
            name,
            token: page_token,
            size,
            keep: Vec::new(),
        }
    }

    /// Whether an item named `name` would be kept, given the items pushed so far.
    pub fn wants(&self, name: &str) -> bool {
        name >= self.token
            && (self.keep.len() <= self.size
                || self
                    .keep
                    .last()
                    .is_some_and(|last| name < (self.name)(last)))
    }

    pub fn push(&mut self, item: T) {
        let n = (self.name)(&item);
        if !self.wants(n) {
            return;
        }
        let full = self.keep.len() > self.size;
        let at = self.keep.partition_point(|k| (self.name)(k) < n);
        if full {
            self.keep.pop();
        }
        self.keep.insert(at, item);
    }

    /// The page in name order and the next page token.
    pub fn finish(mut self) -> (Vec<T>, String) {
        let next = self
            .keep
            .get(self.size)
            .map(|i| (self.name)(i).to_string())
            .unwrap_or_default();
        self.keep.truncate(self.size);
        (self.keep, next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("item{i:04}")).collect()
    }

    #[test]
    fn walks_all_pages_once() {
        let all = names(25);
        let mut seen = Vec::new();
        let mut token = String::new();
        loop {
            let (items, next) = page(all.clone(), |s| s.as_str(), 10, &token);
            assert!(items.len() <= 10);
            seen.extend(items);
            if next.is_empty() {
                break;
            }
            token = next;
        }
        assert_eq!(seen, all);
    }

    #[test]
    fn zero_or_negative_size_is_max() {
        let (items, next) = page(names(1200), |s| s.as_str(), 0, "");
        assert_eq!(items.len(), MAX_PAGE_SIZE);
        assert_eq!(next, "item1000");
        let (items, _) = page(names(5), |s| s.as_str(), -3, "");
        assert_eq!(items.len(), 5);
    }

    #[test]
    fn exact_fit_has_no_next_token() {
        let (items, next) = page(names(10), |s| s.as_str(), 10, "");
        assert_eq!(items.len(), 10);
        assert!(next.is_empty());
    }

    #[test]
    fn token_of_a_deleted_item_still_resumes() {
        let mut all = names(6);
        all.remove(3); // item003 was the token
        let (items, _) = page(all, |s| s.as_str(), 10, "item0003");
        assert_eq!(items, vec!["item0004", "item0005"]);
    }

    #[test]
    fn any_input_order_pages_in_name_order() {
        let all = names(57);
        // Ascending, descending and a scrambled order (7 is coprime with 57).
        let orders = [
            all.clone(),
            all.iter().rev().cloned().collect(),
            (0..57)
                .map(|i| all[(i * 7) % 57].clone())
                .collect::<Vec<_>>(),
        ];
        for input in orders {
            let mut seen = Vec::new();
            let mut token = String::new();
            loop {
                let (items, next) = page(input.clone(), |s| s.as_str(), 10, &token);
                seen.extend(items);
                if next.is_empty() {
                    break;
                }
                token = next;
            }
            assert_eq!(seen, all);
        }
    }
}
