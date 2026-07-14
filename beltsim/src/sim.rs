// beltsim Stage 0: naive tick-by-tick reference oracle. v0.2.
// This file DEFINES the canonical semantics. Every future optimization
// (event core, gap encoding, macro fusion) must be differentially tested
// against this implementation. Correctness over cleverness.
//
// Determinism rules:
//   * Single threaded. Integer ticks. No floating point in any decision path.
//   * Per tick, phases run in fixed order:
//     STIMULI -> SOURCES -> JUNCTIONS -> BELTS.
//   * Within a phase, entities process in ascending declaration id order;
//     same-tick stimuli apply in declaration order.
//   * Junction turns are dirty-driven: a turn is provably a no-op unless the
//     junction's neighborhood changed, so skipping clean junctions cannot
//     change state. Newly dirtied nodes run on the NEXT tick.
//
// All timing constants are PRE-BENCH values. See README "Calibration knobs".

use std::collections::{BTreeMap, VecDeque};

/// Simulated ticks per simulated second.
/// 4680 is the least common multiple that makes every belt tier an exact
/// integer period: Mk1..Mk6 item rates per second are 1, 2, 4.5, 8, 13, 20.
pub const TICK_RATE: u64 = 4680;

pub type Item = u16;
pub const EMPTY: Item = 0;

pub fn mk_rate_per_min(mk: u8) -> Option<u32> {
    // Official wiki values, 1.0+. Pre-bench: confirm in game before trusting
    // any rate-sensitive design (the catalog philosophy says counts, not
    // rates, should carry meaning, so this rarely matters).
    match mk {
        1 => Some(60),
        2 => Some(120),
        3 => Some(270),
        4 => Some(480),
        5 => Some(780),
        6 => Some(1200),
        _ => None,
    }
}

/// Convert an items-per-minute rate into a per-slot-advance period in ticks.
/// Errors if the rate does not divide the tick grid exactly: we refuse to
/// approximate, because approximation is where phantom timing bugs live.
pub fn period_from_rate_per_min(rate: u32) -> Result<u64, String> {
    if rate == 0 {
        return Err("rate must be > 0".into());
    }
    let num = TICK_RATE * 60;
    if num % rate as u64 != 0 {
        return Err(format!(
            "rate {}/min does not divide the {} Hz tick grid exactly; \
             pick a rate r such that {} % r == 0",
            rate, TICK_RATE, num
        ));
    }
    Ok(num / rate as u64)
}

/// Smart splitter output filters. A port carries a LIST of these.
/// Matching semantics (pre-bench, bench the exact in-game trio before
/// trusting a design that leans on the distinctions):
///   * Item(x): exact filter.
///   * Any: matches every item, competes as a normal filter.
///   * Undefined: matches an item only if NO port on this splitter carries
///     an exact filter for it (the game's "Any Undefined").
///   * Overflow: last resort, used only when the eligible set is empty or
///     every eligible port buffer is occupied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    Item(Item),
    Any,
    Undefined,
    Overflow,
}

#[derive(Clone, Debug)]
pub enum NodeKind {
    /// Emits `item` on its cadence. A blocked source holds exactly one
    /// pending item and does NOT accumulate a backlog (documented deviation:
    /// matches "pre-stage everything" usage). A source outside its window
    /// never places an item, even one armed earlier.
    Source {
        item: Item,
        period: u64,
        start_tick: u64,
        stop_tick: u64,
        limit: Option<u64>,
        emitted: u64,
        pending: bool,
    },
    /// Human/script input. No cadence: emits only when pressed. Each press
    /// queues an exact count; queued items place as the output tail frees,
    /// one per turn. Backlog is the point here: press counts are exact.
    Button { item: Item, queued: u64 },
    /// Consumes the head of its input every turn. Counts by type.
    Sink,
    /// Plain splitter. Round-robin over connected outputs, skipping outputs
    /// whose 1-item port buffer is occupied. Per-output buffers model the
    /// game's OutputInventory_N slots and reproduce the one-slot-stub bench
    /// end state (1 parked + 1 in splitter buffer + 1 overflowed).
    Splitter { rr: usize, out_bufs: [Item; 3] },
    /// Smart splitter. Each connected output has a rule LIST. Pull is
    /// prechecked: if the head item has no admissible destination the
    /// splitter refuses the pull and the input jams for ALL types
    /// (poison-pill law). Rules are runtime-editable via stimuli.
    SmartSplitter {
        rules: [Vec<Rule>; 3],
        rr: usize,
        out_bufs: [Item; 3],
    },
    /// Plain merger. Single 1-item central buffer. Round-robin pull over
    /// occupied inputs. Output blocked means buffer stays full means neither
    /// input is serviced: both back up (merge law).
    Merger { rr: usize, buf: Item },
    /// Priority merger. Single 1-item central buffer. Strict priority pull:
    /// ins[0] then ins[1] then ins[2] (the builder orders them high, med,
    /// low; runtime priority stimuli reorder the vector). A backed-up low
    /// never blocks high. The central buffer reproduces the PM-Low Leak Law:
    /// one low item can enter the buffer during a high gap.
    PMerger { buf: Item },
    /// Container: preserves type and count, destroys order and timing.
    /// Modeled as a FIFO for determinism (documented deviation: the game
    /// order rule is uncalibrated; catalog restricts containers to starved
    /// single-type level wires).
    Container { cap: usize, fifo: VecDeque<Item> },
    /// Test stimulus: a 1-in 1-out valve. Not a game entity. Stands in for
    /// corks and cut streams. open_at/close_at in the netlist compile to
    /// timed stimuli; interactive open/close applies immediately.
    Gate { open: bool, buf: Item },
}

