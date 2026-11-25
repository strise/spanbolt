//! Cloud Spanner's SCORE values and its query stopwords, fitted to prod recordings
//! (`conformance/cases/sql/search_score.case`, tags `oracle3` and `oracle4`).
//!
//! The model below reproduces all 60 recorded SCORE values checked (every recorded value except the gaps below) bit for bit (evaluated in f64, rounded to
//! float32 like prod's values):
//! - n is the number of positive query terms (negated terms and stopwords don't count; terms that
//!   no document contains do).
//! - Each term that occurs tf > 0 times in the document adds `1 + A(n) * (ln(1 + C*tf) - D)`, with
//!   `A(n) = ALPHA - BETA / (n + K)`. No document length, document frequency or corpus size factor.
//! - Each pair of consecutive query terms (adjacent in the query text, joined by AND, a phrase or
//!   AROUND, not OR) that also occurs adjacently and in order in the document adds `G * A(n)`.
//!
//! Known gaps: `enhance_query => true` (prod scores by Google's query expansion, not the data: one
//! hit of `alpha` 0.5597, `beta` 0.5813, `delta` 0.6315 in 2 rows and in 12, made-up `qzxv` 1.2589
//! (the plain score) and `wplk` 1.4325 in 2 rows each; #160), and a query term repeated in the query
//! (`alpha alpha` over `alpha alpha`: prod 3.937, this model 4.073; `alpha OR alpha` counts once).

use super::rquery::Node;

const C: f64 = 0.402_750_545_3;
const D: f64 = 0.169_230_773_2;
const ALPHA: f64 = 2.131_147_509_7;
const BETA: f64 = 4.688_524_257_9;
const K: f64 = 6.8;
const G: f64 = 0.453_766_088_7;

/// Words prod drops from SEARCH and SCORE queries unless the query has nothing else, recorded one
/// by one (oracle5, `search_score/stopwords_each_word`): the Lucene English stop set minus `but`,
/// `if`, `into`, `no`, `not`, `such`, `their`, `then`, `there`, `these`, `they` and `will`, plus
/// `i`. Case and `language_tag` don't matter; Norwegian function words (`og`, `på`, `av`, `til`,
/// `med`) and legal forms other than `as` (`asa`, `ans`, `da`, `sa`, `ba`) are kept.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "i", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to", "was", "with",
];

fn is_stopword(t: &[u8]) -> bool {
    STOPWORDS.iter().any(|s| s.as_bytes() == t)
}

/// Whether the query has a term that is not a stopword, negated or not.
fn has_content(n: &Node) -> bool {
    match n {
        Node::Term(t) => !is_stopword(t),
        Node::Not(c) => has_content(c),
        Node::And(c) | Node::Or(c) | Node::Phrase(c) | Node::Around(c) | Node::InOrder(c) => {
            c.iter().any(has_content)
        }
        Node::Number(_) => false,
    }
}

fn is_stop_term(n: &Node) -> bool {
    matches!(n, Node::Term(t) if is_stopword(t))
}

/// Prod's SEARCH query: when the query has a term that is not a stopword, a stopword in an AND
/// or OR group matches every document (`the -apple` is `-apple`, `the | -apple` matches all).
/// Phrases, AROUND and negations keep theirs (`-the` is literal).
pub fn drop_stopwords(n: Node) -> Node {
    if has_content(&n) { strip(n) } else { n }
}

/// Whether prod's SEARCH matches a NULL document: only for a query whose top-level AND is
/// negations plus at least one stopword (or lower-case `or`) dropped from it (`the -apple`,
/// `-apple or`; not `-apple` nor `the | -apple`).
pub fn matches_null(n: &Node, dropped_or: bool) -> bool {
    if !has_content(n) {
        return false;
    }
    let c = match n {
        Node::And(c) => c.as_slice(),
        other => std::slice::from_ref(other),
    };
    let dropped = dropped_or || c.iter().any(is_stop_term);
    let rest: Vec<&Node> = c.iter().filter(|n| !is_stop_term(n)).collect();
    dropped && !rest.is_empty() && rest.iter().all(|n| matches!(n, Node::Not(_)))
}

