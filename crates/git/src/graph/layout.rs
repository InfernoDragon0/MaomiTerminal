//! Lane layout after Vigier 2019 ("straight branches"), computed one commit at a time so rows
//! can be drawn while `git log --topo-order` (or `--date-order`) is still streaming.
//!
//! - Commits must arrive children before parents; each one becomes the next row.
//! - A commit takes the column of a lane waiting for it, preferring lanes that come from a
//!   first-parent edge, leftmost first: first-parent chains stay in one column.
//! - Columns held by a live lane are forbidden to everything else; a column freed by a lane
//!   that ends in this row is reused from the next row on, never by this row's own edges.
//!
//! Storage is sized for 1M commits: a row is 12 bytes plus its half-row edges, and a lane is
//! one [`Span`] per straight vertical run (a first-parent chain through many rows is a single
//! span). [`GraphLayout::spans`] answers viewport queries from checkpoints taken every
//! [`CHECKPOINT`] rows, so the renderer never walks the whole history.

use std::collections::HashMap;
use std::ops::Range;

/// Rows between checkpoints of the live lanes; bounds the work of a viewport query.
pub const CHECKPOINT: usize = 256;

/// [`Span::end`] of a lane still waiting for its commit (or whose commit never comes: shallow
/// clone, truncated or path-limited log).
pub const OPEN: u32 = u32::MAX;

/// A commit's dot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Node {
    pub column: u32,
    /// Lane colour id; the renderer maps it onto its palette (`color % palette.len()`).
    pub color: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// Top half of the row: from `column` at the row's top edge into the node.
    Into,
    /// Bottom half of the row: from the node to `column` at the row's bottom edge.
    Out,
}

/// A half-row edge between a node and a lane. Vertical when `column` is the node's column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    pub kind: EdgeKind,
    pub column: u32,
    pub color: u32,
}

/// A straight vertical lane: from the bottom edge of row `start` to the top edge of row `end`.
/// Every row strictly between is crossed in full, including node rows the lane runs through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub column: u32,
    pub color: u32,
    pub start: u32,
    /// [`OPEN`] while the lane is still waiting.
    pub end: u32,
}

impl Span {
    /// True when the span draws a non-empty piece inside `rows`.
    pub fn overlaps(&self, rows: &Range<usize>) -> bool {
        (self.start as usize) + 1 < rows.end && (self.end as usize) > rows.start
    }
}

#[derive(Debug, Clone, Copy)]
struct Row {
    node: Node,
    /// End of this row's edges in `GraphLayout::edges`; they start at the previous row's end.
    edges_end: u32,
}

#[derive(Debug, Clone, Copy)]
struct Lane {
    span: u32,
    color: u32,
    /// The lane is a first-parent edge, so its commit should continue it.
    first_parent: bool,
}

#[derive(Debug, Clone)]
struct Checkpoint {
    /// Spans live when the row was reached.
    live: Vec<u32>,
    /// First span created at or after the row.
    first_new: u32,
}

#[derive(Debug, Default)]
pub struct GraphLayout {
    rows: Vec<Row>,
    edges: Vec<Edge>,
    spans: Vec<Span>,
    checkpoints: Vec<Checkpoint>,
    /// Live lanes by column; `None` is a free column.
    lanes: Vec<Option<Lane>>,
    /// Commit id -> columns of the lanes waiting for it.
    waiting: HashMap<Box<str>, Vec<u32>>,
    width: u32,
    next_color: u32,
}

impl GraphLayout {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of rows laid out.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Most columns any row has used so far.
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn node(&self, row: usize) -> Option<Node> {
        self.rows.get(row).map(|r| r.node)
    }

    /// The row's half-row edges: `Into` edges first, then `Out` edges in parent order. A lane
    /// that runs straight through the node has no edges here, only its [`Span`].
    pub fn edges(&self, row: usize) -> &[Edge] {
        let Some(r) = self.rows.get(row) else {
            return &[];
        };
        let start = row
            .checked_sub(1)
            .map_or(0, |prev| self.rows[prev].edges_end as usize);
        &self.edges[start..r.edges_end as usize]
    }