pub struct Node {
    pub name: String,
    pub kind: NodeKind,
    /// Input edge ids in port order. PMerger: priority order (index 0 is
    /// serviced first).
    pub ins: Vec<usize>,
    /// Output edge ids in port order.
    pub outs: Vec<usize>,
}

pub struct Edge {
    pub name: String,
    pub from_node: usize,
    pub to_node: usize,
    /// Ticks between slot advances. period = TICK_RATE / (items per second).
    pub period: u64,
    /// slots[0] is the HEAD (downstream end, at to_node).
    /// slots[len-1] is the TAIL (upstream end, at from_node).
    pub slots: Vec<Item>,
    /// Lift mouth: an extra 1-item entrance buffer upstream of the tail
    /// slot. An item entering a lift parks in the mouth first and transfers
    /// to the tail slot on the lift's cadence. This gives a 1-slot lift an
    /// effective capacity of 2, which is why the catalog's minimum lift
    /// seal is 2 tokens. Pre-bench: bench 4 (lift torture) calibrates.
    /// Tight coupling (splitter port directly into lift) is mouth=0.
    pub mouth: Option<Item>,
    pub has_mouth: bool,
    /// Item count including the mouth.
    pub count: usize,
    pub last_progress: u64,
    /// Lifetime items that entered at the tail or mouth (prefill counts).
    pub entered: u64,
    /// Lifetime items delivered off the head.
    pub delivered: u64,
    /// True when the edge's shape is provably static: empty, saturated with
    /// an occupied head, or compacted with nothing arriving. A sleeping edge
    /// is skipped by its cadence group; place_tail/take_head wake it.
    asleep: bool,
    /// Index into edges_by_period / belt_awake.
    group: u32,
}

impl Edge {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        from_node: usize,
        to_node: usize,
        period: u64,
        slots: Vec<Item>,
        has_mouth: bool,
        count: usize,
    ) -> Edge {
        Edge {
            name,
            from_node,
            to_node,
            period,
            slots,
            mouth: None,
            has_mouth,
            count,
            last_progress: 0,
            entered: count as u64,
            delivered: 0,
            asleep: false,
            group: 0,
        }
    }

    pub fn cap(&self) -> usize {
        self.slots.len() + usize::from(self.has_mouth)
    }
}

/// Runtime reconfiguration and input actions. Applied either at scripted
/// ticks (netlist `at` statements) or immediately (interactive mode).
#[derive(Clone, Debug)]
pub enum Stimulus {
    SetRules {
        node: usize,
        port: usize,
        rules: Vec<Rule>,
    },
    /// Full new priority ordering for a PMerger's input edge ids.
    SetPriority { node: usize, new_ins: Vec<usize> },
    GateOpen { node: usize },
    GateClose { node: usize },
    Press { node: usize, n: u64 },
}

#[derive(Clone, Debug)]
pub enum Display {
    /// Occupancy and level classification of one edge.
    Lamp { name: String, edge: usize },
    /// One edge per bit, MSB first. Decode per bit by saturating item type:
    /// saturated `one` item = 1, saturated `zero` item = 0, empty = N,
    /// anything else = ? (unsettled or invalid).
    Register {
        name: String,
        bits: Vec<usize>,
        one: Item,
        zero: Item,
    },
    /// Sink consumption counter, one item type or total.
    Counter {
        name: String,
        node: usize,
        item: Option<Item>,
    },
    /// Edge throughput meter: occupancy plus lifetime in/out and average
    /// delivery rate. total = items delivered off the head.
    Meter { name: String, edge: usize },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EdgeLevel {
    Null,
    Saturated(Item),
    Partial,
    FullMixed,
}

pub struct World {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub item_names: Vec<String>, // id = index + 1 (0 is EMPTY)
    pub tick: u64,
    pub last_change: u64,
    pub sink_counts: Vec<BTreeMap<Item, u64>>,
    // Dirty-driven junction scheduling.
    dirty: Vec<bool>,
    queue: Vec<usize>,
    // Precomputed cadence groups.
    edges_by_period: Vec<(u64, Vec<usize>)>,
    /// Count of awake edges per cadence group. A group with zero awake
    /// edges contributes no events and is skipped entirely.
    belt_awake: Vec<usize>,
    sources_by_period: Vec<(u64, Vec<usize>)>,
    /// Disable tick skipping (naive stepping). For differential tests.
    pub no_skip: bool,
    /// Processing-order permutation seed. Junction and belt ordering within
    /// each phase follows this permutation instead of declaration order.
    /// Deterministic per seed; state semantics unchanged except for
    /// declaration-order tie-breaks. A circuit whose settled results change
    /// under shuffle has a hidden order race.
    shuffle_seed: Option<u64>,
    node_rank: Vec<u64>,
    // Scripted stimuli, sorted stably by tick (declaration order within a
    // tick). Applied in the STIMULI phase.
    pub stimuli: Vec<(u64, Stimulus)>,
    stim_cursor: usize,
    pub displays: Vec<Display>,
    pub probes: Vec<usize>, // edge ids
    pub probe_log: Vec<(u64, Vec<(usize, u16, Item)>)>, // (tick, [(edge, count, head)])
    pub probe_every: u64,   // 0 = probes disabled
}

impl World {
    pub fn item_id(&self, name: &str) -> Option<Item> {
        self.item_names
            .iter()
            .position(|n| n == name)
            .map(|i| (i + 1) as Item)
    }

    pub fn item_name(&self, id: Item) -> &str {
        if id == EMPTY {
            "EMPTY"
        } else {
            &self.item_names[(id - 1) as usize]
        }
    }

