//! `page_size` / `page_token` for the List* RPCs.
//!
//! Follows the official emulator: items are listed in name order, a page holds at most
//! `page_size` items (0 or out of range means [`MAX_PAGE_SIZE`]), and the next page token is the
//! name of the first item on the next page. A page starts at the first item whose name is at or
//! after the token, so a token stays valid when that item is deleted in between.

/// Largest page, and the page size when the request leaves it at 0.
pub const MAX_PAGE_SIZE: usize = 1000;

/// Returns one page of `items` and the token for the next page (empty on the last page).
pub fn page<T>(
    mut items: Vec<T>,
    name: impl Fn(&T) -> &str,
    page_size: i32,
    page_token: &str,
) -> (Vec<T>, String) {
    let size = match usize::try_from(page_size) {
        Ok(n) if (1..=MAX_PAGE_SIZE).contains(&n) => n,
        _ => MAX_PAGE_SIZE,
    };
    items.sort_by(|a, b| name(a).cmp(name(b)));
    let start = items.partition_point(|i| name(i) < page_token);
    let mut rest = items.split_off(start);
    let next = rest
        .get(size)
        .map(|i| name(i).to_string())
        .unwrap_or_default();
    rest.truncate(size);
    (rest, next)
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
}