    /// Spans that draw anything inside `rows`, for the renderer to clip to the viewport.
    pub fn spans(&self, rows: Range<usize>) -> impl Iterator<Item = &Span> + '_ {
        let checkpoint = match self.checkpoints.len() {
            0 => None,
            n => self.checkpoints.get((rows.start / CHECKPOINT).min(n - 1)),
        };
        let (live, new) = match checkpoint {
            Some(c) => (c.live.as_slice(), &self.spans[c.first_new as usize..]),
            None => (&[][..], &[][..]),
        };
        let end = rows.end;
        live.iter()
            .map(|&id| &self.spans[id as usize])
            .chain(new.iter().take_while(move |s| (s.start as usize) < end))
            .filter(move |s| s.overlaps(&rows))
    }

    /// Lays out the next commit as a new row and returns its row index. `parents` in git
    /// order (first parent first); repeated parents are ignored.
    pub fn push(&mut self, id: &str, parents: &[impl AsRef<str>]) -> usize {
        let row = self.rows.len();
        let r = u32::try_from(row).expect("more than u32::MAX commits");
        if row.is_multiple_of(CHECKPOINT) {
            self.checkpoints.push(Checkpoint {
                live: self.lanes.iter().flatten().map(|l| l.span).collect(),
                first_new: self.spans.len() as u32,
            });
        }

        let mut unique: Vec<&str> = Vec::with_capacity(parents.len());
        for parent in parents.iter().map(AsRef::as_ref) {
            if !unique.contains(&parent) {
                unique.push(parent);
            }
        }
        let parents = unique;

        let mut waiting = self.waiting.remove(id).unwrap_or_default();
        waiting.sort_unstable();
        let chosen = waiting
            .iter()
            .copied()
            .find(|&c| self.lane(c).first_parent)
            .or_else(|| waiting.first().copied());

        // Lanes merging into this node end here. Their columns stay taken until the row is
        // done, so this row's new lanes cannot reuse them.
        let mut ended = Vec::new();
        for &column in &waiting {
            let continues = Some(column) == chosen && !parents.is_empty();
            if continues {
                continue;
            }
            let lane = self.lane(column);
            self.spans[lane.span as usize].end = r;
            self.edges.push(Edge {
                kind: EdgeKind::Into,
                column,
                color: lane.color,
            });
            ended.push(column);
        }

        let node = match chosen {
            Some(column) => Node {
                column,
                color: self.lane(column).color,
            },
            None => Node {
                column: self.free_column(),
                color: self.new_color(),
            },
        };

        for (i, &parent) in parents.iter().enumerate() {
            if i == 0 {
                match chosen {
                    // First-parent chain: the lane runs straight through this node.
                    Some(column) => {
                        if let Some(lane) = &mut self.lanes[column as usize] {
                            lane.first_parent = true;
                        }
                    }
                    None => {
                        self.open_lane(node.column, node.color, r, true);
                        self.push_out(node.column, node.color);
                    }
                }
                self.wait_for(parent, node.column);
                continue;
            }
            let joined = self
                .waiting
                .get(parent)
                .and_then(|cols| cols.iter().copied().min());
            match joined {
                Some(column) => {
                    let color = self.lane(column).color;
                    self.push_out(column, color);
                }
                None => {
                    let column = self.free_column();
                    let color = self.new_color();
                    self.open_lane(column, color, r, false);
                    self.wait_for(parent, column);
                    self.push_out(column, color);
                }
            }
        }

        self.width = self.width.max(node.column + 1).max(self.lanes.len() as u32);
        for column in ended {
            self.lanes[column as usize] = None;
        }
        while matches!(self.lanes.last(), Some(None)) {
            self.lanes.pop();
        }
        self.rows.push(Row {
            node,
            edges_end: self.edges.len() as u32,
        });
        row
    }

    fn lane(&self, column: u32) -> Lane {
        self.lanes[column as usize].expect("lane is live")
    }

    fn new_color(&mut self) -> u32 {
        let color = self.next_color;
        self.next_color = self.next_color.wrapping_add(1);
        color
    }

    /// Leftmost free column.
    fn free_column(&mut self) -> u32 {
        match self.lanes.iter().position(Option::is_none) {
            Some(column) => column as u32,
            None => {
                self.lanes.push(None);
                (self.lanes.len() - 1) as u32
            }
        }
    }

    fn open_lane(&mut self, column: u32, color: u32, row: u32, first_parent: bool) {
        let span = self.spans.len() as u32;
        self.spans.push(Span {
            column,
            color,
            start: row,
            end: OPEN,
        });
        self.lanes[column as usize] = Some(Lane {
            span,
            color,
            first_parent,
        });
    }

    fn push_out(&mut self, column: u32, color: u32) {
        self.edges.push(Edge {
            kind: EdgeKind::Out,
            column,
            color,
        });
    }

    fn wait_for(&mut self, parent: &str, column: u32) {
        match self.waiting.get_mut(parent) {
            Some(cols) => cols.push(column),
            None => {
                self.waiting.insert(parent.into(), vec![column]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lays out `(id, parents)` rows in the given (child-first) order.
    fn layout(commits: &[(&str, &[&str])]) -> GraphLayout {
        let mut g = GraphLayout::new();
        for (id, parents) in commits {
            g.push(id, parents);
        }
        check(&g);
        g
    }

    fn columns(g: &GraphLayout) -> Vec<u32> {
        (0..g.len()).map(|r| g.node(r).unwrap().column).collect()
    }

    fn out(column: u32, color: u32) -> Edge {
        Edge {
            kind: EdgeKind::Out,
            column,
            color,
        }
    }

    fn into(column: u32, color: u32) -> Edge {
        Edge {
            kind: EdgeKind::Into,
            column,
            color,
        }
    }

    /// Structural invariants, checked against the raw span list (no checkpoints).
    fn check(g: &GraphLayout) {
        let rows = g.len() as u32;
        for r in 0..rows {
            let node = g.node(r as usize).unwrap();
            // Spans crossing the whole row never share a column.
            let mut crossing: Vec<u32> = g
                .spans
                .iter()
                .filter(|s| s.start < r && s.end > r)
                .map(|s| s.column)
                .collect();
            crossing.sort_unstable();
            let before = crossing.len();
            crossing.dedup();
            assert_eq!(before, crossing.len(), "row {r}: two lanes in one column");

            for e in g.edges(r as usize) {
                match e.kind {
                    EdgeKind::Into => assert!(
                        g.spans
                            .iter()
                            .any(|s| s.column == e.column && s.end == r && s.color == e.color),
                        "row {r}: Into edge {e:?} has no lane ending here"
                    ),
                    EdgeKind::Out => {
                        let starts = g
                            .spans
                            .iter()
                            .any(|s| s.column == e.column && s.start == r && s.color == e.color);
                        let joins = e.column != node.column
                            && g.spans
                                .iter()
                                .any(|s| s.column == e.column && s.start < r && s.end > r);
                        assert!(starts || joins, "row {r}: Out edge {e:?} leads nowhere");
                    }
                }
            }
            // A lane crossing the node's column must be the node's own (straight through).
            if let Some(s) = g
                .spans
                .iter()
                .find(|s| s.column == node.column && s.start < r && s.end > r)
            {
                assert_eq!(s.color, node.color, "row {r}: lane runs over the node");
                assert!(
                    g.edges(r as usize).iter().all(|e| e.column != node.column),
                    "row {r}: straight lane plus vertical edge"
                );
            }
        }
        for (i, s) in g.spans.iter().enumerate() {
            assert!(s.start < rows, "span {i} starts after the last row");
            assert!(
                s.end == OPEN || (s.start < s.end && s.end < rows),
                "span {i}: {s:?}"
            );
            assert!(
                g.edges(s.start as usize)
                    .iter()
                    .any(|e| e.kind == EdgeKind::Out && e.column == s.column),
                "span {i} has no Out edge at its start"
            );
            if s.end != OPEN {
                assert!(
                    g.edges(s.end as usize)
                        .iter()
                        .any(|e| e.kind == EdgeKind::Into && e.column == s.column),
                    "span {i} has no Into edge at its end"
                );
            }
        }
    }

    /// Checkpointed viewport query equals a full scan.
    fn check_queries(g: &GraphLayout, ranges: impl IntoIterator<Item = Range<usize>>) {
        for range in ranges {
            let mut fast: Vec<Span> = g.spans(range.clone()).copied().collect();
            let mut slow: Vec<Span> = g
                .spans
                .iter()
                .filter(|s| s.overlaps(&range))
                .copied()
                .collect();
            let key = |s: &Span| (s.start, s.column);
            fast.sort_by_key(key);
            slow.sort_by_key(key);
            assert_eq!(fast, slow, "spans({range:?})");
        }
    }

    #[test]
    fn empty() {
        let g = GraphLayout::new();
        assert!(g.is_empty());
        assert_eq!(g.width(), 0);
        assert_eq!(g.edges(0), &[]);
        assert_eq!(g.spans(0..10).count(), 0);
    }

    #[test]
    fn linear_history_is_one_straight_lane() {
        let g = layout(&[("c", &["b"]), ("b", &["a"]), ("a", &[])]);
        assert_eq!(columns(&g), [0, 0, 0]);
        assert_eq!(g.width(), 1);
        assert_eq!(g.edges(0), &[out(0, 0)]);
        assert_eq!(g.edges(1), &[]);
        assert_eq!(g.edges(2), &[into(0, 0)]);
        assert_eq!(
            g.spans,
            [Span {
                column: 0,
                color: 0,
                start: 0,
                end: 2
            }]
        );
    }

    #[test]
    fn single_root_commit() {
        let g = layout(&[("a", &[])]);
        assert_eq!(columns(&g), [0]);
        assert_eq!(g.edges(0), &[]);
        assert!(g.spans.is_empty());
    }

    #[test]
    fn feature_branch_merged_back() {
        // m merges f into main; both branch off b.
        let g = layout(&[
            ("m", &["c", "f"]),
            ("f", &["b"]),
            ("c", &["b"]),
            ("b", &["a"]),
            ("a", &[]),
        ]);
        assert_eq!(columns(&g), [0, 1, 0, 0, 0]);
        assert_eq!(g.width(), 2);
        assert_eq!(g.edges(0), &[out(0, 0), out(1, 1)]);
        // f keeps its lane straight down to b, where it merges into main's column.
        assert_eq!(g.edges(1), &[]);
        assert_eq!(g.edges(3), &[into(1, 1)]);
        assert_eq!(g.node(3).unwrap().color, 0);
    }

    #[test]
    fn first_parent_wins_over_a_merge_lane_to_its_left() {
        // x's lane to b is a merge (second-parent) edge in column 1; y's first-parent lane to
        // b is in column 2, so b continues y's branch in column 2.
        let g = layout(&[("x", &["p", "b"]), ("y", &["b"]), ("p", &[]), ("b", &[])]);
        assert_eq!(columns(&g), [0, 2, 0, 2]);
        assert_eq!(g.node(3).unwrap().color, g.node(1).unwrap().color);
        assert_eq!(g.edges(3), &[into(1, 1), into(2, 2)]);
    }

    #[test]
    fn two_tips_on_one_base() {
        let g = layout(&[("x", &["b"]), ("y", &["b"]), ("b", &[])]);
        assert_eq!(columns(&g), [0, 1, 0]);
        assert_eq!(g.edges(2), &[into(0, 0), into(1, 1)]);
        assert_eq!(g.width(), 2);
    }

    #[test]
    fn octopus_merge_opens_a_lane_per_parent() {
        let g = layout(&[
            ("m", &["a", "b", "c"]),
            ("c", &["r"]),
            ("b", &["r"]),
            ("a", &["r"]),
            ("r", &[]),
        ]);
        assert_eq!(columns(&g), [0, 2, 1, 0, 0]);
        assert_eq!(g.edges(0), &[out(0, 0), out(1, 1), out(2, 2)]);
        assert_eq!(g.width(), 3);
    }

    #[test]
    fn merge_joins_a_lane_already_waiting_for_the_parent() {
        // x and m both need b: m's second-parent edge joins x's lane instead of opening one.
        let g = layout(&[("x", &["b"]), ("m", &["a", "b"]), ("a", &["b"]), ("b", &[])]);
        assert_eq!(columns(&g), [0, 1, 1, 0]);
        assert_eq!(g.edges(1), &[out(1, 1), out(0, 0)]);
        assert_eq!(g.width(), 2);
    }

    #[test]
    fn freed_columns_are_reused_next_row_not_this_one() {
        // At b the lane from y (column 1) ends; b's new second-parent lane must not take
        // column 1 in the same row, but the next tip may.
        let g = layout(&[
            ("x", &["b"]),
            ("y", &["b"]),
            ("b", &["a", "s"]),
            ("z", &["a"]),
            ("s", &[]),
            ("a", &[]),
        ]);
        assert_eq!(g.edges(2), &[into(1, 1), out(2, 2)]);
        assert_eq!(columns(&g), [0, 1, 0, 1, 2, 0]);
        assert_eq!(g.width(), 3);
    }

    #[test]
    fn columns_are_reused_after_a_branch_ends() {
        // Ten short side branches merged into main one after another.
        let mut commits: Vec<(String, Vec<String>)> = Vec::new();
        for i in 0..10 {
            commits.push((
                format!("m{i}"),
                vec![format!("m{}", i + 1), format!("s{i}")],
            ));
            commits.push((format!("s{i}"), vec![format!("m{}", i + 1)]));
        }
        commits.push(("m10".into(), vec![]));
        let mut g = GraphLayout::new();
        for (id, parents) in &commits {
            g.push(id, parents);
        }
        check(&g);
        // Each side lane ends in the row that opens the next one, so they alternate between
        // columns 1 and 2 instead of growing to 11.
        assert_eq!(g.width(), 3);
        let sides: Vec<u32> = (0..10).map(|i| g.node(2 * i + 1).unwrap().column).collect();
        assert_eq!(sides, [1, 2, 1, 2, 1, 2, 1, 2, 1, 2]);
    }

    #[test]
    fn dangling_parent_stays_open() {
        // Shallow clone: b's parent never arrives.
        let g = layout(&[("c", &["b"]), ("b", &["gone"]), ("x", &[])]);
        assert_eq!(g.spans[0].end, OPEN);
        assert_eq!(columns(&g), [0, 0, 1]);
        assert_eq!(g.spans(2..3).count(), 1);
    }

    #[test]
    fn repeated_parent_is_one_edge() {
        let g = layout(&[("m", &["a", "a"]), ("a", &[])]);
        assert_eq!(g.edges(0), &[out(0, 0)]);
        assert_eq!(g.width(), 1);
    }

    #[test]
    fn repeated_commit_becomes_a_tip() {
        let g = layout(&[("b", &["a"]), ("a", &[]), ("a", &[])]);
        assert_eq!(columns(&g), [0, 0, 0]);
        assert_eq!(g.edges(2), &[]);
    }

    /// Tiny deterministic PRNG (xorshift) so the stress test needs no dependency.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Random DAG in child-first order: commit `i`'s parents are older commits (`< i`),
    /// mostly recent ones, with merges, octopuses, extra roots and dangling parents. One parent
    /// in `far` may be any older commit (long-lived lanes).
    fn random_dag(rng: &mut Rng, n: usize, far: usize) -> Vec<(String, Vec<String>)> {
        let mut commits = Vec::with_capacity(n);
        for i in 0..n {
            let mut parents = Vec::new();
            if i > 0 && rng.below(40) != 0 {
                let count = match rng.below(20) {
                    0 => 3,
                    1..=5 => 2,
                    _ => 1,
                };
                for _ in 0..count {
                    let back = if rng.below(far) == 0 {
                        1 + rng.below(i)
                    } else {
                        1 + rng.below(i.min(8))
                    };
                    parents.push(format!("c{}", i - back));
                }
            }
            if rng.below(200) == 0 {
                parents.push(format!("missing{i}"));
            }
            commits.push((format!("c{i}"), parents));
        }
        commits.reverse();
        commits
    }

    #[test]
    fn random_dags_keep_invariants_and_queries_match() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for round in 0..40 {
            let n = 50 + round * 40;
            let mut g = GraphLayout::new();
            for (id, parents) in &random_dag(&mut rng, n, 4) {
                g.push(id, parents);
            }
            assert_eq!(g.len(), n);
            check(&g);
            let mut ranges = vec![0..n, 0..0, n..n + 5, n - 1..n, 0..1];
            for _ in 0..30 {
                let a = rng.below(n + 1);
                ranges.push(a..a + rng.below(80));
            }
            check_queries(&g, ranges);
        }
    }

    /// `cargo test -p maomi-git --release --lib graph::layout::tests::perf -- --ignored --nocapture`
    #[test]
    #[ignore = "timing, run by hand"]
    fn perf_one_million_commits() {
        let n = 1_000_000;
        let dag = random_dag(&mut Rng(7), n, 500);
        let started = std::time::Instant::now();
        let mut g = GraphLayout::new();
        for (id, parents) in &dag {
            g.push(id, parents);
        }
        let layout = started.elapsed();
        let started = std::time::Instant::now();
        let mut drawn = 0;
        for top in (0..n).step_by(997) {
            drawn += g.spans(top..top + 60).count();
        }
        let queries = n.div_ceil(997);
        println!(
            "layout {n} commits: {layout:?} ({:?}/commit), width {}, {} spans; \
             viewport query: {:?} avg ({drawn} spans over {queries} queries)",
            layout / n as u32,
            g.width(),
            g.spans.len(),
            started.elapsed() / queries as u32,
        );
    }

    #[test]
    fn long_first_parent_chain_is_a_single_span() {
        let n = 10_000;
        let mut g = GraphLayout::new();
        for i in (0..n).rev() {
            let parents: Vec<String> = (i > 0).then(|| (i - 1).to_string()).into_iter().collect();
            g.push(&i.to_string(), &parents);
        }
        assert_eq!(g.spans.len(), 1);
        assert_eq!(g.spans(5_000..5_040).count(), 1);
        check_queries(&g, [0..1, 9_990..10_000, 255..257, 4_000..4_100]);
    }
}