    pub fn node_id(&self, name: &str) -> Option<usize> {
        self.nodes.iter().position(|n| n.name == name)
    }

    pub fn edge_id(&self, name: &str) -> Option<usize> {
        self.edges.iter().position(|e| e.name == name)
    }

    pub fn edge_count(&self, name: &str) -> usize {
        self.edges[self.edge_id(name).expect("no such edge")].count
    }

    pub fn sink_count(&self, node: &str, item: &str) -> u64 {
        let nid = self.node_id(node).expect("no such node");
        let iid = self.item_id(item).expect("no such item");
        *self.sink_counts[nid].get(&iid).unwrap_or(&0)
    }

    /// Total items currently held in a node's port buffers.
    pub fn node_buffered(&self, name: &str) -> usize {
        let n = &self.nodes[self.node_id(name).expect("no such node")];
        match &n.kind {
            NodeKind::Splitter { out_bufs, .. }
            | NodeKind::SmartSplitter { out_bufs, .. } => {
                out_bufs.iter().filter(|&&b| b != EMPTY).count()
            }
            NodeKind::Merger { buf, .. }
            | NodeKind::PMerger { buf }
            | NodeKind::Gate { buf, .. } => usize::from(*buf != EMPTY),
            NodeKind::Container { fifo, .. } => fifo.len(),
            _ => 0,
        }
    }

    pub fn edge_level(&self, e: usize) -> EdgeLevel {
        let edge = &self.edges[e];
        if edge.count == 0 {
            return EdgeLevel::Null;
        }
        if edge.count == edge.cap() {
            let t0 = if edge.slots[0] != EMPTY {
                edge.slots[0]
            } else {
                edge.mouth.unwrap_or(EMPTY)
            };
            let uniform = edge.slots.iter().all(|&x| x == t0)
                && edge.mouth.map_or(true, |m| m == t0);
            if uniform {
                return EdgeLevel::Saturated(t0);
            }
            return EdgeLevel::FullMixed;
        }
        EdgeLevel::Partial
    }

    fn splitmix64(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
        x ^ (x >> 31)
    }

    /// Set (or clear) the processing-order shuffle and rebuild orderings.
    /// Call before running; safe on a freshly parsed world.
    pub fn set_shuffle(&mut self, seed: Option<u64>) {
        self.shuffle_seed = seed;
        self.rebuild_ranks();
    }

    fn rebuild_ranks(&mut self) {
        self.node_rank = (0..self.nodes.len() as u64).collect();
        let mut edge_rank: Vec<u64> = (0..self.edges.len() as u64).collect();
        if let Some(seed) = self.shuffle_seed {
            for (i, r) in self.node_rank.iter_mut().enumerate() {
                *r = Self::splitmix64(seed ^ (i as u64).wrapping_mul(0x9E37));
            }
            for (i, r) in edge_rank.iter_mut().enumerate() {
                *r = Self::splitmix64(seed ^ 0xE1 ^ (i as u64).wrapping_mul(0x85EB));
            }
        }
        for (_, ids) in self.edges_by_period.iter_mut() {
            ids.sort_by_key(|&e| (edge_rank[e], e));
        }
        for (_, ids) in self.sources_by_period.iter_mut() {
            ids.sort_by_key(|&n| (self.node_rank[n], n));
        }
    }

    pub fn finalize(&mut self) {
        // Build cadence groups, sort stimuli, seed the dirty set. Called
        // once after construction by the netlist builder.
        let mut ebp: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for (i, e) in self.edges.iter().enumerate() {
            ebp.entry(e.period).or_default().push(i);
        }
        self.edges_by_period = ebp.into_iter().collect();
        for (gi, (_, ids)) in self.edges_by_period.iter().enumerate() {
            for &e in ids {
                self.edges[e].group = gi as u32;
                self.edges[e].asleep = false;
            }
        }
        self.belt_awake = self.edges_by_period.iter().map(|(_, ids)| ids.len()).collect();

        let mut sbp: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
        for (i, n) in self.nodes.iter().enumerate() {
            if let NodeKind::Source { period, .. } = n.kind {
                sbp.entry(period).or_default().push(i);
            }
        }
        self.sources_by_period = sbp.into_iter().collect();

        self.rebuild_ranks();

        // Stable sort keeps declaration order within a tick.
        self.stimuli.sort_by_key(|(t, _)| *t);
        self.stim_cursor = 0;

        self.dirty = vec![false; self.nodes.len()];
        self.queue.clear();
        for i in 0..self.nodes.len() {
            self.mark(i);
        }
        self.sink_counts = vec![BTreeMap::new(); self.nodes.len()];
    }

    #[inline]
    fn mark(&mut self, nid: usize) {
        if !self.dirty[nid] {
            self.dirty[nid] = true;
            self.queue.push(nid);
        }
    }

    #[inline]
    fn sleep_edge(&mut self, e: usize) {
        let edge = &mut self.edges[e];
        if !edge.asleep {
            edge.asleep = true;
            self.belt_awake[edge.group as usize] -= 1;
        }
    }

    #[inline]
    fn wake_edge(&mut self, e: usize) {
        let edge = &mut self.edges[e];
        if edge.asleep {
            edge.asleep = false;
            self.belt_awake[edge.group as usize] += 1;
        }
    }

    #[inline]
    fn head(&self, e: usize) -> Item {
        self.edges[e].slots[0]
    }

    /// Whether the upstream junction can push into this edge right now.
    /// For a lift with a mouth, entry goes through the mouth.
    #[inline]
    fn tail_free(&self, e: usize) -> bool {
        let edge = &self.edges[e];
        if edge.has_mouth {
            edge.mouth.is_none()
        } else {
            *edge.slots.last().unwrap() == EMPTY
        }
    }