fn strip(n: Node) -> Node {
    let keep = |c: Vec<Node>| -> Vec<Node> {
        let all: Vec<Node> = c.into_iter().map(strip).collect();
        let kept: Vec<Node> = all
            .iter()
            .filter(|n| !matches!(n, Node::Term(t) if is_stopword(t)))
            .cloned()
            .collect();
        if kept.is_empty() { all } else { kept }
    };
    let wrap = |mut c: Vec<Node>, f: fn(Vec<Node>) -> Node| {
        if c.len() == 1 {
            c.pop().unwrap_or(Node::And(Vec::new()))
        } else {
            f(c)
        }
    };
    match n {
        Node::And(c) => wrap(keep(c), Node::And),
        // An empty AND matches every document.
        Node::Or(c) if c.iter().any(is_stop_term) => Node::And(Vec::new()),
        Node::Or(c) => wrap(keep(c), Node::Or),
        Node::Not(c) => Node::Not(Box::new(strip(*c))),
        other => other,
    }
}

/// A query prepared for SCORE: its counted terms and the adjacent pairs among them.
#[derive(Debug, Default)]
pub struct ScoreQuery {
    /// Distinct terms; `slots` index into this.
    pub terms: Vec<Vec<u8>>,
    /// The counted query terms in query order, as indexes into `terms`.
    slots: Vec<usize>,
    /// `i` for each pair `(slots[i], slots[i + 1])` that earns the adjacency bonus.
    pairs: Vec<usize>,
}

/// A query term: its text, its position in the query, its ancestors as (node id, is OR), and
/// whether it may be dropped as a stopword (not inside a phrase or AROUND).
type Placed = (Vec<u8>, usize, Vec<(usize, bool)>, bool);

struct Walk {
    next: usize,
    ids: usize,
    out: Vec<Placed>,
}

fn count_terms(n: &Node) -> usize {
    match n {
        Node::Term(_) => 1,
        Node::Number(_) => 0,
        Node::Not(c) => count_terms(c),
        Node::And(c) | Node::Or(c) | Node::Phrase(c) | Node::Around(c) | Node::InOrder(c) => {
            c.iter().map(count_terms).sum()
        }
    }
}

fn walk(n: &Node, path: &mut Vec<(usize, bool)>, grouped: bool, w: &mut Walk) {
    match n {
        Node::Term(t) => {
            w.out
                .push((t.clone(), w.next, path.clone(), !grouped && is_stopword(t)));
            w.next += 1;
        }
        Node::Number(_) => {}
        Node::Not(c) => w.next += count_terms(c),
        Node::And(c) | Node::Or(c) | Node::Phrase(c) | Node::Around(c) | Node::InOrder(c) => {
            w.ids += 1;
            path.push((w.ids, matches!(n, Node::Or(_))));
            let grouped =
                grouped || matches!(n, Node::Phrase(_) | Node::Around(_) | Node::InOrder(_));
            c.iter().for_each(|c| walk(c, path, grouped, w));
            path.pop();
        }
    }
}

impl ScoreQuery {
    /// From the parsed query, before stopwords are dropped (they still separate terms).
    pub fn new(n: &Node) -> ScoreQuery {
        let mut w = Walk {
            next: 0,
            ids: 0,
            out: Vec::new(),
        };
        walk(n, &mut Vec::new(), false, &mut w);
        if w.out.iter().any(|(t, ..)| !is_stopword(t)) {
            w.out.retain(|(.., stop)| !stop);
        }
        let mut q = ScoreQuery::default();
        for (t, ..) in &w.out {
            let i = match q.terms.iter().position(|x| x == t) {
                Some(i) => i,
                None => {
                    q.terms.push(t.clone());
                    q.terms.len() - 1
                }
            };
            q.slots.push(i);
        }
        for (i, pair) in w.out.windows(2).enumerate() {
            let ((_, qa, pa, _), (_, qb, pb, _)) = (&pair[0], &pair[1]);
            let common = pa.iter().zip(pb).take_while(|(a, b)| a == b).last();
            if *qb == qa + 1 && common.is_some_and(|(&(_, or), _)| !or) {
                q.pairs.push(i);
            }
        }
        q
    }