    fn take_head(&mut self, e: usize) -> Item {
        self.wake_edge(e);
        let t = self.tick;
        let edge = &mut self.edges[e];
        let it = edge.slots[0];
        debug_assert!(it != EMPTY);
        edge.slots[0] = EMPTY;
        edge.count -= 1;
        edge.delivered += 1;
        edge.last_progress = t;
        self.last_change = t;
        if edge.slots.len() == 1 && !edge.has_mouth {
            // Head is also the tail: upstream node can now push.
            let from = edge.from_node;
            self.mark(from);
        }
        it
    }

    fn place_tail(&mut self, e: usize, it: Item) {
        self.wake_edge(e);
        let t = self.tick;
        let edge = &mut self.edges[e];
        if edge.has_mouth {
            debug_assert!(edge.mouth.is_none());
            edge.mouth = Some(it);
            edge.count += 1;
            edge.entered += 1;
            edge.last_progress = t;
            self.last_change = t;
            // The mouth is upstream of every slot: no head change here.
            return;
        }
        let last = edge.slots.len() - 1;
        debug_assert!(edge.slots[last] == EMPTY);
        edge.slots[last] = it;
        edge.count += 1;
        edge.entered += 1;
        edge.last_progress = t;
        self.last_change = t;
        if last == 0 {
            // Tail is also the head: downstream node has work.
            let to = edge.to_node;
            self.mark(to);
        }
    }

    /// One slot advance for an edge. Head stays put (waits for a pull).
    /// Everything else moves one slot downstream if the slot ahead is empty.
    /// Lift mouths transfer into the tail slot after the shift.
    fn advance(&mut self, e: usize) {
        let (moved_head, freed_tail);
        {
            let edge = &self.edges[e];
            if edge.count == 0 {
                self.sleep_edge(e);
                return;
            }
            if edge.count == edge.cap() && edge.slots[0] != EMPTY {
                // Fully saturated with an occupied head: nothing can move.
                // A settled level costs zero work. This mirrors the design
                // philosophy: parked state is free.
                self.sleep_edge(e);
                return;
            }
        }
        {
            let edge = &mut self.edges[e];
            let l = edge.slots.len();
            let mut mh = false;
            let mut ft = false;
            let mut any = false;
            for i in 0..l - 1 {
                if edge.slots[i] == EMPTY && edge.slots[i + 1] != EMPTY {
                    edge.slots[i] = edge.slots[i + 1];
                    edge.slots[i + 1] = EMPTY;
                    any = true;
                    if i == 0 {
                        mh = true;
                    }
                    if i + 1 == l - 1 && !edge.has_mouth {
                        ft = true;
                    }
                }
            }
            // Mouth to tail slot transfer.
            if edge.has_mouth {
                if let Some(m) = edge.mouth {
                    if edge.slots[l - 1] == EMPTY {
                        edge.slots[l - 1] = m;
                        edge.mouth = None;
                        any = true;
                        ft = true;
                        if l == 1 {
                            mh = true;
                        }
                    }
                }
            }
            if any {
                edge.last_progress = self.tick;
                self.last_change = self.tick;
            } else {
                // Compacted and nothing arriving: shape cannot change until
                // an external place or take. Sleep.
                self.sleep_edge(e);
                return;
            }
            moved_head = mh;
            freed_tail = ft;
        }
        if moved_head {
            let to = self.edges[e].to_node;
            self.mark(to);
        }
        if freed_tail {
            let from = self.edges[e].from_node;
            self.mark(from);
        }
    }

    /// STIMULI phase: apply all scripted stimuli due at this tick.
    fn stimulus_phase(&mut self) {
        let t = self.tick;
        while self.stim_cursor < self.stimuli.len() && self.stimuli[self.stim_cursor].0 <= t
        {
            let s = self.stimuli[self.stim_cursor].1.clone();
            self.stim_cursor += 1;
            self.apply_stimulus(&s);
        }
    }

    /// Apply one stimulus immediately (used by both the scripted phase and
    /// the interactive REPL). Marks the node dirty so a cleared jam or a
    /// new press is serviced on the next junction phase.
    pub fn apply_stimulus(&mut self, s: &Stimulus) {
        let t = self.tick;
        match s {
            Stimulus::SetRules { node, port, rules } => {
                if let NodeKind::SmartSplitter { rules: r, .. } = &mut self.nodes[*node].kind
                {
                    r[*port] = rules.clone();
                }
                self.mark(*node);
            }
            Stimulus::SetPriority { node, new_ins } => {
                self.nodes[*node].ins = new_ins.clone();
                self.mark(*node);
            }
            Stimulus::GateOpen { node } => {
                if let NodeKind::Gate { open, .. } = &mut self.nodes[*node].kind {
                    *open = true;
                }
                self.mark(*node);
            }
            Stimulus::GateClose { node } => {
                if let NodeKind::Gate { open, .. } = &mut self.nodes[*node].kind {
                    *open = false;
                }
                self.mark(*node);
            }
            Stimulus::Press { node, n } => {
                if let NodeKind::Button { queued, .. } = &mut self.nodes[*node].kind {
                    *queued += n;
                }
                self.mark(*node);
            }
        }
        self.last_change = t;
    }