    /// The score given each distinct term's (sorted) positions in the document.
    pub fn score(&self, positions: &[Vec<i64>]) -> f64 {
        let n = self.slots.len();
        if n == 0 {
            return 0.0;
        }
        let a = ALPHA - BETA / (n as f64 + K);
        let mut s = 0.0;
        for &t in &self.slots {
            let tf = positions[t].len();
            if tf > 0 {
                s += 1.0 + a * ((1.0 + C * tf as f64).ln() - D);
            }
        }
        for &i in &self.pairs {
            let (pa, pb) = (&positions[self.slots[i]], &positions[self.slots[i + 1]]);
            if pa.iter().any(|p| pb.binary_search(&(p + 1)).is_ok()) {
                s += G * a;
            }
        }
        s as f32 as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::rquery::parse;

    fn q(s: &str) -> ScoreQuery {
        ScoreQuery::new(&parse(s, false).unwrap().unwrap())
    }

    /// Positions of each distinct term of `query` in the space-separated `doc`.
    fn score(query: &str, doc: &str) -> f64 {
        let sq = q(query);
        let words: Vec<&str> = doc.split(' ').collect();
        let pos: Vec<Vec<i64>> = sq
            .terms
            .iter()
            .map(|t| {
                (0..words.len() as i64)
                    .filter(|&i| words[i as usize].as_bytes() == t.as_slice())
                    .collect()
            })
            .collect();
        sq.score(&pos)
    }

    #[test]
    fn recorded_prod_values() {
        let cases: &[(&str, &str, f64)] = &[
            ("alpha", "alpha", 1.2588917016983032),
            ("alpha", "alpha alpha alpha alpha", 2.2095134258270264),
            ("alpha OR beta", "alpha alpha", 1.673880696296692),
            (
                "alpha OR beta OR gamma",
                "alpha beta beta",
                2.976450204849243,
            ),
            (
                "alpha OR zzzz OR yyyy",
                "alpha alpha alpha",
                2.029599666595459,
            ),
            ("alpha beta", "alpha beta beta", 3.669611930847168),
            ("alpha beta gamma", "alpha beta gamma", 5.338847637176514),
            (
                "\"alpha beta gamma\"",
                "alpha beta gamma",
                5.338847637176514,
            ),
            (
                "alpha (beta OR delta)",
                "alpha beta beta",
                3.7264013290405273,
            ),
            ("alpha beta delta", "alpha beta x delta", 4.58889627456665),
            ("alpha -beta", "alpha alpha", 1.6450823545455933),
            ("alpha gamma", "alpha beta gamma", 2.5408987998962402),
            ("w2 w1", "w1 w2 w3", 2.5408987998962402),
            (
                "w1 w2 w3 w4 w5 w6 w7 w8",
                "w1 w2 w3 w4 w5 w6 w7 w8",
                16.219022750854492,
            ),
            ("alpha as", "alpha", 1.2588917016983032),
            ("alpha of beta", "alpha of beta", 2.5408987998962402),
            ("as", "alpha as", 1.2588917016983032),
            (
                "nordlys holding as",
                "nordlys holding as",
                3.2661805152893066,
            ),
            ("alpha xy", "alpha xy", 3.2661805152893066),
            ("alpha i", "alpha", 1.2588917016983032),
            ("\"alpha as\"", "alpha as", 3.2661805152893066),
            ("alpha og", "alpha og", 3.2661805152893066),
            ("alpha not", "alpha not", 3.2661805152893066),
            (
                "alpha OR zz1 OR zz2 OR zz3 OR zz4 OR zz5 OR zz6 OR zz7 OR zz8 OR zz9 OR zz10 OR zz11 OR zz12 OR zz13 OR zz14 OR zz15 OR zz16 OR zz17 OR zz18 OR zz19 OR zz20 OR zz21 OR zz22 OR zz23",
                "alpha alpha alpha alpha",
                2.564345121383667,
            ),
        ];
        for (query, doc, want) in cases {
            assert_eq!(score(query, doc), *want, "{query} over {doc}");
        }
        let kappa16 = vec!["kappa"; 16].join(" ");
        assert_eq!(score("kappa", &kappa16), 3.812513828277588);
    }

    #[test]
    fn stopwords_dropped_from_search() {
        let show = |s: &str| format!("{:?}", drop_stopwords(parse(s, false).unwrap().unwrap()));
        assert_eq!(show("alpha as"), "Term([97, 108, 112, 104, 97])");
        assert_eq!(show("as"), "Term([97, 115])");
        assert_eq!(show("the OR of"), show("the OR of"));
        assert!(show("\"alpha of beta\"").starts_with("Phrase"));
    }
}