    /// SOURCES phase: on each source's cadence, arm a pending item.
    fn source_phase(&mut self) {
        let t = self.tick;
        // Cheap: at most a handful of distinct periods.
        for gi in 0..self.sources_by_period.len() {
            let (p, _) = self.sources_by_period[gi];
            if t % p != 0 {
                continue;
            }
            let ids = std::mem::take(&mut self.sources_by_period[gi].1);
            for &nid in &ids {
                let arm = if let NodeKind::Source {
                    start_tick,
                    stop_tick,
                    limit,
                    emitted,
                    pending,
                    ..
                } = &mut self.nodes[nid].kind
                {
                    let in_window = t >= *start_tick && t < *stop_tick;
                    let under_limit = limit.map_or(true, |l| *emitted < l);
                    if in_window && under_limit && !*pending {
                        *pending = true;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if arm {
                    self.mark(nid);
                }
            }
            self.sources_by_period[gi].1 = ids;
        }
    }

    /// JUNCTIONS phase: process this tick's dirty set in id order.
    /// Nodes dirtied during processing run on the next tick.
    fn junction_phase(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        let mut batch = std::mem::take(&mut self.queue);
        batch.sort_unstable_by_key(|&n| (self.node_rank[n], n));
        for nid in batch {
            self.dirty[nid] = false;
            self.node_turn(nid);
        }
    }

    fn node_turn(&mut self, nid: usize) {
        // Take the kind out to sidestep aliasing; restore at the end.
        let mut kind = std::mem::replace(&mut self.nodes[nid].kind, NodeKind::Sink);
        let (ins_buf, ins_n) = {
            let v = &self.nodes[nid].ins;
            let mut a = [0usize; 3];
            a[..v.len()].copy_from_slice(v);
            (a, v.len())
        };
        let ins = &ins_buf[..ins_n];
        let (outs_buf, outs_n) = {
            let v = &self.nodes[nid].outs;
            let mut a = [0usize; 3];
            a[..v.len()].copy_from_slice(v);
            (a, v.len())
        };
        let outs = &outs_buf[..outs_n];

        match &mut kind {
            NodeKind::Sink => {
                if let Some(&e) = ins.first() {
                    if self.head(e) != EMPTY {
                        let it = self.take_head(e);
                        *self.sink_counts[nid].entry(it).or_insert(0) += 1;
                    }
                }
            }
            NodeKind::Source {
                item,
                start_tick,
                stop_tick,
                pending,
                emitted,
                ..
            } => {
                if *pending {
                    let t = self.tick;
                    if t < *start_tick || t >= *stop_tick {
                        // The armed item was never manufactured: a stopped
                        // source must not emit into a belt that unblocks
                        // later. Control dead means dead.
                        *pending = false;
                    } else {
                        let e = outs[0];
                        if self.tail_free(e) {
                            self.place_tail(e, *item);
                            *pending = false;
                            *emitted += 1;
                        }
                    }
                }
            }
            NodeKind::Button { item, queued } => {
                if *queued > 0 {
                    let e = outs[0];
                    if self.tail_free(e) {
                        self.place_tail(e, *item);
                        *queued -= 1;
                        if *queued > 0 {
                            // More to place: come back when the tail frees.
                            // The tail-free advance will re-mark us, but a
                            // 1-slot edge with a fast consumer might drain
                            // between our turns; self-mark is safe (no-op if
                            // still blocked).
                            self.mark(nid);
                        }
                    }
                }
            }
            NodeKind::Gate { open, buf } => {
                // Drain buffer regardless of gate state (break-before-make:
                // an item already admitted completes its transit).
                if *buf != EMPTY {
                    let e = outs[0];
                    if self.tail_free(e) {
                        self.place_tail(e, *buf);
                        *buf = EMPTY;
                    }
                }
                if *open && *buf == EMPTY {
                    let e_in = ins[0];
                    if self.head(e_in) != EMPTY {
                        let it = self.take_head(e_in);
                        *buf = it;
                        let e = outs[0];
                        if self.tail_free(e) {
                            self.place_tail(e, *buf);
                            *buf = EMPTY;
                        }
                    }
                }
            }
            NodeKind::Splitter { rr, out_bufs } => {
                // 1) Drain every occupied port buffer whose belt tail is free.
                for (p, &e) in outs.iter().enumerate() {
                    if out_bufs[p] != EMPTY && self.tail_free(e) {
                        let it = out_bufs[p];
                        out_bufs[p] = EMPTY;
                        self.place_tail(e, it);
                    }
                }
                // 2) Pull at most one item into the next free port buffer,
                //    round-robin, skipping occupied buffers (skip-blocked).
                if let Some(&e_in) = ins.first() {
                    if self.head(e_in) != EMPTY {
                        let n = outs.len();
                        for k in 0..n {
                            let p = (*rr + k) % n;
                            if out_bufs[p] == EMPTY {
                                let it = self.take_head(e_in);
                                out_bufs[p] = it;
                                *rr = (p + 1) % n;
                                // Immediate drain attempt.
                                let e = outs[p];
                                if self.tail_free(e) {
                                    let it2 = out_bufs[p];
                                    out_bufs[p] = EMPTY;
                                    self.place_tail(e, it2);
                                }
                                break;
                            }
                        }
                    }
                }
            }
            NodeKind::SmartSplitter { rules, rr, out_bufs } => {
                // 1) Drain buffers.
                for (p, &e) in outs.iter().enumerate() {
                    if out_bufs[p] != EMPTY && self.tail_free(e) {
                        let it = out_bufs[p];
                        out_bufs[p] = EMPTY;
                        self.place_tail(e, it);
                    }
                }
                // 2) Prechecked pull. Refusal = poison jam: head stays, and
                //    everything behind it on the input belt stalls, all types.
                if let Some(&e_in) = ins.first() {
                    let h = self.head(e_in);
                    if h != EMPTY {
                        let n = outs.len();
                        // Does ANY port carry an exact filter for this item?
                        let exact_exists = (0..n)
                            .any(|p| rules[p].iter().any(|r| *r == Rule::Item(h)));
                        // Eligible set: exact matches plus Any ports, plus
                        // Undefined ports when no exact filter exists.
                        let mut eligible = [false; 3];
                        let mut any_eligible = false;
                        let mut overflow_ports = [false; 3];
                        for p in 0..n {
                            for r in &rules[p] {
                                match *r {
                                    Rule::Item(x) if x == h => {
                                        eligible[p] = true;
                                    }
                                    Rule::Any => {
                                        eligible[p] = true;
                                    }
                                    Rule::Undefined if !exact_exists => {
                                        eligible[p] = true;
                                    }
                                    Rule::Overflow => {
                                        overflow_ports[p] = true;
                                    }
                                    _ => {}
                                }
                            }
                            any_eligible |= eligible[p];
                        }
                        // Choose round-robin among eligible ports with a
                        // free buffer.
                        let mut chosen: Option<usize> = None;
                        if any_eligible {
                            for k in 0..n {
                                let p = (*rr + k) % n;
                                if eligible[p] && out_bufs[p] == EMPTY {
                                    chosen = Some(p);
                                    break;
                                }
                            }
                        }
                        // Overflow: only when nothing eligible can accept
                        // (either no eligible ports exist, or all their
                        // buffers are occupied). Round-robin among overflow
                        // ports too.
                        if chosen.is_none() {
                            for k in 0..n {
                                let p = (*rr + k) % n;
                                if overflow_ports[p] && out_bufs[p] == EMPTY {
                                    chosen = Some(p);
                                    break;
                                }
                            }
                        }
                        if let Some(p) = chosen {
                            let it = self.take_head(e_in);
                            out_bufs[p] = it;
                            *rr = (p + 1) % n;
                            let e = outs[p];
                            if self.tail_free(e) {
                                let it2 = out_bufs[p];
                                out_bufs[p] = EMPTY;
                                self.place_tail(e, it2);
                            }
                        }
                        // else: refuse. Jam. Nothing moves. (Poison-pill law.)
                    }
                }
            }
            NodeKind::Merger { rr, buf } => {
                if *buf != EMPTY {
                    let e = outs[0];
                    if self.tail_free(e) {
                        self.place_tail(e, *buf);
                        *buf = EMPTY;
                    }
                }
                if *buf == EMPTY && !ins.is_empty() {
                    let n = ins.len();
                    for k in 0..n {
                        let p = (*rr + k) % n;
                        let e_in = ins[p];
                        if self.head(e_in) != EMPTY {
                            *buf = self.take_head(e_in);
                            *rr = (p + 1) % n;
                            let e = outs[0];
                            if self.tail_free(e) {
                                self.place_tail(e, *buf);
                                *buf = EMPTY;
                            }
                            break;
                        }
                    }
                }
            }
            NodeKind::PMerger { buf } => {
                if *buf != EMPTY {
                    let e = outs[0];
                    if self.tail_free(e) {
                        self.place_tail(e, *buf);
                        *buf = EMPTY;
                    }
                }
                if *buf == EMPTY {
                    // ins is priority-ordered. Strict priority: first
                    // occupied input wins. A backed-up low can never block
                    // high.
                    for &e_in in ins {
                        if self.head(e_in) != EMPTY {
                            *buf = self.take_head(e_in);
                            let e = outs[0];
                            if self.tail_free(e) {
                                self.place_tail(e, *buf);
                                *buf = EMPTY;
                            }
                            break;
                        }
                    }
                }
            }
            NodeKind::Container { cap, fifo } => {
                if let Some(&e) = outs.first() {
                    if !fifo.is_empty() && self.tail_free(e) {
                        let it = fifo.pop_front().unwrap();
                        self.place_tail(e, it);
                    }
                }
                if let Some(&e_in) = ins.first() {
                    if fifo.len() < *cap && self.head(e_in) != EMPTY {
                        let it = self.take_head(e_in);
                        fifo.push_back(it);
                    }
                }
            }
        }

        self.nodes[nid].kind = kind;
    }

    /// BELTS phase: advance every edge whose cadence is due this tick.
    fn belt_phase(&mut self) {
        let t = self.tick;
        for gi in 0..self.edges_by_period.len() {
            let p = self.edges_by_period[gi].0;
            if t % p != 0 || self.belt_awake[gi] == 0 {
                continue;
            }
            // ids are already in ascending order (built that way).
            let ids = std::mem::take(&mut self.edges_by_period[gi].1);
            for &e in &ids {
                if !self.edges[e].asleep {
                    self.advance(e);
                }
            }
            self.edges_by_period[gi].1 = ids;
        }
    }

    pub fn step(&mut self) {
        self.stimulus_phase();
        self.source_phase();
        self.junction_phase();
        self.belt_phase();
        self.record_probes();
        self.tick += 1;
    }

    /// The earliest tick >= self.tick at which anything could possibly
    /// happen, given an empty junction queue: a scripted stimulus, a probe
    /// sample, a source arming on its cadence, or an awake belt group's
    /// cadence. Every tick strictly before it is provably a no-op, so the
    /// tick counter may jump there directly without simulating.
    fn next_event_tick(&self, limit: u64) -> u64 {
        let t = self.tick;
        let mut best = limit;
        if self.stim_cursor < self.stimuli.len() {
            best = best.min(self.stimuli[self.stim_cursor].0.max(t));
        }
        if self.probe_every > 0 && !self.probes.is_empty() {
            let pe = self.probe_every;
            best = best.min(t + (pe - t % pe) % pe);
        }
        for (nid, n) in self.nodes.iter().enumerate() {
            let _ = nid;
            if let NodeKind::Source {
                period,
                start_tick,
                stop_tick,
                limit: lim,
                emitted,
                pending,
                ..
            } = &n.kind
            {
                if *pending || lim.map_or(false, |l| *emitted >= l) {
                    continue;
                }
                let base = t.max(*start_tick);
                if base >= *stop_tick {
                    continue;
                }
                let p = *period;
                let arm = base + (p - base % p) % p;
                if arm < *stop_tick {
                    best = best.min(arm);
                }
            }
        }
        for (gi, (p, ids)) in self.edges_by_period.iter().enumerate() {
            if self.belt_awake[gi] == 0 || ids.is_empty() {
                continue;
            }
            best = best.min(t + (p - t % p) % p);
        }
        best
    }

    /// Advance simulated time to end_tick, stepping only ticks where work
    /// can happen and jumping over provably idle spans. Optionally stop
    /// once quiet_ticks pass without a state change. Exactness: the state
    /// at every simulated tick equals naive stepping (differentially
    /// tested); skipped ticks are no-ops by construction.
    fn run_span(&mut self, end_tick: u64, quiet_ticks: Option<u64>) {
        while self.tick < end_tick {
            if let Some(q) = quiet_ticks {
                if self.tick.saturating_sub(self.last_change) > q {
                    break;
                }
            }
            if !self.no_skip && self.queue.is_empty() {
                let mut target = self.next_event_tick(end_tick);
                if let Some(q) = quiet_ticks {
                    target = target.min(self.last_change + q + 1);
                }
                if target > self.tick {
                    self.tick = target;
                    continue;
                }
            }
            self.step();
        }
    }

    pub fn run_ticks(&mut self, n: u64) {
        let end = self.tick + n;
        self.run_span(end, None);
    }

    /// Run for at most `max_ticks`; stop early if no state change happened
    /// for `quiet_ticks`. Returns ticks actually simulated. This is the
    /// completion-detection primitive: step until the machine settles.
    pub fn run_until_quiet(&mut self, max_ticks: u64, quiet_ticks: u64) -> u64 {
        let start = self.tick;
        self.run_span(start + max_ticks, Some(quiet_ticks));
        self.tick - start
    }

    fn record_probes(&mut self) {
        if self.probes.is_empty() || self.probe_every == 0 {
            return;
        }
        if self.tick % self.probe_every != 0 {
            return;
        }
        let snap: Vec<(usize, u16, Item)> = self
            .probes
            .iter()
            .map(|&e| (e, self.edges[e].count as u16, self.edges[e].slots[0]))
            .collect();
        self.probe_log.push((self.tick, snap));
    }

    /// FNV-1a over the full mutable state. Used by the determinism test.
    pub fn state_hash(&self) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        let mix = |v: u64, h: &mut u64| {
            *h ^= v;
            *h = h.wrapping_mul(0x100000001b3);
        };
        mix(self.tick, &mut h);
        for e in &self.edges {
            for &s in &e.slots {
                mix(s as u64 + 1, &mut h);
            }
            mix(e.mouth.map_or(0, |m| m as u64 + 1), &mut h);
        }
        for n in &self.nodes {
            for &i in &n.ins {
                mix(i as u64 + 7, &mut h);
            }
            match &n.kind {
                NodeKind::Splitter { rr, out_bufs } => {
                    mix(*rr as u64, &mut h);
                    for &b in out_bufs {
                        mix(b as u64 + 1, &mut h);
                    }
                }
                NodeKind::SmartSplitter { rr, out_bufs, rules } => {
                    mix(*rr as u64, &mut h);
                    for &b in out_bufs {
                        mix(b as u64 + 1, &mut h);
                    }
                    for port in rules {
                        for r in port {
                            let tag = match r {
                                Rule::Item(x) => 10 + *x as u64,
                                Rule::Any => 2,
                                Rule::Undefined => 3,
                                Rule::Overflow => 4,
                            };
                            mix(tag, &mut h);
                        }
                        mix(5, &mut h);
                    }
                }
                NodeKind::Merger { rr, buf } => {
                    mix(*rr as u64, &mut h);
                    mix(*buf as u64 + 1, &mut h);
                }
                NodeKind::PMerger { buf } => {
                    mix(*buf as u64 + 1, &mut h);
                }
                NodeKind::Gate { open, buf } => {
                    mix(*open as u64, &mut h);
                    mix(*buf as u64 + 1, &mut h);
                }
                NodeKind::Container { fifo, .. } => {
                    for &it in fifo {
                        mix(it as u64 + 1, &mut h);
                    }
                }
                NodeKind::Source { emitted, pending, .. } => {
                    mix(*emitted, &mut h);
                    mix(*pending as u64, &mut h);
                }
                NodeKind::Button { queued, .. } => {
                    mix(*queued, &mut h);
                }
                NodeKind::Sink => {}
            }
        }
        for m in &self.sink_counts {
            for (&k, &v) in m {
                mix(k as u64, &mut h);
                mix(v, &mut h);
            }
        }
        h
    }

    /// Render the declared displays as a panel string.
    pub fn render_displays(&self) -> String {
        let mut s = String::new();
        if self.displays.is_empty() {
            return s;
        }
        s.push_str(&format!(
            "displays @ {:.3} s\n",
            self.tick as f64 / TICK_RATE as f64
        ));
        for d in &self.displays {
            match d {
                Display::Lamp { name, edge } => {
                    let e = &self.edges[*edge];
                    let lv = match self.edge_level(*edge) {
                        EdgeLevel::Null => "NULL".to_string(),
                        EdgeLevel::Saturated(t) => {
                            format!("LEVEL {}", self.item_name(t))
                        }
                        EdgeLevel::Partial => format!("partial {}/{}", e.count, e.cap()),
                        EdgeLevel::FullMixed => "FULL mixed".to_string(),
                    };
                    s.push_str(&format!("  {:<10} [lamp {:<10}] {}\n", name, e.name, lv));
                }
                Display::Register { name, bits, one, zero } => {
                    let mut chars = String::new();
                    let mut value: u64 = 0;
                    let mut settled = true;
                    for &b in bits {
                        let c = match self.edge_level(b) {
                            EdgeLevel::Saturated(t) if t == *one => '1',
                            EdgeLevel::Saturated(t) if t == *zero => '0',
                            EdgeLevel::Null => 'N',
                            _ => '?',
                        };
                        chars.push(c);
                        value <<= 1;
                        if c == '1' {
                            value |= 1;
                        } else if c != '0' {
                            settled = false;
                        }
                    }
                    if settled {
                        s.push_str(&format!(
                            "  {:<10} [register] {} = 0x{:0width$X} = {}\n",
                            name,
                            chars,
                            value,
                            value,
                            width = (bits.len() + 3) / 4
                        ));
                    } else {
                        s.push_str(&format!(
                            "  {:<10} [register] {} (not settled)\n",
                            name, chars
                        ));
                    }
                }
                Display::Meter { name, edge } => {
                    let e = &self.edges[*edge];
                    let secs = self.tick as f64 / TICK_RATE as f64;
                    let rate = if secs > 0.0 { e.delivered as f64 / secs } else { 0.0 };
                    s.push_str(&format!(
                        "  {:<10} [meter {:<10}] {}/{}, in={}, total={}, {:.2}/s avg\n",
                        name, e.name, e.count, e.cap(), e.entered, e.delivered, rate
                    ));
                }
                Display::Counter { name, node, item } => {
                    let counts = &self.sink_counts[*node];
                    let v = match item {
                        Some(i) => *counts.get(i).unwrap_or(&0),
                        None => counts.values().sum(),
                    };
                    s.push_str(&format!(
                        "  {:<10} [counter {:<10}] {}\n",
                        name, self.nodes[*node].name, v
                    ));
                }
            }
        }
        s
    }

    /// Human report of final state.
    pub fn report(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "tick {} ({:.3} sim-seconds)\n",
            self.tick,
            self.tick as f64 / TICK_RATE as f64
        ));
        let panel = self.render_displays();
        if !panel.is_empty() {
            s.push_str(&panel);
        }
        s.push_str("sinks:\n");
        for (nid, n) in self.nodes.iter().enumerate() {
            if matches!(n.kind, NodeKind::Sink) {
                let counts = &self.sink_counts[nid];
                if counts.is_empty() {
                    s.push_str(&format!("  {:<12} (nothing)\n", n.name));
                } else {
                    let parts: Vec<String> = counts
                        .iter()
                        .map(|(&k, &v)| format!("{}={}", self.item_name(k), v))
                        .collect();
                    s.push_str(&format!("  {:<12} {}\n", n.name, parts.join(" ")));
                }
            }
        }
        s.push_str("edges:\n");
        for (ei, e) in self.edges.iter().enumerate() {
            let level = match self.edge_level(ei) {
                EdgeLevel::Null => "NULL".to_string(),
                EdgeLevel::Saturated(t) => {
                    format!("LEVEL {} (saturated)", self.item_name(t))
                }
                EdgeLevel::Partial => "partial".to_string(),
                EdgeLevel::FullMixed => "FULL mixed".to_string(),
            };
            let stale = e.count == e.cap()
                && self.tick.saturating_sub(e.last_progress) > 4 * e.period;
            let lift = if e.has_mouth {
                match e.mouth {
                    Some(m) => format!("  (lift, mouth: {})", self.item_name(m)),
                    None => "  (lift, mouth empty)".to_string(),
                }
            } else {
                String::new()
            };
            s.push_str(&format!(
                "  {:<12} {:>3}/{:<3} {}{}{}\n",
                e.name,
                e.count,
                e.cap(),
                level,
                if stale { "  [JAMMED]" } else { "" },
                lift
            ));
        }
        let mut buffered = Vec::new();
        for n in &self.nodes {
            let b = match &n.kind {
                NodeKind::Splitter { out_bufs, .. }
                | NodeKind::SmartSplitter { out_bufs, .. } => {
                    out_bufs.iter().filter(|&&b| b != EMPTY).count()
                }
                NodeKind::Merger { buf, .. }
                | NodeKind::PMerger { buf }
                | NodeKind::Gate { buf, .. } => usize::from(*buf != EMPTY),
                NodeKind::Container { fifo, .. } => fifo.len(),
                NodeKind::Button { queued, .. } => *queued as usize,
                _ => 0,
            };
            if b > 0 {
                buffered.push(format!("  {:<12} {} item(s)", n.name, b));
            }
        }
        if !buffered.is_empty() {
            s.push_str("node buffers / queues holding items:\n");
            for line in buffered {
                s.push_str(&line);
                s.push('\n');
            }
        }
        s
    }
}

impl Default for World {
    fn default() -> Self {
        World {
            nodes: Vec::new(),
            edges: Vec::new(),
            item_names: Vec::new(),
            tick: 0,
            last_change: 0,
            sink_counts: Vec::new(),
            dirty: Vec::new(),
            queue: Vec::new(),
            edges_by_period: Vec::new(),
            belt_awake: Vec::new(),
            sources_by_period: Vec::new(),
            no_skip: false,
            shuffle_seed: None,
            node_rank: Vec::new(),
            stimuli: Vec::new(),
            stim_cursor: 0,
            displays: Vec::new(),
            probes: Vec::new(),
            probe_log: Vec::new(),
            probe_every: 0,
        }
    }
}
